// App Runtime Isolate 单元测试

#[cfg(test)]
mod tests {
    use crate::runtime::isolate::AppRuntime;
    use std::collections::HashMap;

    #[test]
    fn test_transform_esm_typescript() {
        let ts_code = r#"export default {
  async fetch(_request: Request): Promise<Response> {
    return new Response("Hello from Denix!", {
      status: 200,
      headers: {
        "Content-Type": "text/plain",
      },
    });
  },
};"#;

        let result = AppRuntime::new("test_esm".to_string(), ts_code.to_string());
        assert!(result.is_ok(), "Failed to parse TypeScript ESM: {:?}", result.err());
    }

    #[test]
    fn test_transform_counter_app() {
        let ts_code = r#"export default {
  async fetch(request: Request, env: any): Promise<Response> {
    const url = new URL(request.url);
    return new Response("ok");
  },
};"#;

        let result = AppRuntime::new("test_counter".to_string(), ts_code.to_string());
        assert!(result.is_ok(), "Failed to parse counter app: {:?}", result.err());
    }

    #[test]
    fn test_transform_with_interface() {
        let ts_code = r#"interface Todo {
  id: string;
  title: string;
}

export default {
  async fetch(request: Request, env: any): Promise<Response> {
    const todos: Todo[] = [];
    return new Response(JSON.stringify(todos));
  },
};"#;

        let result = AppRuntime::new("test_interface".to_string(), ts_code.to_string());
        assert!(result.is_ok(), "Failed to parse TypeScript with interface: {:?}", result.err());
    }

    #[test]
    fn test_transform_echo_app() {
        let ts_code = r#"export default {
  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const info: any = {
      method: request.method,
      url: request.url,
      path: url.pathname,
    };
    return new Response(JSON.stringify(info));
  },
};"#;

        let result = AppRuntime::new("test_echo".to_string(), ts_code.to_string());
        assert!(result.is_ok(), "Failed to parse echo app: {:?}", result.err());
    }

    #[test]
    fn test_create_runtime_with_valid_code() {
        let code = r#"
            const message = "Hello";
            globalThis.default = {
                fetch() {
                    return new Response(message);
                }
            }
        "#;

        let result = AppRuntime::new("test_app".to_string(), code.to_string());
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_runtime_with_invalid_code() {
        let invalid_code = "this is not valid javascript {{{";
        let result = AppRuntime::new("test_app".to_string(), invalid_code.to_string());
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_create_runtime() {
        let code = r#"
            globalThis.default = {
                fetch(request) {
                    return new Response("test");
                }
            }
        "#;

        let result = AppRuntime::new("test_app".to_string(), code.to_string());
        assert!(result.is_ok());

        let runtime = result.unwrap();
        assert_eq!(runtime.app_id(), "test_app");
    }

    #[tokio::test]
    async fn test_execute_fetch() {
        let code = r#"
            globalThis.default = {
                fetch(request) {
                    return new Response("Hello from test", {
                        status: 200,
                        headers: { "content-type": "text/plain" }
                    });
                }
            }
        "#;

        let mut runtime = AppRuntime::new("test_app".to_string(), code.to_string()).unwrap();

        let mut headers = HashMap::new();
        headers.insert("host".to_string(), "localhost".to_string());

        let result = runtime
            .execute_fetch(
                "http://localhost/test".to_string(),
                "GET".to_string(),
                headers,
                None,
            )
            .await;

        assert!(result.is_ok());

        let (status, _headers, body) = result.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, Some("Hello from test".to_string()));
    }

    #[tokio::test]
    async fn test_execute_fetch_with_kv() {
        let code = r#"
            globalThis.default = {
                async fetch(request, env) {
                    await env.kv.set("test", "value123");
                    const val = await env.kv.get("test");
                    return new Response(val);
                }
            }
        "#;

        let mut runtime = AppRuntime::new("test_app".to_string(), code.to_string()).unwrap();

        let result = runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await;

        assert!(result.is_ok());

        let (_status, _headers, body) = result.unwrap();
        assert_eq!(body, Some("value123".to_string()));
    }

    #[tokio::test]
    async fn test_execute_fetch_error_handling() {
        let code = r#"
            globalThis.default = {
                fetch(request) {
                    throw new Error("Test error");
                }
            }
        "#;

        let mut runtime = AppRuntime::new("test_app".to_string(), code.to_string()).unwrap();

        let result = runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await;

        assert!(result.is_ok());

        let (status, _headers, body) = result.unwrap();
        assert_eq!(status, 500);
        assert!(body.unwrap().contains("Test error"));
    }
}
