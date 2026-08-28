// KV storage resource implementation
// Provides a simple in-memory KV store

import type { ResourceProvider, KVStore } from "./types.ts";

/**
 * In-memory KV store implementation
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

    // Check for expiration
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
        // Check for expiration
        if (item.expiresAt && item.expiresAt < Date.now()) {
          this.store.delete(key);
          continue;
        }

        // Strip the prefix and return the original key
        keys.push(key.substring(this.prefix.length));
      }
    }

    return keys;
  }

  /**
   * Remove expired keys
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
 * KV resource provider
 */
export class KVResourceProvider implements ResourceProvider {
  type = "kv";

  async create(
    appId: string,
    config: Record<string, any>,
  ): Promise<KVStore> {
    const namespace = config.namespace || "default";
    const store = new MemoryKVStore(appId, namespace);

    // Periodically clean up expired keys
    setInterval(() => {
      store.cleanup();
    }, 60000); // Clean up once per minute

    return store;
  }

  async destroy(_instance: KVStore): Promise<void> {
    // In-memory store requires no special cleanup
  }
}
