// Worker runtime code
// This code runs in a dedicated Worker thread

import type { WorkerRequest, WorkerResponse } from "../core/types.ts";

// Worker global state
let appHandler: any = null;
let isReady = false;

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
 * Deserialize a Request object
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
 * Serialize a Response object into a transferable format
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
 * Handle messages from the main thread
 */
self.onmessage = async (event: MessageEvent<WorkerRequest>) => {
  const message = event.data;

  try {
    if (message.type === "ping") {
      // Health check
      const response: WorkerResponse = {
        type: "pong",
        requestId: message.requestId,
      };
      self.postMessage(response);
      return;
    }

    if (message.type === "execute") {
      // Execute the application code
      if (!isReady || !appHandler) {
        throw new Error("App handler not initialized");
      }

      if (!message.request) {
        throw new Error("Request data is missing");
      }

      // Deserialize the request
      const request = deserializeRequest(message.request);

      // Prepare the environment object
      const env = message.env || {};

      // Invoke the app's fetch function
      const response = await appHandler.fetch(request, env);

      // Serialize the response
      const serializedResponse = await serializeResponse(response);

      const responseMessage: WorkerResponse = {
        type: "response",
        requestId: message.requestId,
        response: serializedResponse,
      };

      self.postMessage(responseMessage);
    }
  } catch (error) {
    // Send an error response
    const errorMessage: WorkerResponse = {
      type: "error",
      requestId: message.requestId,
      error: error instanceof Error ? error.message : String(error),
    };
    self.postMessage(errorMessage);
  }
};

/**
 * Initialize the application
 * This function runs immediately when the Worker starts
 */
async function initializeApp() {
  try {
    // The application code is injected externally;
    // at runtime, __APP_CODE__ is replaced with the real application code
    const appModule = await import("data:text/javascript," + encodeURIComponent(
      // @ts-ignore - __APP_CODE__ is injected at runtime
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

// Expose the global initialization function
// @ts-ignore
self.initializeApp = initializeApp;
