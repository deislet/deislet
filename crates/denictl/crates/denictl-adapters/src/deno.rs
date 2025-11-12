use denictl_core::ProjectConfig;
use std::fs;
use std::path::Path;

pub fn generate(output_dir: &Path, config: &ProjectConfig) -> anyhow::Result<()> {
    // Generate deno.json
    let deno_json = serde_json::json!({
        "tasks": {
            "start": "deno serve -A functions/index.ts"
        },
        "imports": {
            "std/": "https://deno.land/std@0.208.0/"
        }
    });

    fs::write(
        output_dir.join("deno.json"),
        serde_json::to_string_pretty(&deno_json)?,
    )?;

    // Generate KV adapter
    let kv_adapter = r#"/**
 * Deno KV Adapter
 */

export function createKVAdapter(kv: Deno.Kv) {
  return {
    async get(key: string): Promise<string | null> {
      const result = await kv.get([key]);
      return result.value ? String(result.value) : null;
    },

    async getJSON<T = any>(key: string): Promise<T | null> {
      const result = await kv.get([key]);
      return result.value as T | null;
    },

    async put(key: string, value: string, options?: any): Promise<void> {
      const kvOptions: any = {};
      if (options?.expirationTtl) {
        kvOptions.expireIn = options.expirationTtl * 1000;
      }
      await kv.set([key], value, kvOptions);
    },

    async putJSON<T = any>(key: string, value: T, options?: any): Promise<void> {
      const kvOptions: any = {};
      if (options?.expirationTtl) {
        kvOptions.expireIn = options.expirationTtl * 1000;
      }
      await kv.set([key], value, kvOptions);
    },

    async delete(key: string): Promise<void> {
      await kv.delete([key]);
    },

    async list(options?: any) {
      const entries = kv.list(options?.prefix ? { prefix: [options.prefix] } : {});
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
"#;

    let adapters_dir = output_dir.join("adapters");
    fs::create_dir_all(&adapters_dir)?;
    fs::write(adapters_dir.join("kv-adapter.ts"), kv_adapter)?;

    Ok(())
}
