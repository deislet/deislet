// WebSocket Server Ops unit tests

#[cfg(test)]
mod tests {
    use crate::runtime::ops::{init_ops_extension, ResponseStore};
    use crate::runtime::ws_server_ops::{create_ws_server_manager, init_ws_server_extension};
    use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};
    use std::cell::RefCell;
    // use std::collections::HashMap;
    use std::rc::Rc;

    fn setup_test_runtime() -> JsRuntime {
        let runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_ops_extension(), init_ws_server_extension()],
            ..Default::default()
        });

        // Inject necessary Stores
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        let response_store: ResponseStore = Rc::new(RefCell::new(None));
        state.put(response_store);
        state.put(create_ws_server_manager());

        runtime
    }

    #[test]
    fn test_ws_server_accept() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            const result = Deno.core.ops.op_ws_server_accept("test-ws-1");
            if (result.success === true) {
                "ok";
            } else {
                throw new Error("Expected success");
            }
        "#;

        runtime.execute_script("<test>", code).unwrap();
    }

    #[tokio::test]
    async fn test_ws_server_send_nonexistent() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                const result = await Deno.core.ops.op_ws_server_send("nonexistent-ws", "hello");
                if (result.success === false && result.error.includes("not found")) {
                    return "ok";
                }
                throw new Error("Expected error about connection not found");
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
    async fn test_ws_server_close_nonexistent() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                const request = {
                    ws_id: "nonexistent-ws",
                    code: 1000,
                    reason: "test close"
                };
                const result = await Deno.core.ops.op_ws_server_close(JSON.stringify(request));
                if (result.success === false && result.error.includes("not found")) {
                    return "ok";
                }
                throw new Error("Expected error about connection not found");
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    /// The WebSocket classes are reachable from an application.
    ///
    /// Driven through a real `AppRuntime` rather than by evaluating
    /// `js/runtime.js` on a bare `JsRuntime`, which is what this used to do.
    /// That shortcut stopped working once the standard web globals were bound
    /// from the deno extensions: `js/runtime.js` now subclasses the `Response`
    /// that `js/bootstrap.js` binds, so evaluating it in an isolate that was
    /// built without those extensions fails on the first class it declares.
    /// Going through `AppRuntime` also means this asserts what an application
    /// actually sees, rather than what one particular script leaves behind.
    #[tokio::test]
    async fn test_ws_server_api_availability() {
        use crate::runtime::isolate::{AppRuntime, IsolateSpec};
        use crate::runtime::ServiceWiring;
        use std::collections::HashMap;

        let code = r#"
            export default function handler() {
                if (typeof WebSocketServer === "undefined") {
                    throw new Error("WebSocketServer not available");
                }
                if (typeof WebSocketPair === "undefined") {
                    throw new Error("WebSocketPair not available");
                }
                return new Response("ok");
            }
        "#;

        let mut runtime = AppRuntime::new(
            "ws_api_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(body.as_deref(), Some("ok"));
    }
}
