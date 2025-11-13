use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KVEntry {
    pub value: String,
    pub metadata: Option<serde_json::Value>,
    pub expiration: Option<u64>, // Unix timestamp
}

#[derive(Clone)]
pub struct MockKVStore {
    data: Arc<RwLock<HashMap<String, KVEntry>>>,
}

impl MockKVStore {
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn get(&self, key: &str) -> Option<String> {
        let data = self.data.read().await;

        if let Some(entry) = data.get(key) {
            // Check expiration
            if let Some(exp) = entry.expiration {
                let now = Self::current_timestamp();
                if now >= exp {
                    // Expired, return None
                    return None;
                }
            }
            Some(entry.value.clone())
        } else {
            None
        }
    }

    pub async fn get_with_metadata(&self, key: &str) -> Option<(String, Option<serde_json::Value>)> {
        let data = self.data.read().await;

        if let Some(entry) = data.get(key) {
            // Check expiration
            if let Some(exp) = entry.expiration {
                let now = Self::current_timestamp();
                if now >= exp {
                    return None;
                }
            }
            Some((entry.value.clone(), entry.metadata.clone()))
        } else {
            None
        }
    }

    pub async fn put(
        &self,
        key: String,
        value: String,
        metadata: Option<serde_json::Value>,
        expiration_ttl: Option<u64>,
    ) {
        let expiration = expiration_ttl.map(|ttl| Self::current_timestamp() + ttl);

        let entry = KVEntry {
            value,
            metadata,
            expiration,
        };

        let mut data = self.data.write().await;
        data.insert(key, entry);
    }

    pub async fn delete(&self, key: &str) -> bool {
        let mut data = self.data.write().await;
        data.remove(key).is_some()
    }

    pub async fn list(
        &self,
        prefix: Option<&str>,
        limit: Option<usize>,
        cursor: Option<&str>,
    ) -> ListResult {
        let data = self.data.read().await;
        let now = Self::current_timestamp();

        // Filter and collect keys
        let mut keys: Vec<String> = data
            .iter()
            .filter(|(k, v)| {
                // Check prefix
                if let Some(p) = prefix {
                    if !k.starts_with(p) {
                        return false;
                    }
                }

                // Check expiration
                if let Some(exp) = v.expiration {
                    if now >= exp {
                        return false;
                    }
                }

                // Check cursor (pagination)
                if let Some(c) = cursor {
                    if k.as_str() <= c {
                        return false;
                    }
                }

                true
            })
            .map(|(k, _)| k.clone())
            .collect();

        // Sort keys
        keys.sort();

        // Apply limit
        let limit = limit.unwrap_or(1000).min(1000); // Max 1000 like real KV
        let list_complete = keys.len() <= limit;

        if keys.len() > limit {
            keys.truncate(limit);
        }

        let next_cursor = if !list_complete {
            keys.last().cloned()
        } else {
            None
        };

        ListResult {
            keys: keys
                .into_iter()
                .map(|name| KeyInfo {
                    name: name.clone(),
                    expiration: data.get(&name).and_then(|e| e.expiration),
                    metadata: data.get(&name).and_then(|e| e.metadata.clone()),
                })
                .collect(),
            list_complete,
            cursor: next_cursor,
        }
    }

    pub async fn clear(&self) {
        let mut data = self.data.write().await;
        data.clear();
    }

    pub async fn count(&self) -> usize {
        let data = self.data.read().await;
        let now = Self::current_timestamp();

        data.iter()
            .filter(|(_, v)| {
                if let Some(exp) = v.expiration {
                    now < exp
                } else {
                    true
                }
            })
            .count()
    }

    fn current_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }
}

#[derive(Debug, Serialize)]
pub struct ListResult {
    pub keys: Vec<KeyInfo>,
    pub list_complete: bool,
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct KeyInfo {
    pub name: String,
    pub expiration: Option<u64>,
    pub metadata: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_basic_get_put() {
        let kv = MockKVStore::new();

        kv.put("key1".to_string(), "value1".to_string(), None, None).await;

        let value = kv.get("key1").await;
        assert_eq!(value, Some("value1".to_string()));
    }

    #[tokio::test]
    async fn test_delete() {
        let kv = MockKVStore::new();

        kv.put("key1".to_string(), "value1".to_string(), None, None).await;
        assert!(kv.delete("key1").await);
        assert_eq!(kv.get("key1").await, None);
    }

    #[tokio::test]
    async fn test_list_with_prefix() {
        let kv = MockKVStore::new();

        kv.put("app:user:1".to_string(), "data1".to_string(), None, None).await;
        kv.put("app:user:2".to_string(), "data2".to_string(), None, None).await;
        kv.put("app:post:1".to_string(), "data3".to_string(), None, None).await;

        let result = kv.list(Some("app:user:"), None, None).await;
        assert_eq!(result.keys.len(), 2);
    }

    #[tokio::test]
    async fn test_expiration() {
        let kv = MockKVStore::new();

        // TTL of 1 second
        kv.put("temp".to_string(), "value".to_string(), None, Some(1)).await;

        assert!(kv.get("temp").await.is_some());

        // Wait for expiration
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        assert!(kv.get("temp").await.is_none());
    }
}
