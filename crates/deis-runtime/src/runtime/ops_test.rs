// Runtime Ops unit tests

#[cfg(test)]
mod tests {
    use crate::runtime::ops::{init_ops_extension, EnvStore, ResponseStore};
    use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    fn setup_test_runtime() -> JsRuntime {
        let runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_ops_extension()],
            ..Default::default()
        });

        // Inject Response Store and Env Store
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        let response_store: ResponseStore = Rc::new(RefCell::new(None));
        let env_store: EnvStore = Rc::new(RefCell::new(HashMap::from([(
            "ENV_KEY".to_string(),
            "env_value".to_string(),
        )])));
        state.put(response_store);
        state.put(env_store);

        runtime
    }

    #[tokio::test]
    async fn test_get_env() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                const env = Deno.core.ops.op_get_env();
                Deno.core.ops.op_set_response(JSON.stringify(env));
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        let op_state = runtime.op_state();
        let state = op_state.borrow();
        let store = state.borrow::<ResponseStore>().clone();
        let response = store.borrow().clone().unwrap();
        let env: HashMap<String, String> = serde_json::from_str(&response).unwrap();

        assert_eq!(env.get("ENV_KEY"), Some(&"env_value".to_string()));
    }
}
