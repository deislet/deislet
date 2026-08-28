// Runtime isolate implementation
// Uses Deno Workers to give each application an isolated runtime environment

import type {
  AppConfig,
  Permissions,
  WorkerRequest,
  WorkerResponse,
} from "./types.ts";

/**
 * Serialize a Request object into a transferable format
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
 * Deserialize Response data
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
 * RuntimeIsolate class
 * Wraps the Deno Worker instance of a single application
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
   * Initialize the Worker
   */
  async initialize(): Promise<void> {
    // Build the Worker code
    const workerCode = this.buildWorkerCode();

    // Create the Worker
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

    // Listen for Worker messages
    this.worker.onmessage = (event: MessageEvent<WorkerResponse>) => {
      this.handleWorkerMessage(event.data);
    };

    // Listen for Worker errors
    this.worker.onerror = (error) => {
      console.error(`[${this.appId}] Worker error:`, error);
      // Reject all pending requests
      for (const [requestId, pending] of this.pendingRequests.entries()) {
        clearTimeout(pending.timer);
        pending.reject(new Error("Worker crashed"));
        this.pendingRequests.delete(requestId);
      }
    };

    // Wait for the Worker to finish initializing
    await this.ping();
  }

  /**
   * Build the Worker code
   */
  private buildWorkerCode(): string {
    // Inject the application code
    const appCode = this.code;

    // Worker runtime code
    return `
      // Inject the application code into the global scope
      self.__APP_CODE__ = ${JSON.stringify(appCode)};

      // Worker global state
      let appHandler = null;
      let isReady = false;

      // Initialize the application
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

      // Deserialize a Request
      function deserializeRequest(data) {
        const { url, method, headers, body } = data;
        return new Request(url, { method, headers, body });
      }

      // Serialize a Response
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

      // Handle messages
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
   * Build the permission configuration
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
   * Ping the Worker to check readiness
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

          // Send the initialization message
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
   * Execute a request
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
   * Handle a Worker message
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
   * Terminate the Worker
   */
  terminate(): void {
    if (this.worker) {
      this.worker.terminate();
      this.worker = null;
    }

    // Reject all pending requests
    for (const [requestId, pending] of this.pendingRequests.entries()) {
      clearTimeout(pending.timer);
      pending.reject(new Error("Worker terminated"));
      this.pendingRequests.delete(requestId);
    }
  }

  /**
   * Get the application ID
   */
  getAppId(): string {
    return this.appId;
  }
}
