// Resource manager implementation

import type { ResourceProvider } from "../resources/types.ts";
import { KVResourceProvider } from "../resources/kv.ts";

/**
 * ResourceManager class
 * Manages external resources accessible to applications
 */
export class ResourceManager {
  private providers = new Map<string, ResourceProvider>();
  private appResources = new Map<string, Map<string, any>>();

  constructor() {
    // Register default resource providers
    this.registerProvider(new KVResourceProvider());
  }

  /**
   * Register a resource provider
   */
  registerProvider(provider: ResourceProvider): void {
    this.providers.set(provider.type, provider);
  }

  /**
   * Create a resource for an application
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

    // Store the resource instance
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
   * Get the environment object for an application,
   * containing all created resources
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
   * Destroy all resources of an application
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
   * List registered resource types
   */
  listProviders(): string[] {
    return Array.from(this.providers.keys());
  }

  /**
   * Get the resource list of an application
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
