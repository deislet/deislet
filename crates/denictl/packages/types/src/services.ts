/**
 * Unified service interfaces (KV Store, Database, Cache)
 * These are platform-agnostic interfaces that compilers adapt to platform-specific implementations
 */

/**
 * KV Store - Key-Value storage interface
 */
export interface KVStore {
  get(key: string, options?: GetOptions): Promise<string | null>;
  getJSON<T = any>(key: string, options?: GetOptions): Promise<T | null>;
  getWithMetadata<T = any>(
    key: string,
    options?: GetOptions
  ): Promise<{ value: string | null; metadata: T | null }>;

  put(key: string, value: string, options?: PutOptions): Promise<void>;
  putJSON<T = any>(key: string, value: T, options?: PutOptions): Promise<void>;

  delete(key: string): Promise<void>;
  deleteMultiple(keys: string[]): Promise<void>;

  list(options?: ListOptions): Promise<ListResult>;
  exists(key: string): Promise<boolean>;
}

export interface GetOptions {
  cacheTtl?: number;
}

export interface PutOptions {
  expirationTtl?: number; // Time to live in seconds
  expiration?: number; // Absolute expiration timestamp
  metadata?: any;
}

export interface ListOptions {
  prefix?: string;
  limit?: number;
  cursor?: string;
}

export interface ListResult {
  keys: Array<{ name: string; expiration?: number; metadata?: any }>;
  list_complete: boolean;
  cursor?: string;
}

/**
 * Database - SQL database interface
 */
export interface Database {
  query<T = any>(sql: string, params?: any[]): Promise<QueryResult<T>>;
  execute(sql: string, params?: any[]): Promise<ExecuteResult>;

  // Transaction support
  transaction<T>(
    callback: (tx: Transaction) => Promise<T>
  ): Promise<T>;

  // Batch operations
  batch(statements: BatchStatement[]): Promise<BatchResult[]>;
}

export interface QueryResult<T = any> {
  rows: T[];
  rowCount: number;
}

export interface ExecuteResult {
  rowsAffected: number;
  lastInsertId?: number | string;
}

export interface Transaction {
  query<T = any>(sql: string, params?: any[]): Promise<QueryResult<T>>;
  execute(sql: string, params?: any[]): Promise<ExecuteResult>;
  rollback(): Promise<void>;
}

export interface BatchStatement {
  sql: string;
  params?: any[];
}

export interface BatchResult {
  success: boolean;
  result?: ExecuteResult;
  error?: string;
}

/**
 * Services container - provided in context.services
 */
export interface Services {
  kv?: KVStore;
  database?: Database;
  // Future: queue, pubsub, etc.
}
