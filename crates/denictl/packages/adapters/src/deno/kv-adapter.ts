/**
 * Deno KV adapter
 * Adapts Deno.Kv to unified KVStore interface
 */

import type { KVStore, PutOptions, GetOptions, ListOptions, ListResult } from '@denictl/types';

/**
 * Create KV adapter for Deno KV
 */
export function createKVAdapter(kv: any): KVStore {
  return {
    async get(key: string, options?: GetOptions): Promise<string | null> {
      const result = await kv.get([key]);
      return result.value ? String(result.value) : null;
    },

    async getJSON<T = any>(key: string, options?: GetOptions): Promise<T | null> {
      const result = await kv.get([key]);
      return result.value as T | null;
    },

    async getWithMetadata<T = any>(
      key: string,
      options?: GetOptions
    ): Promise<{ value: string | null; metadata: T | null }> {
      const result = await kv.get([key]);
      // Deno KV doesn't have separate metadata, return versionstamp as metadata
      return {
        value: result.value ? String(result.value) : null,
        metadata: (result.versionstamp as T) || null
      };
    },

    async put(key: string, value: string, options?: PutOptions): Promise<void> {
      const kvOptions: any = {};
      if (options?.expirationTtl) {
        kvOptions.expireIn = options.expirationTtl * 1000; // Convert seconds to ms
      }

      await kv.set([key], value, kvOptions);
    },

    async putJSON<T = any>(key: string, value: T, options?: PutOptions): Promise<void> {
      const kvOptions: any = {};
      if (options?.expirationTtl) {
        kvOptions.expireIn = options.expirationTtl * 1000;
      }

      await kv.set([key], value, kvOptions);
    },

    async delete(key: string): Promise<void> {
      await kv.delete([key]);
    },

    async deleteMultiple(keys: string[]): Promise<void> {
      const atomic = kv.atomic();
      keys.forEach(k => atomic.delete([k]));
      await atomic.commit();
    },

    async list(options?: ListOptions): Promise<ListResult> {
      const listOptions: any = {};
      if (options?.prefix) {
        listOptions.prefix = [options.prefix];
      }
      if (options?.limit) {
        listOptions.limit = options.limit;
      }

      const entries = kv.list(listOptions);
      const keys: any[] = [];

      for await (const entry of entries) {
        keys.push({
          name: entry.key[0],
          metadata: entry.versionstamp
        });
      }

      return {
        keys,
        list_complete: true,
        cursor: undefined
      };
    },

    async exists(key: string): Promise<boolean> {
      const result = await kv.get([key]);
      return result.value !== null;
    }
  };
}
