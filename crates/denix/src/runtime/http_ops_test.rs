// HTTP Ops 单元测试

#[cfg(test)]
mod tests {
    use crate::runtime::http_ops::init_http_extension;
    use crate::runtime::ops::{init_ops_extension, ResponseStore};
    use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    fn setup_test_runtime() -> JsRuntime {
        let runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_ops_extension(), init_http_extension()],
            ..Default::default()
        });

        // 注入必要的 Store
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        let response_store: ResponseStore = Rc::new(RefCell::new(None));
        state.put(response_store);

        runtime
    }

    #[tokio::test]
    async fn test_fetch_invalid_json() {
        let mut runtime = setup_test_runtime();

        // 测试无效 JSON 输入
        let code = r#"
            (async () => {
                try {
                    await Deno.core.ops.op_fetch("invalid json");
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

        // 验证代码执行不会崩溃即可
    }

    #[tokio::test]
    async fn test_fetch_empty_url() {
        let mut runtime = setup_test_runtime();

        // 测试空 URL
        let code = r#"
            (async () => {
                const request = {
                    url: "",
                    method: "GET",
                    headers: {},
                    body: null,
                };
                const result = await Deno.core.ops.op_fetch(JSON.stringify(request));
                if (result.error && result.error.includes("URL cannot be empty")) {
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
    async fn test_fetch_unsupported_method() {
        let mut runtime = setup_test_runtime();

        // 测试不支持的 HTTP 方法
        let code = r#"
            (async () => {
                const request = {
                    url: "https://example.com",
                    method: "INVALID",
                    headers: {},
                    body: null,
                };
                const result = await Deno.core.ops.op_fetch(JSON.stringify(request));
                if (result.error && result.error.includes("Unsupported HTTP method")) {
                    return "ok";
                }
                throw new Error("Expected error about unsupported method");
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    // 注意：真实的 HTTP 请求测试标记为 #[ignore]，需要网络连接
    #[tokio::test]
    #[ignore]
    async fn test_fetch_real_http_request() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                const request = {
                    url: "https://httpbin.org/get",
                    method: "GET",
                    headers: {
                        "User-Agent": "Denix-Test/1.0"
                    },
                    body: null,
                    timeout: 10
                };
                const result = await Deno.core.ops.op_fetch(JSON.stringify(request));

                if (result.error) {
                    throw new Error(`fetch failed: ${result.error}`);
                }

                if (result.status !== 200) {
                    throw new Error(`Expected status 200, got ${result.status}`);
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

    #[tokio::test]
    #[ignore]
    async fn test_fetch_post_with_body() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                const request = {
                    url: "https://httpbin.org/post",
                    method: "POST",
                    headers: {
                        "Content-Type": "application/json"
                    },
                    body: JSON.stringify({ test: "data" }),
                    timeout: 10
                };
                const result = await Deno.core.ops.op_fetch(JSON.stringify(request));

                if (result.error) {
                    throw new Error(`fetch failed: ${result.error}`);
                }

                if (result.status !== 200) {
                    throw new Error(`Expected status 200, got ${result.status}`);
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
