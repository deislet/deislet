// Application manager implementation

import type { App, AppConfig, AppState } from "./types.ts";
import { RuntimeIsolate } from "./isolate.ts";
import { ResourceManager } from "./resource_manager.ts";

/**
 * ApplicationManager class
 * Manages the full lifecycle of applications
 */
export class ApplicationManager {
  private apps = new Map<string, App>();
  private isolates = new Map<string, RuntimeIsolate>();
  private resourceManager: ResourceManager;

  constructor(resourceManager?: ResourceManager) {
    this.resourceManager = resourceManager || new ResourceManager();
  }

  /**
   * Load an application
   */
  async loadApp(
    id: string,
    code: string,
    config: AppConfig = {},
  ): Promise<void> {
    // Check whether the app already exists
    if (this.apps.has(id)) {
      throw new Error(`App ${id} already exists`);
    }

    // Create the app record
    const app: App = {
      id,
      code,
      state: "loading",
      config,
      createdAt: new Date(),
      updatedAt: new Date(),
    };

    this.apps.set(id, app);

    try {
      // Create resources
      await this.createAppResources(id, config);

      // Create and initialize the runtime isolate
      const isolate = new RuntimeIsolate(id, code, config);
      await isolate.initialize();

      this.isolates.set(id, isolate);

      // Update the app state
      app.state = "running";
      app.updatedAt = new Date();

      console.log(`[AppManager] App ${id} loaded successfully`);
    } catch (error) {
      app.state = "error";
      app.error = error instanceof Error ? error.message : String(error);
      app.updatedAt = new Date();

      console.error(`[AppManager] Failed to load app ${id}:`, error);
      throw error;
    }
  }

  /**
   * Create the resources required by an application
   */
  private async createAppResources(
    appId: string,
    config: AppConfig,
  ): Promise<void> {
    const resources = config.resources || [];

    for (const resourceSpec of resources) {
      // resourceSpec format: "type:name" or "type:name:config"
      const parts = resourceSpec.split(":");
      const type = parts[0];
      const name = parts[1] || type;
      const configStr = parts[2];

      let resourceConfig = {};
      if (configStr) {
        try {
          resourceConfig = JSON.parse(configStr);
        } catch {
          // Ignore config parsing errors
        }
      }

      await this.resourceManager.createResource(
        appId,
        type,
        name,
        resourceConfig,
      );
    }

    // If no resources are specified, create a KV store by default
    if (resources.length === 0) {
      await this.resourceManager.createResource(appId, "kv", "kv", {});
    }
  }

  /**
   * Unload an application
   */
  async unloadApp(id: string): Promise<void> {
    const app = this.apps.get(id);
    if (!app) {
      throw new Error(`App ${id} not found`);
    }

    try {
      // Update the state
      app.state = "stopped";
      app.updatedAt = new Date();

      // Terminate the isolate
      const isolate = this.isolates.get(id);
      if (isolate) {
        isolate.terminate();
        this.isolates.delete(id);
      }

      // Destroy resources
      await this.resourceManager.destroyAppResources(id);

      // Remove the app record
      this.apps.delete(id);

      console.log(`[AppManager] App ${id} unloaded successfully`);
    } catch (error) {
      app.state = "error";
      app.error = error instanceof Error ? error.message : String(error);
      app.updatedAt = new Date();

      console.error(`[AppManager] Failed to unload app ${id}:`, error);
      throw error;
    }
  }

  /**
   * Update an application (hot update)
   */
  async updateApp(id: string, code: string, config?: AppConfig): Promise<void> {
    const app = this.apps.get(id);
    if (!app) {
      throw new Error(`App ${id} not found`);
    }

    try {
      // Keep a reference to the old isolate
      const oldIsolate = this.isolates.get(id);

      // Update the configuration
      if (config) {
        app.config = config;
      }

      // Create a new isolate
      const newIsolate = new RuntimeIsolate(id, code, app.config);
      await newIsolate.initialize();

      // Update the app record
      app.code = code;
      app.state = "running";
      app.updatedAt = new Date();
      delete app.error;

      // Swap in the new isolate
      this.isolates.set(id, newIsolate);

      // Terminate the old isolate
      if (oldIsolate) {
        oldIsolate.terminate();
      }

      console.log(`[AppManager] App ${id} updated successfully`);
    } catch (error) {
      app.state = "error";
      app.error = error instanceof Error ? error.message : String(error);
      app.updatedAt = new Date();

      console.error(`[AppManager] Failed to update app ${id}:`, error);
      throw error;
    }
  }

  /**
   * Execute an application
   */
  async executeApp(id: string, request: Request): Promise<Response> {
    const app = this.apps.get(id);
    if (!app) {
      return new Response("App not found", { status: 404 });
    }

    if (app.state !== "running") {
      return new Response(`App is ${app.state}`, { status: 503 });
    }

    const isolate = this.isolates.get(id);
    if (!isolate) {
      return new Response("App isolate not found", { status: 500 });
    }

    try {
      // Get the app environment
      const env = this.resourceManager.getAppEnv(id);

      // Execute the request
      const response = await isolate.execute(request, env);
      return response;
    } catch (error) {
      console.error(`[AppManager] Error executing app ${id}:`, error);
      return new Response(
        `Internal error: ${error instanceof Error ? error.message : String(error)}`,
        { status: 500 },
      );
    }
  }

  /**
   * Get application info
   */
  getApp(id: string): App | undefined {
    return this.apps.get(id);
  }

  /**
   * List all applications
   */
  listApps(): App[] {
    return Array.from(this.apps.values());
  }

  /**
   * Get application statistics
   */
  getStats() {
    const apps = Array.from(this.apps.values());
    const stats = {
      total: apps.length,
      running: apps.filter((a) => a.state === "running").length,
      stopped: apps.filter((a) => a.state === "stopped").length,
      error: apps.filter((a) => a.state === "error").length,
      loading: apps.filter((a) => a.state === "loading").length,
    };

    return stats;
  }

  /**
   * Get the resource manager
   */
  getResourceManager(): ResourceManager {
    return this.resourceManager;
  }
}
