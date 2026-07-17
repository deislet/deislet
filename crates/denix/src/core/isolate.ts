// Runtime Isolate 实现
// 使用 Deno Worker 为每个应用提供独立的运行时环境

import type {
  AppConfig,
  Permissions,
  WorkerRequest,
  WorkerResponse,
} from "./types.ts";

/**
 * 将 Request 对象序列化为可传输的格式
 */
async function serializeRequest(request: Request) {
  const body = request.body ? await request.text() : undefined;
  const headers: Record<string, string> = {};
  request.headers.forEach((value, key) => {
    headers[key] = value;
  });

  return {
    url: request.url,
    method: request.method,
    headers,
    body,
  };
}

/**
 * 反序列化 Response 数据
 */
function deserializeResponse(data: any): Response {
  const { status, statusText, headers, body } = data;
  return new Response(body, {
    status,
    statusText,
    headers,
  });
}

/**
 * RuntimeIsolate 类
 * 封装单个应用的 Deno Worker 实例
 */
export class RuntimeIsolate {
  private worker: Worker | null = null;
  private pendingRequests: Map<
    string,
    { resolve: Function; reject: Function; timer: number }
  > = new Map();
  private requestCounter = 0;

  constructor(
    private appId: string,
    private code: string,
    private config: AppConfig,
  ) {}

  /**
   * 初始化 Worker
   */
  async initialize(): Promise<void> {
    // 构建 Worker 代码
    const workerCode = this.buildWorkerCode();

    // 创建 Worker
    const permissions = this.buildPermissions(this.config.permissions);

    this.worker = new Worker(
      `data:text/javascript,${encodeURIComponent(workerCode)}`,
      {
        type: "module",
        deno: {
          permissions,
        },
      },
    );

    // 监听 Worker 消息
    this.worker.onmessage = (event: MessageEvent<WorkerResponse>) => {
      this.handleWorkerMessage(event.data);
    };

    // 监听 Worker 错误
    this.worker.onerror = (error) => {
      console.error(`[${this.appId}] Worker error:`, error);
      // 拒绝所有待处理的请求
      for (const [requestId, pending] of this.pendingRequests.entries()) {
        clearTimeout(pending.timer);
        pending.reject(new Error("Worker crashed"));
        this.pendingRequests.delete(requestId);
      }
    };

    // 等待 Worker 初始化完成
    await this.ping();
  }

  /**
   * 构建 Worker 代码
   */
  private buildWorkerCode(): string {
    // 注入应用代码
    const appCode = this.code;

    // Worker 运行时代码
    return `
      // 注入应用代码到全局
      self.__APP_CODE__ = ${JSON.stringify(appCode)};

      // Worker 全局状态
      let appHandler = null;
      let isReady = false;

      // 初始化应用
      async function initializeApp() {
        try {
          const appModule = await import("data:text/javascript," + encodeURIComponent(self.__APP_CODE__));
          appHandler = appModule.default;

          if (!appHandler || typeof appHandler.fetch !== "function") {
            throw new Error("App must export a default object with a fetch method");
          }

          isReady = true;
        } catch (error) {
          console.error("Failed to initialize app:", error);
          throw error;
        }
      }

      // 反序列化 Request
      function deserializeRequest(data) {
        const { url, method, headers, body } = data;
        return new Request(url, { method, headers, body });
      }

      // 序列化 Response
      async function serializeResponse(response) {
        const body = await response.text();
        const headers = {};
        response.headers.forEach((value, key) => {
          headers[key] = value;
        });

        return {
          status: response.status,
          statusText: response.statusText,
          headers,
          body,
        };
      }

      // 处理消息
      self.onmessage = async (event) => {
        const message = event.data;

        try {
          if (message.type === "ping") {
            self.postMessage({ type: "pong", requestId: message.requestId });
            return;
          }

          if (message.type === "init") {
            await initializeApp();
            self.postMessage({ type: "pong", requestId: message.requestId });
            return;
          }

          if (message.type === "execute") {
            if (!isReady || !appHandler) {
              throw new Error("App handler not initialized");
            }

            const request = deserializeRequest(message.request);
            const env = message.env || {};

            const response = await appHandler.fetch(request, env);
            const serializedResponse = await serializeResponse(response);

            self.postMessage({
              type: "response",
              requestId: message.requestId,
              response: serializedResponse,
            });
          }
        } catch (error) {
          self.postMessage({
            type: "error",
            requestId: message.requestId,
            error: error instanceof Error ? error.message : String(error),
          });
        }
      };
    `;
  }

  /**
   * 构建权限配置
   */
  private buildPermissions(permissions?: Permissions): Deno.PermissionOptions {
    return {
      net: permissions?.net ?? false,
      read: permissions?.read ?? false,
      write: permissions?.write ?? false,
      env: permissions?.env ?? false,
      run: permissions?.run ?? false,
    };
  }

  /**
   * Ping Worker 检查是否就绪
   */
  private async ping(): Promise<void> {
    const initMessage: WorkerRequest = {
      type: "ping",
      requestId: "init",
    };

    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error("Worker initialization timeout"));
      }, 5000);

      const handler = (event: MessageEvent<WorkerResponse>) => {
        if (event.data.requestId === "init") {
          clearTimeout(timer);
          this.worker?.removeEventListener("message", handler);

          // 发送初始化消息
          const initMsg: WorkerRequest = {
            type: "ping",
            requestId: "init2",
          };
          this.worker?.postMessage(initMsg);

          const handler2 = (event: MessageEvent<WorkerResponse>) => {
            if (event.data.requestId === "init2") {
              this.worker?.removeEventListener("message", handler2);
              resolve();
            }
          };
          this.worker?.addEventListener("message", handler2);
        }
      };

      this.worker?.addEventListener("message", handler);
      this.worker?.postMessage(initMessage);
    });
  }

  /**
   * 执行请求
   */
  async execute(
    request: Request,
    env: Record<string, any> = {},
  ): Promise<Response> {
    if (!this.worker) {
      throw new Error("Worker not initialized");
    }

    const requestId = `req_${++this.requestCounter}`;
    const serializedRequest = await serializeRequest(request);

    const message: WorkerRequest = {
      type: "execute",
      requestId,
      request: serializedRequest,
      env,
    };

    const timeout = this.config.limits?.requestTimeoutMS || 30000;

    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingRequests.delete(requestId);
        reject(new Error("Request timeout"));
      }, timeout);

      this.pendingRequests.set(requestId, { resolve, reject, timer });
      this.worker!.postMessage(message);
    });
  }

  /**
   * 处理 Worker 消息
   */
  private handleWorkerMessage(message: WorkerResponse) {
    const pending = this.pendingRequests.get(message.requestId);
    if (!pending) {
      return;
    }

    clearTimeout(pending.timer);
    this.pendingRequests.delete(message.requestId);

    if (message.type === "response" && message.response) {
      const response = deserializeResponse(message.response);
      pending.resolve(response);
    } else if (message.type === "error") {
      pending.reject(new Error(message.error || "Unknown error"));
    }
  }

  /**
   * 终止 Worker
   */
  terminate(): void {
    if (this.worker) {
      this.worker.terminate();
      this.worker = null;
    }

    // 拒绝所有待处理的请求
    for (const [requestId, pending] of this.pendingRequests.entries()) {
      clearTimeout(pending.timer);
      pending.reject(new Error("Worker terminated"));
      this.pendingRequests.delete(requestId);
    }
  }

  /**
   * 获取应用 ID
   */
  getAppId(): string {
    return this.appId;
  }
}
