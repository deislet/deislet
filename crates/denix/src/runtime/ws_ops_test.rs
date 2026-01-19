// WebSocket Ops 单元测试

#[cfg(test)]
mod tests {
    use crate::runtime::ops::{init_ops_extension, KVStore, ResponseStore};
    use crate::runtime::ws_ops::{create_ws_manager, init_ws_extension};
    use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    fn setup_test_runtime() -> JsRuntime {
        let runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_ops_extension(), init_ws_extension()],
            ..Default::default()
        });

        // 注入必要的 Store
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        let kv_store: KVStore = Rc::new(RefCell::new(HashMap::new()));
        let response_store: ResponseStore = Rc::new(RefCell::new(None));
        state.put(kv_store);
        state.put(response_store);
        state.put(create_ws_manager());

        runtime
    }

    #[tokio::test]
    async fn test_ws_connect_invalid_json() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                try {
                    await Deno.core.ops.op_ws_connect("invalid json");
                    return "should_not_reach";
                } catch (error) {
                    return error.message || "error";
                }
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        let _resolved = runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_ws_connect_empty_url() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                const request = {
                    ws_id: "test-ws-1",
                    url: "",
                    protocols: null
                };
                const result = await Deno.core.ops.op_ws_connect(JSON.stringify(request));
                if (result.success === false && result.error.includes("URL cannot be empty")) {
                    return "ok";
                }
                throw new Error("Expected error about empty URL");
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
    async fn test_ws_send_nonexistent() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                const result = await Deno.core.ops.op_ws_send("nonexistent-ws", "hello");
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
    async fn test_ws_close_nonexistent() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                const result = await Deno.core.ops.op_ws_close("nonexistent-ws");
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

    // 注意：真实的 WebSocket 连接测试标记为 #[ignore]，需要 WebSocket 服务器
    #[tokio::test]
    #[ignore]
    async fn test_ws_real_connection() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                // 连接到 WebSocket echo 服务
                const connectRequest = {
                    ws_id: "test-ws-1",
                    url: "wss://echo.websocket.org/",
                    protocols: null
                };
                const connectResult = await Deno.core.ops.op_ws_connect(
                    JSON.stringify(connectRequest)
                );

                if (!connectResult.success) {
                    throw new Error(`Connect failed: ${connectResult.error}`);
                }

                // 发送消息
                const sendResult = await Deno.core.ops.op_ws_send(
                    "test-ws-1",
                    "Hello WebSocket!"
                );

                if (!sendResult.success) {
                    throw new Error(`Send failed: ${sendResult.error}`);
                }

                // 等待一下（实际应该通过事件接收响应）
                await new Promise(resolve => setTimeout(resolve, 100));

                // 关闭连接
                const closeResult = await Deno.core.ops.op_ws_close("test-ws-1");

                if (!closeResult.success) {
                    throw new Error(`Close failed: ${closeResult.error}`);
                }

                return "ok";
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }
}
