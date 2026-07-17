// Worker 运行时代码
// 这段代码运行在独立的 Worker 线程中

import type { WorkerRequest, WorkerResponse } from "../core/types.ts";

// Worker 全局状态
let appHandler: any = null;
let isReady = false;

/**
 * 序列化 Request 对象为可传输的格式
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
 * 反序列化 Request 对象
 */
function deserializeRequest(data: any): Request {
  const { url, method, headers, body } = data;
  return new Request(url, {
    method,
    headers,
    body,
  });
}

/**
 * 序列化 Response 对象为可传输的格式
 */
async function serializeResponse(response: Response) {
  const body = await response.text();
  const headers: Record<string, string> = {};
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

/**
 * 处理来自主线程的消息
 */
self.onmessage = async (event: MessageEvent<WorkerRequest>) => {
  const message = event.data;

  try {
    if (message.type === "ping") {
      // 健康检查
      const response: WorkerResponse = {
        type: "pong",
        requestId: message.requestId,
      };
      self.postMessage(response);
      return;
    }

    if (message.type === "execute") {
      // 执行应用代码
      if (!isReady || !appHandler) {
        throw new Error("App handler not initialized");
      }

      if (!message.request) {
        throw new Error("Request data is missing");
      }

      // 反序列化请求
      const request = deserializeRequest(message.request);

      // 准备环境对象
      const env = message.env || {};

      // 执行应用的 fetch 函数
      const response = await appHandler.fetch(request, env);

      // 序列化响应
      const serializedResponse = await serializeResponse(response);

      const responseMessage: WorkerResponse = {
        type: "response",
        requestId: message.requestId,
        response: serializedResponse,
      };

      self.postMessage(responseMessage);
    }
  } catch (error) {
    // 发送错误响应
    const errorMessage: WorkerResponse = {
      type: "error",
      requestId: message.requestId,
      error: error instanceof Error ? error.message : String(error),
    };
    self.postMessage(errorMessage);
  }
};

/**
 * 初始化应用
 * 这个函数会在 Worker 启动时立即执行
 */
async function initializeApp() {
  try {
    // 应用代码会通过特殊的方式注入
    // 在实际运行时，__APP_CODE__ 会被替换为真实的应用代码
    const appModule = await import("data:text/javascript," + encodeURIComponent(
      // @ts-ignore - __APP_CODE__ 会在运行时注入
      self.__APP_CODE__,
    ));

    appHandler = appModule.default;

    if (!appHandler || typeof appHandler.fetch !== "function") {
      throw new Error(
        "App must export a default object with a fetch method",
      );
    }

    isReady = true;
  } catch (error) {
    console.error("Failed to initialize app:", error);
    isReady = false;
  }
}

// 暴露全局初始化函数
// @ts-ignore
self.initializeApp = initializeApp;
