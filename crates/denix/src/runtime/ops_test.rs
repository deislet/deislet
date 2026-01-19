// Runtime Ops 单元测试

#[cfg(test)]
mod tests {
    use crate::runtime::ops::{init_ops_extension, KVStore, ResponseStore};
    use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    fn setup_test_runtime() -> JsRuntime {
        let runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_ops_extension()],
            ..Default::default()
        });

        // 注入 KV Store 和 Response Store
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        let kv_store: KVStore = Rc::new(RefCell::new(HashMap::new()));
        let response_store: ResponseStore = Rc::new(RefCell::new(None));
        state.put(kv_store);
        state.put(response_store);

        runtime
    }

    #[tokio::test]
    async fn test_kv_set_and_get() {
        let mut runtime = setup_test_runtime();

        // Set and get value (ops are now async)
        let code = r#"
            (async () => {
                await Deno.core.ops.op_kv_set("test_key", "test_value");
                return await Deno.core.ops.op_kv_get("test_key");
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        // 验证结果通过 OpState 直接访问
        let op_state = runtime.op_state();
        let state = op_state.borrow();
        let kv = state.borrow::<KVStore>().clone();
        let store = kv.borrow();

        assert_eq!(store.get("test_key"), Some(&"test_value".to_string()));
    }

    #[tokio::test]
    async fn test_kv_delete() {
        let mut runtime = setup_test_runtime();

        // Set then delete (ops are now async)
        let code = r#"
            (async () => {
                await Deno.core.ops.op_kv_set("del_key", "del_value");
                await Deno.core.ops.op_kv_delete("del_key");
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        // 验证结果通过 OpState 直接访问
        let op_state = runtime.op_state();
        let state = op_state.borrow();
        let kv = state.borrow::<KVStore>().clone();
        let store = kv.borrow();

        assert_eq!(store.get("del_key"), None);
    }

    #[tokio::test]
    async fn test_kv_list() {
        let mut runtime = setup_test_runtime();

        // Set keys and list (ops are now async)
        let code = r#"
            (async () => {
                await Deno.core.ops.op_kv_set("prefix:key1", "val1");
                await Deno.core.ops.op_kv_set("prefix:key2", "val2");
                await Deno.core.ops.op_kv_set("other:key3", "val3");
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        // 验证结果通过 OpState 直接访问
        let op_state = runtime.op_state();
        let state = op_state.borrow();
        let kv = state.borrow::<KVStore>().clone();
        let store = kv.borrow();

        // 检查存储的键
        let keys: Vec<String> = store
            .keys()
            .filter(|k| k.starts_with("prefix:"))
            .cloned()
            .collect();

        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"prefix:key1".to_string()));
        assert!(keys.contains(&"prefix:key2".to_string()));
    }
}
