/**
 * Cloudflare KV adapter
 * Adapts Cloudflare KV Namespace to unified KVStore interface
 */

import type { KVStore, PutOptions, GetOptions, ListOptions, ListResult } from '@denictl/types';

/**
 * Create KV adapter for Cloudflare Workers KV
 */
export function createKVAdapter(namespace: any): KVStore {
  return {
    async get(key: string, options?: GetOptions): Promise<string | null> {
      return namespace.get(key, { cacheTtl: options?.cacheTtl });
    },

    async getJSON<T = any>(key: string, options?: GetOptions): Promise<T | null> {
      const value = await namespace.get(key, {
        type: 'json',
        cacheTtl: options?.cacheTtl
      });
      return value as T | null;
    },

    async getWithMetadata<T = any>(
      key: string,
      options?: GetOptions
    ): Promise<{ value: string | null; metadata: T | null }> {
      const result = await namespace.getWithMetadata(key, {
        cacheTtl: options?.cacheTtl
      });
      return {
        value: result.value,
        metadata: result.metadata as T | null
      };
    },

    async put(key: string, value: string, options?: PutOptions): Promise<void> {
      await namespace.put(key, value, {
        expirationTtl: options?.expirationTtl,
        expiration: options?.expiration,
        metadata: options?.metadata
      });
    },

    async putJSON<T = any>(key: string, value: T, options?: PutOptions): Promise<void> {
      await namespace.put(key, JSON.stringify(value), {
        expirationTtl: options?.expirationTtl,
        expiration: options?.expiration,
        metadata: options?.metadata
      });
    },

    async delete(key: string): Promise<void> {
      await namespace.delete(key);
    },

    async deleteMultiple(keys: string[]): Promise<void> {
      await Promise.all(keys.map(k => namespace.delete(k)));
    },

    async list(options?: ListOptions): Promise<ListResult> {
      const result = await namespace.list({
        prefix: options?.prefix,
        limit: options?.limit,
        cursor: options?.cursor
      });

      return {
        keys: result.keys.map((k: any) => ({
          name: k.name,
          expiration: k.expiration,
          metadata: k.metadata
        })),
        list_complete: result.list_complete,
        cursor: result.cursor
      };
    },

    async exists(key: string): Promise<boolean> {
      const value = await namespace.get(key);
      return value !== null;
    }
  };
}
