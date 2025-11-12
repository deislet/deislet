/**
 * Cloudflare D1 Database adapter
 * Adapts Cloudflare D1 to unified Database interface
 */

import type { Database, QueryResult, ExecuteResult, Transaction, BatchStatement, BatchResult } from '@denictl/types';

/**
 * Create Database adapter for Cloudflare D1
 */
export function createDatabaseAdapter(d1: any): Database {
  return {
    async query<T = any>(sql: string, params?: any[]): Promise<QueryResult<T>> {
      const stmt = params ? d1.prepare(sql).bind(...params) : d1.prepare(sql);
      const result = await stmt.all();

      return {
        rows: result.results as T[],
        rowCount: result.results.length
      };
    },

    async execute(sql: string, params?: any[]): Promise<ExecuteResult> {
      const stmt = params ? d1.prepare(sql).bind(...params) : d1.prepare(sql);
      const result = await stmt.run();

      return {
        rowsAffected: result.meta.changes,
        lastInsertId: result.meta.last_row_id
      };
    },

    async transaction<T>(
      callback: (tx: Transaction) => Promise<T>
    ): Promise<T> {
      // D1 doesn't support transactions yet, simulate with batch
      const tx: Transaction = {
        query: async <T = any>(sql: string, params?: any[]) => {
          return this.query<T>(sql, params);
        },
        execute: async (sql: string, params?: any[]) => {
          return this.execute(sql, params);
        },
        rollback: async () => {
          throw new Error('D1 does not support explicit rollback');
        }
      };

      return callback(tx);
    },

    async batch(statements: BatchStatement[]): Promise<BatchResult[]> {
      const preparedStatements = statements.map(stmt =>
        stmt.params
          ? d1.prepare(stmt.sql).bind(...stmt.params)
          : d1.prepare(stmt.sql)
      );

      const results = await d1.batch(preparedStatements);

      return results.map((result: any) => ({
        success: result.success,
        result: result.meta
          ? {
              rowsAffected: result.meta.changes,
              lastInsertId: result.meta.last_row_id
            }
          : undefined,
        error: result.error
      }));
    }
  };
}
