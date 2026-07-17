// 应用管理器实现

import type { App, AppConfig, AppState } from "./types.ts";
import { RuntimeIsolate } from "./isolate.ts";
import { ResourceManager } from "./resource_manager.ts";

/**
 * ApplicationManager 类
 * 管理应用的完整生命周期
 */
export class ApplicationManager {
  private apps = new Map<string, App>();
  private isolates = new Map<string, RuntimeIsolate>();
  private resourceManager: ResourceManager;

  constructor(resourceManager?: ResourceManager) {
    this.resourceManager = resourceManager || new ResourceManager();
  }

  /**
   * 加载应用
   */
  async loadApp(
    id: string,
    code: string,
    config: AppConfig = {},
  ): Promise<void> {
    // 检查应用是否已存在
    if (this.apps.has(id)) {
      throw new Error(`App ${id} already exists`);
    }

    // 创建应用记录
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
      // 创建资源
      await this.createAppResources(id, config);

      // 创建并初始化 Runtime Isolate
      const isolate = new RuntimeIsolate(id, code, config);
      await isolate.initialize();

      this.isolates.set(id, isolate);

      // 更新应用状态
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
   * 创建应用所需的资源
   */
  private async createAppResources(
    appId: string,
    config: AppConfig,
  ): Promise<void> {
    const resources = config.resources || [];

    for (const resourceSpec of resources) {
      // resourceSpec 格式: "type:name" 或 "type:name:config"
      const parts = resourceSpec.split(":");
      const type = parts[0];
      const name = parts[1] || type;
      const configStr = parts[2];

      let resourceConfig = {};
      if (configStr) {
        try {
          resourceConfig = JSON.parse(configStr);
        } catch {
          // 忽略配置解析错误
        }
      }

      await this.resourceManager.createResource(
        appId,
        type,
        name,
        resourceConfig,
      );
    }

    // 如果没有指定资源，默认创建一个 KV 存储
    if (resources.length === 0) {
      await this.resourceManager.createResource(appId, "kv", "kv", {});
    }
  }

  /**
   * 卸载应用
   */
  async unloadApp(id: string): Promise<void> {
    const app = this.apps.get(id);
    if (!app) {
      throw new Error(`App ${id} not found`);
    }

    try {
      // 更新状态
      app.state = "stopped";
      app.updatedAt = new Date();

      // 终止 Isolate
      const isolate = this.isolates.get(id);
      if (isolate) {
        isolate.terminate();
        this.isolates.delete(id);
      }

      // 销毁资源
      await this.resourceManager.destroyAppResources(id);

      // 删除应用记录
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
   * 更新应用 (热更新)
   */
  async updateApp(id: string, code: string, config?: AppConfig): Promise<void> {
    const app = this.apps.get(id);
    if (!app) {
      throw new Error(`App ${id} not found`);
    }

    try {
      // 保存旧的 isolate
      const oldIsolate = this.isolates.get(id);

      // 更新配置
      if (config) {
        app.config = config;
      }

      // 创建新的 isolate
      const newIsolate = new RuntimeIsolate(id, code, app.config);
      await newIsolate.initialize();

      // 更新应用记录
      app.code = code;
      app.state = "running";
      app.updatedAt = new Date();
      delete app.error;

      // 替换 isolate
      this.isolates.set(id, newIsolate);

      // 终止旧的 isolate
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
   * 执行应用
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
      // 获取应用环境
      const env = this.resourceManager.getAppEnv(id);

      // 执行请求
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
   * 获取应用信息
   */
  getApp(id: string): App | undefined {
    return this.apps.get(id);
  }

  /**
   * 列出所有应用
   */
  listApps(): App[] {
    return Array.from(this.apps.values());
  }

  /**
   * 获取应用统计信息
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
   * 获取资源管理器
   */
  getResourceManager(): ResourceManager {
    return this.resourceManager;
  }
}
