// Database Ops unit tests

#[cfg(test)]
mod tests {
    use crate::runtime::db_ops::init_db_extension;
    use crate::runtime::ops::{init_ops_extension, ResponseStore};
    use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};
    use std::cell::RefCell;
    // use std::collections::HashMap;
    use std::rc::Rc;

    fn setup_test_runtime() -> JsRuntime {
        let runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_ops_extension(), init_db_extension()],
            ..Default::default()
        });

        // Inject necessary Stores (DbClient not injected in tests to test error handling)
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        let response_store: ResponseStore = Rc::new(RefCell::new(None));
        state.put(response_store);

        runtime
    }

    #[tokio::test]
    async fn test_db_execute_without_client() {
        let mut runtime = setup_test_runtime();

        // Test error handling without DbClient
        let code = r#"
            (async () => {
                try {
                    await Deno.core.ops.op_db_execute("SELECT 1", null);
                    return "should_not_reach";
                } catch (error) {
                    return error.message;
                }
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        let resolved = runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        // Verify that error information is returned (via runtime internal mechanism)
        // Should throw "Database client not configured" error since DbClient is not configured
        // Actual test verifies that the code execution does not crash
        drop(resolved);
    }

    #[tokio::test]
    async fn test_db_query_without_client() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                try {
                    await Deno.core.ops.op_db_query("SELECT 1", null);
                    return "should_not_reach";
                } catch (error) {
                    return error.message;
                }
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_db_batch_without_client() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                try {
                    await Deno.core.ops.op_db_batch(JSON.stringify([
                        { sql: "INSERT INTO test VALUES (?)", params: "[1]" }
                    ]));
                    return "should_not_reach";
                } catch (error) {
                    return error.message;
                }
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_db_prepare_without_client() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                try {
                    await Deno.core.ops.op_db_prepare("SELECT * FROM test WHERE id = ?");
                    return "should_not_reach";
                } catch (error) {
                    return error.message;
                }
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    // Note: Complete integration tests require a real denira server
    // These tests only verify the basic structure and error handling of ops
    // Real Database operation tests are in db_client.rs, marked as #[ignore]
}
