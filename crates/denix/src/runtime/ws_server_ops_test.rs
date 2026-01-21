// WebSocket Server Ops 单元测试

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

        // 注入必要的 Store
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

    #[test]
    fn test_ws_server_api_availability() {
        let mut runtime = setup_test_runtime();

        // 测试 WebSocketServer 和 WebSocketPair 类是否可用
        let code = r#"
            if (typeof WebSocketServer === "undefined") {
                throw new Error("WebSocketServer not available");
            }
            if (typeof WebSocketPair === "undefined") {
                throw new Error("WebSocketPair not available");
            }
            "ok";
        "#;

        // 先加载 runtime.js
        runtime
            .execute_script("<runtime>", include_str!("../../runtime_js/runtime.js"))
            .unwrap();

        runtime.execute_script("<test>", code).unwrap();
    }
}
