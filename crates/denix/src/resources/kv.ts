// KV 存储资源实现
// 提供简单的内存 KV 存储

import type { ResourceProvider, KVStore } from "./types.ts";

/**
 * 内存 KV 存储实现
 */
class MemoryKVStore implements KVStore {
  private store = new Map<string, { value: string; expiresAt?: number }>();
  private prefix: string;

  constructor(private appId: string, namespace = "default") {
    this.prefix = `${appId}:${namespace}:`;
  }

  private getFullKey(key: string): string {
    return this.prefix + key;
  }

  async get(key: string): Promise<string | null> {
    const fullKey = this.getFullKey(key);
    const item = this.store.get(fullKey);

    if (!item) {
      return null;
    }

    // 检查是否过期
    if (item.expiresAt && item.expiresAt < Date.now()) {
      this.store.delete(fullKey);
      return null;
    }

    return item.value;
  }

  async set(
    key: string,
    value: string,
    options?: { expiresIn?: number },
  ): Promise<void> {
    const fullKey = this.getFullKey(key);
    const expiresAt = options?.expiresIn
      ? Date.now() + options.expiresIn
      : undefined;

    this.store.set(fullKey, { value, expiresAt });
  }

  async delete(key: string): Promise<void> {
    const fullKey = this.getFullKey(key);
    this.store.delete(fullKey);
  }

  async list(prefix = ""): Promise<string[]> {
    const fullPrefix = this.getFullKey(prefix);
    const keys: string[] = [];

    for (const [key, item] of this.store.entries()) {
      if (key.startsWith(fullPrefix)) {
        // 检查是否过期
        if (item.expiresAt && item.expiresAt < Date.now()) {
          this.store.delete(key);
          continue;
        }

        // 移除前缀返回原始 key
        keys.push(key.substring(this.prefix.length));
      }
    }

    return keys;
  }

  /**
   * 清理过期的键
   */
  cleanup(): void {
    const now = Date.now();
    for (const [key, item] of this.store.entries()) {
      if (item.expiresAt && item.expiresAt < now) {
        this.store.delete(key);
      }
    }
  }
}

/**
 * KV 资源提供者
 */
export class KVResourceProvider implements ResourceProvider {
  type = "kv";

  async create(
    appId: string,
    config: Record<string, any>,
  ): Promise<KVStore> {
    const namespace = config.namespace || "default";
    const store = new MemoryKVStore(appId, namespace);

    // 定期清理过期键
    setInterval(() => {
      store.cleanup();
    }, 60000); // 每分钟清理一次

    return store;
  }

  async destroy(_instance: KVStore): Promise<void> {
    // 内存存储不需要特殊清理
  }
}
