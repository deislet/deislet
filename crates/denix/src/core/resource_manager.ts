// 资源管理器实现

import type { ResourceProvider } from "../resources/types.ts";
import { KVResourceProvider } from "../resources/kv.ts";

/**
 * ResourceManager 类
 * 管理应用可访问的外部资源
 */
export class ResourceManager {
  private providers = new Map<string, ResourceProvider>();
  private appResources = new Map<string, Map<string, any>>();

  constructor() {
    // 注册默认资源提供者
    this.registerProvider(new KVResourceProvider());
  }

  /**
   * 注册资源提供者
   */
  registerProvider(provider: ResourceProvider): void {
    this.providers.set(provider.type, provider);
  }

  /**
   * 为应用创建资源
   */
  async createResource(
    appId: string,
    resourceType: string,
    resourceName: string,
    config: Record<string, any> = {},
  ): Promise<any> {
    const provider = this.providers.get(resourceType);
    if (!provider) {
      throw new Error(`Unknown resource type: ${resourceType}`);
    }

    const instance = await provider.create(appId, config);

    // 存储资源实例
    if (!this.appResources.has(appId)) {
      this.appResources.set(appId, new Map());
    }

    this.appResources.get(appId)!.set(resourceName, {
      type: resourceType,
      instance,
    });

    return instance;
  }

  /**
   * 获取应用的环境对象
   * 包含所有已创建的资源
   */
  getAppEnv(appId: string): Record<string, any> {
    const resources = this.appResources.get(appId);
    if (!resources) {
      return {};
    }

    const env: Record<string, any> = {};
    for (const [name, resource] of resources.entries()) {
      env[name] = resource.instance;
    }

    return env;
  }

  /**
   * 销毁应用的所有资源
   */
  async destroyAppResources(appId: string): Promise<void> {
    const resources = this.appResources.get(appId);
    if (!resources) {
      return;
    }

    for (const [_name, resource] of resources.entries()) {
      const provider = this.providers.get(resource.type);
      if (provider) {
        await provider.destroy(resource.instance);
      }
    }

    this.appResources.delete(appId);
  }

  /**
   * 列出已注册的资源类型
   */
  listProviders(): string[] {
    return Array.from(this.providers.keys());
  }

  /**
   * 获取应用的资源列表
   */
  listAppResources(appId: string): Array<{ name: string; type: string }> {
    const resources = this.appResources.get(appId);
    if (!resources) {
      return [];
    }

    const list: Array<{ name: string; type: string }> = [];
    for (const [name, resource] of resources.entries()) {
      list.push({ name, type: resource.type });
    }

    return list;
  }
}
