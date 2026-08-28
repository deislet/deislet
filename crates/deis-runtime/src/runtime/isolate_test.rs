#[cfg(test)]
mod tests {
    use crate::app::types::AppPermissions;
    use crate::runtime::isolate::AppRuntime;
    use std::collections::HashMap;
    use tokio::sync::oneshot;

    #[test]
    fn test_transform_esm_typescript() {
        let ts_code = r#"export default function handler(_request: Request): Response {
    return new Response("Hello from Runtime!", {
      status: 200,
      headers: {
        "Content-Type": "text/plain",
      },
    });
};"#;

        let result = AppRuntime::new(
            "test_esm".to_string(),
            ts_code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            Vec::new(),
            30_000,
            1000.0,
        );
        assert!(
            result.is_ok(),
            "Failed to parse TypeScript ESM: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_transform_counter_app() {
        let ts_code = r#"export default function handler(context: any): Response {
    const url = new URL(context.request.url);
    return new Response("ok");
};"#;

        let result = AppRuntime::new(
            "test_counter".to_string(),
            ts_code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            Vec::new(),
            30_000,
            1000.0,
        );
        assert!(
            result.is_ok(),
            "Failed to parse counter app: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_transform_with_interface() {
        let ts_code = r#"interface Todo {
  id: string;
  title: string;
}

export default function handler(context: any): Response {
    const todos: Todo[] = [];
    return new Response(JSON.stringify(todos));
};"#;

        let result = AppRuntime::new(
            "test_interface".to_string(),
            ts_code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            Vec::new(),
            30_000,
            1000.0,
        );
        assert!(
            result.is_ok(),
            "Failed to parse TypeScript with interface: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_transform_echo_app() {
        let ts_code = r#"export default function handler(context: any): Response {
    const request = context.request;
    const url = new URL(request.url);
    const info: any = {
      method: request.method,
      url: request.url,
      path: url.pathname,
    };
    return new Response(JSON.stringify(info));
};"#;

        let result = AppRuntime::new(
            "test_echo".to_string(),
            ts_code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            Vec::new(),
            30_000,
            1000.0,
        );
        assert!(
            result.is_ok(),
            "Failed to parse echo app: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_create_runtime_with_valid_code() {
        let code = r#"
            export default function handler() {
                    return new Response("Hello");
            }
        "#;

        let result = AppRuntime::new(
            "test_app".to_string(),
            code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            Vec::new(),
            30_000,
            1000.0,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_runtime_with_invalid_code() {
        let invalid_code = "this is not valid javascript {{{";
        let result = AppRuntime::new(
            "test_app".to_string(),
            invalid_code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            Vec::new(),
            30_000,
            1000.0,
        );
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_create_runtime() {
        let code = r#"
            export default function handler() {
                    return new Response("test");
            }
        "#;

        let result = AppRuntime::new(
            "test_app".to_string(),
            code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            Vec::new(),
            30_000,
            1000.0,
        );
        assert!(result.is_ok());

        let runtime = result.unwrap();
        assert_eq!(runtime.app_id(), "test_app");
    }

    #[tokio::test]
    async fn test_execute_fetch() {
        let code = r#"
            export default function handler() {
                    return new Response("Hello from test", {
                        status: 200,
                        headers: { "content-type": "text/plain" }
                    });
            }
        "#;

        let mut runtime = AppRuntime::new(
            "test_app".to_string(),
            code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            Vec::new(),
            30_000,
            1000.0,
        )
        .unwrap();

        runtime.init_module().await.unwrap();

        let mut headers = HashMap::new();
        headers.insert("host".to_string(), "localhost".to_string());

        let (tx, rx) = oneshot::channel();
        let result = runtime
            .execute_fetch(
                "http://localhost/test".to_string(),
                "GET".to_string(),
                headers,
                None,
                tx,
            )
            .await;

        assert!(result.is_ok());

        let result_data = rx.await.unwrap().unwrap();
        let (status, _headers, body) = result_data;
        assert_eq!(status, 200);
        assert_eq!(body, Some("Hello from test".to_string()));
    }

    #[tokio::test]
    #[ignore]
    async fn test_execute_fetch_with_kv() {
        let code = r#"
            export default async function handler(ctx) {
                    await ctx.services.kv.put("test", "value123");
                    const val = await ctx.services.kv.get("test");
                    return new Response(val);
            }
        "#;

        let mut runtime = AppRuntime::new(
            "test_app".to_string(),
            code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            Vec::new(),
            30_000,
            1000.0,
        )
        .unwrap();

        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        let result = runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await;

        assert!(result.is_ok());

        let result_data = rx.await.unwrap().unwrap();
        let (_status, _headers, body) = result_data;
        assert_eq!(body, Some("value123".to_string()));
    }

    #[tokio::test]
    async fn test_execute_fetch_error_handling() {
        let code = r#"
            export default function handler() {
                    throw new Error("Test error");
            }
        "#;

        let mut runtime = AppRuntime::new(
            "test_app".to_string(),
            code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            Vec::new(),
            30_000,
            1000.0,
        )
        .unwrap();

        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        let result = runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await;

        assert!(result.is_ok());

        let result_data = rx.await.unwrap().unwrap();
        let (status, _headers, body) = result_data;
        assert_eq!(status, 500);
        assert!(body.unwrap().contains("Test error"));
    }

    #[tokio::test]
    async fn test_web_api_console_url_timers() {
        let code = r#"
            export default async function handler(ctx) {
                    console.log("web api ok");
                    const url = new URL(ctx.request.url);
                    await new Promise((resolve) => setTimeout(resolve, 1));
                    return new Response(url.pathname, {
                        status: 200,
                        headers: { "content-type": "text/plain" }
                    });
            }
        "#;

        let mut runtime = AppRuntime::new(
            "test_app".to_string(),
            code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            Vec::new(),
            30_000,
            1000.0,
        )
        .unwrap();

        runtime.init_module().await.unwrap();

        let mut headers = HashMap::new();
        headers.insert("host".to_string(), "localhost".to_string());

        let (tx, rx) = oneshot::channel();
        let result = runtime
            .execute_fetch(
                "http://localhost/test".to_string(),
                "GET".to_string(),
                headers,
                None,
                tx,
            )
            .await;

        assert!(result.is_ok());

        let result_data = rx.await.unwrap().unwrap();
        let (status, _headers, body) = result_data;
        assert_eq!(status, 200);
        assert_eq!(body, Some("/test".to_string()));
    }

    #[tokio::test]
    async fn test_execute_fetch_populates_route_params() {
        let code = r#"
            export default function handler(context) {
                return new Response(JSON.stringify(context.params), {
                    status: 200,
                    headers: { "content-type": "application/json" }
                });
            }
        "#;

        let mut runtime = AppRuntime::new(
            "test_params_app".to_string(),
            code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            vec!["/users/:id".to_string()],
            30_000,
            1000.0,
        )
        .unwrap();

        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        let result = runtime
            .execute_fetch(
                "http://localhost/users/123".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await;

        assert!(result.is_ok());

        let result_data = rx.await.unwrap().unwrap();
        let (status, _headers, body) = result_data;
        assert_eq!(status, 200);
        assert_eq!(body, Some(r#"{"id":"123"}"#.to_string()));
    }

    #[tokio::test]
    async fn test_execute_fetch_without_matching_route_gives_empty_params() {
        let code = r#"
            export default function handler(context) {
                return new Response(JSON.stringify(context.params));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "test_no_params_app".to_string(),
            code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            vec!["/users/:id".to_string()],
            30_000,
            1000.0,
        )
        .unwrap();

        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        let result = runtime
            .execute_fetch(
                "http://localhost/posts/1".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await;

        assert!(result.is_ok());

        let result_data = rx.await.unwrap().unwrap();
        let (_status, _headers, body) = result_data;
        assert_eq!(body, Some("{}".to_string()));
    }

    #[test]
    fn test_new_stores_configured_timeout_and_rate_limit_overrides() {
        let code = r#"
            export default function handler() {
                return new Response("ok");
            }
        "#;

        let runtime = AppRuntime::new(
            "test_limits_override_app".to_string(),
            code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            Vec::new(),
            5_000,
            42.5,
        )
        .unwrap();

        // Both values differ from the platform defaults (30_000ms /
        // 1000.0 ops/sec) used elsewhere in this file, so this only passes
        // if the values actually threaded through `AppRuntime::new` rather
        // than falling back to a hard-coded default.
        assert_eq!(runtime.timeout_ms(), 5_000);
        assert_eq!(runtime.rate_limit_ops_per_sec(), 42.5);
    }

    #[test]
    fn test_new_stores_platform_default_limits_when_not_overridden() {
        let code = r#"
            export default function handler() {
                return new Response("ok");
            }
        "#;

        let runtime = AppRuntime::new(
            "test_default_limits_app".to_string(),
            code.to_string(),
            None,
            None,
            HashMap::new(),
            AppPermissions::default(),
            None,
            None,
            Vec::new(),
            30_000,
            1000.0,
        )
        .unwrap();

        assert_eq!(runtime.timeout_ms(), 30_000);
        assert_eq!(runtime.rate_limit_ops_per_sec(), 1000.0);
    }
}
