#[cfg(test)]
mod tests {
    use crate::config::ExecutionLimits;
    use crate::runtime::isolate::{AppRuntime, IsolateSpec};
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
            IsolateSpec::default(),
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
            IsolateSpec::default(),
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
            IsolateSpec::default(),
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
            IsolateSpec::default(),
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
            IsolateSpec::default(),
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
            IsolateSpec::default(),
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
            IsolateSpec::default(),
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
            IsolateSpec::default(),
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
            IsolateSpec::default(),
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
            IsolateSpec::default(),
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
            IsolateSpec::default(),
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
            IsolateSpec {
                route_patterns: vec!["/users/:id".to_string()],
                ..Default::default()
            },
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
            IsolateSpec {
                route_patterns: vec!["/users/:id".to_string()],
                ..Default::default()
            },
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
            IsolateSpec {
                limits: ExecutionLimits {
                    timeout_ms: 5_000,
                    rate_limit_ops_per_sec: 42.5,
                },
                ..Default::default()
            },
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
            IsolateSpec::default(),
        )
        .unwrap();

        assert_eq!(runtime.timeout_ms(), 30_000);
        assert_eq!(runtime.rate_limit_ops_per_sec(), 1000.0);
    }

    /// A `console.log` from an application must reach deis-telemetry tagged
    /// with that application. Untagged, the entry is stored but no per-app
    /// query can ever surface it, so `deis tail my-app` stays empty while the
    /// shipper reports success.
    #[tokio::test]
    async fn console_log_is_attributed_to_the_application() {
        use crate::logging::{LogBuffer, RuntimeLayer};
        use std::sync::Arc;
        use tracing_subscriber::prelude::*;

        let code = r#"
            export default function handler() {
                console.log("hello from the app");
                return new Response("ok");
            }
        "#;

        let buffer = Arc::new(LogBuffer::new(16));
        let subscriber = tracing_subscriber::registry()
            .with(RuntimeLayer::new(buffer.clone(), "node-a".to_string()));
        // `set_default` rather than `with_default`: the guard has to stay alive
        // across the awaits below, which a closure-scoped default cannot do.
        let _guard = tracing::subscriber::set_default(subscriber);

        let mut runtime = AppRuntime::new(
            "log_attribution_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
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
        rx.await.unwrap().unwrap();

        let entries = buffer.drain(16);
        let logged = entries
            .iter()
            .find(|e| e.message.contains("hello from the app"))
            .expect("the app's console.log should have been buffered");
        assert_eq!(
            logged.app_id.as_deref(),
            Some("log_attribution_app"),
            "an app log with no app_id is unqueryable in deis-telemetry"
        );
    }

    /// The console bridge has to preserve severity and render non-string
    /// arguments; a log that arrives at the wrong level defeats the `min_level`
    /// filter every telemetry query uses, and one that arrives as
    /// "[object Object]" carries no information at all.
    #[tokio::test]
    async fn console_levels_and_objects_survive_the_bridge() {
        use crate::logging::{LogBuffer, LogLevel, RuntimeLayer};
        use std::sync::Arc;
        use tracing_subscriber::prelude::*;

        let code = r#"
            export default function handler() {
                console.error("boom", { code: 7 });
                console.warn("careful");
                return new Response("ok");
            }
        "#;

        let buffer = Arc::new(LogBuffer::new(16));
        let subscriber = tracing_subscriber::registry()
            .with(RuntimeLayer::new(buffer.clone(), "node-a".to_string()));
        let _guard = tracing::subscriber::set_default(subscriber);

        let mut runtime = AppRuntime::new(
            "console_levels_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
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
        rx.await.unwrap().unwrap();

        let entries = buffer.drain(16);
        let error = entries
            .iter()
            .find(|e| e.message.starts_with("boom"))
            .expect("console.error should have been buffered");
        assert_eq!(error.level, LogLevel::Error);
        assert_eq!(error.message, r#"boom {"code":7}"#);

        let warning = entries
            .iter()
            .find(|e| e.message == "careful")
            .expect("console.warn should have been buffered");
        assert_eq!(warning.level, LogLevel::Warn);
    }
}
