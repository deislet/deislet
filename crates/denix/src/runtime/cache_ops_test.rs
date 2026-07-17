// Cache Ops unit tests

#[cfg(test)]
mod tests {
    use crate::runtime::cache_ops::{create_cache_store, init_cache_extension, CacheStore};
    use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};

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
        assert_eq!(value, Some("test_value".to_string()));
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

        assert_eq!(has_exists, true);
        assert_eq!(has_not_exists, false);
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

        assert_eq!(v1, Some("value1".to_string()));
        assert_eq!(v2, None);
        assert_eq!(v3, Some("value3".to_string()));
    }
}
