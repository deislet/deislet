// Cache Ops unit tests

#[cfg(test)]
mod tests {
    use crate::runtime::cache_ops::{create_cache_store, init_cache_extension, CacheStore};
    use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};
    use std::time::Duration;

    /// Run one script to completion, driving the event loop the async ops
    /// need.
    async fn run(runtime: &mut JsRuntime, code: &'static str) {
        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    fn store(runtime: &JsRuntime) -> CacheStore {
        let op_state = runtime.op_state();
        let state = op_state.borrow();
        let cache = state.borrow::<CacheStore>().clone();
        cache
    }

    fn setup_test_runtime() -> JsRuntime {
        let runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_cache_extension()],
            ..Default::default()
        });

        // Inject CacheStore
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        let cache_store: CacheStore = create_cache_store(100, 0); // 100 entries, no TTL
        state.put(cache_store);

        runtime
    }

    #[tokio::test]
    async fn test_cache_set_and_get() {
        let mut runtime = setup_test_runtime();

        // Set and get value
        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set("test_key", "test_value", 0);
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        // Verify by directly accessing CacheStore
        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        let value = cache.get("test_key").await;
        assert_eq!(
            value.map(|entry| entry.payload),
            Some("test_value".to_string())
        );
    }

    #[tokio::test]
    async fn test_cache_get_nonexistent() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_get("nonexistent_key");
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        let value = cache.get("nonexistent_key").await;
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn test_cache_delete() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set("key_to_delete", "value", 0);
                await Deno.core.ops.op_cache_delete("key_to_delete");
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        let value = cache.get("key_to_delete").await;
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn test_cache_has() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set("exists_key", "value", 0);
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        let has_exists = cache.contains_key("exists_key");
        let has_not_exists = cache.contains_key("not_exists_key");

        assert!(has_exists);
        assert!(!has_not_exists);
    }

    #[tokio::test]
    async fn test_cache_clear() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set("key1", "value1", 0);
                await Deno.core.ops.op_cache_set("key2", "value2", 0);
                await Deno.core.ops.op_cache_clear();
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        cache.run_pending_tasks().await;
        let count = cache.entry_count();

        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_cache_stats() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set("stat_key1", "value1", 0);
                await Deno.core.ops.op_cache_set("stat_key2", "value2", 0);
                await Deno.core.ops.op_cache_set("stat_key3", "value3", 0);
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        cache.run_pending_tasks().await;
        let count = cache.entry_count();

        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_cache_multiple_operations() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set("multi1", "value1", 0);
                await Deno.core.ops.op_cache_set("multi2", "value2", 0);
                await Deno.core.ops.op_cache_set("multi3", "value3", 0);
                await Deno.core.ops.op_cache_delete("multi2");
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        let v1 = cache.get("multi1").await;
        let v2 = cache.get("multi2").await;
        let v3 = cache.get("multi3").await;

        assert_eq!(v1.map(|entry| entry.payload), Some("value1".to_string()));
        assert!(v2.is_none());
        assert_eq!(v3.map(|entry| entry.payload), Some("value3".to_string()));
    }
    #[tokio::test]
    async fn a_lifetime_the_response_asked_for_is_stored_with_the_entry() {
        // The third argument used to be ignored and documented as "reserved",
        // so every entry lived exactly as long as every other one no matter
        // what its `Cache-Control` said.
        let mut runtime = setup_test_runtime();

        run(
            &mut runtime,
            r#"
            (async () => {
                await Deno.core.ops.op_cache_set("with_ttl", "value", 120);
                await Deno.core.ops.op_cache_set("without_ttl", "value", 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.get("with_ttl").await.unwrap().ttl,
            Some(Duration::from_secs(120))
        );
        // Zero is not "expire immediately"; it is "the response named no
        // lifetime", and the store's own default applies.
        assert_eq!(cache.get("without_ttl").await.unwrap().ttl, None);
    }

    #[test]
    fn an_entry_expires_on_its_own_lifetime_and_falls_back_to_the_default() {
        // The policy itself, asked directly, so the assertion is about which
        // duration is chosen and not about waiting for one to elapse.
        use crate::runtime::cache_ops::{CacheEntry, ResponseFreshness};
        use moka::Expiry;
        use std::time::Instant;

        let policy = ResponseFreshness {
            default_ttl: Some(Duration::from_secs(600)),
        };
        let now = Instant::now();
        let key = "http://example/x".to_string();

        let asked = CacheEntry {
            payload: "body".to_string(),
            ttl: Some(Duration::from_secs(30)),
        };
        let silent = CacheEntry {
            payload: "body".to_string(),
            ttl: None,
        };

        assert_eq!(
            policy.expire_after_create(&key, &asked, now),
            Some(Duration::from_secs(30)),
            "a response that named a max-age keeps it"
        );
        assert_eq!(
            policy.expire_after_create(&key, &silent, now),
            Some(Duration::from_secs(600)),
            "a response that named none gets the store's default"
        );
        // Writing the same key again restarts the clock rather than
        // inheriting what was left of the previous response's lifetime.
        assert_eq!(
            policy.expire_after_update(&key, &asked, now, Some(Duration::from_secs(1))),
            Some(Duration::from_secs(30))
        );
        // Reading must not extend freshness: an HTTP cache entry goes stale a
        // fixed time after it was written, however often it is served.
        assert_eq!(
            policy.expire_after_read(&key, &asked, now, Some(Duration::from_secs(7)), now),
            Some(Duration::from_secs(7))
        );
    }

    #[tokio::test]
    async fn delete_says_whether_there_was_anything_to_delete() {
        // The Cache API's `delete()` resolves to that answer, and an
        // unconditional `true` told an application it had invalidated
        // something it never had.
        let mut runtime = setup_test_runtime();

        run(
            &mut runtime,
            r#"
            (async () => {
                await Deno.core.ops.op_cache_set("present", "value", 0);
                const removedPresent = await Deno.core.ops.op_cache_delete("present");
                const removedAbsent = await Deno.core.ops.op_cache_delete("never_written");
                // Parked in the cache itself rather than read back through a
                // v8 scope: the answer under test is a value the op returned.
                await Deno.core.ops.op_cache_set(
                    "answers", removedPresent + "," + removedAbsent, 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.get("answers").await.unwrap().payload,
            "true,false",
            "delete() must report a hit as true and a miss as false"
        );
        assert!(cache.get("present").await.is_none());
    }
}
