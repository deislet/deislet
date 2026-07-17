// Resource type definitions

/**
 * Resource provider interface
 */
export interface ResourceProvider {
  type: string;
  create(appId: string, config: Record<string, any>): Promise<any>;
  destroy(instance: any): Promise<void>;
}

/**
 * KV store interface
 */
export interface KVStore {
  get(key: string): Promise<string | null>;
  set(key: string, value: string, options?: { expiresIn?: number }): Promise<void>;
  delete(key: string): Promise<void>;
  list(prefix?: string): Promise<string[]>;
}
