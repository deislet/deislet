// Application Manager 单元测试

#[cfg(test)]
mod tests {
    use crate::app::{ApplicationManager, types::{AppConfig, AppState}};
    use std::collections::HashMap;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    // 使用全局单例 ApplicationManager 避免多次创建 RuntimePool
    static TEST_MANAGER: Lazy<Mutex<ApplicationManager>> = Lazy::new(|| {
        Mutex::new(ApplicationManager::with_threads(Some(2)))
    });

    #[tokio::test]
    async fn test_load_app() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            globalThis.default = {
                fetch(request) {
                    return new Response("test app");
                }
            }
        "#;

        let result = manager
            .load_app("test1".to_string(), code.to_string(), AppConfig::default())
            .await;

        assert!(result.is_ok());

        let app = manager.get_app("test1");
        assert!(app.is_some());

        let app = app.unwrap();
        assert_eq!(app.id, "test1");
        assert_eq!(app.state, AppState::Running);

        // 清理
        let _ = manager.unload_app("test1").await;
    }

    #[tokio::test]
    async fn test_load_duplicate_app() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            globalThis.default = {
                fetch(request) {
                    return new Response("test");
                }
            }
        "#;

        manager
            .load_app("dup".to_string(), code.to_string(), AppConfig::default())
            .await
            .unwrap();

        let result = manager
            .load_app("dup".to_string(), code.to_string(), AppConfig::default())
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));

        // 清理
        let _ = manager.unload_app("dup").await;
    }

    #[tokio::test]
    async fn test_unload_app() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            globalThis.default = {
                fetch(request) {
                    return new Response("test");
                }
            }
        "#;

        manager
            .load_app("test2".to_string(), code.to_string(), AppConfig::default())
            .await
            .unwrap();

        assert!(manager.get_app("test2").is_some());

        let result = manager.unload_app("test2").await;
        assert!(result.is_ok());

        assert!(manager.get_app("test2").is_none());
    }

    #[tokio::test]
    async fn test_update_app() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code1 = r#"
            globalThis.default = {
                fetch(request) {
                    return new Response("version 1");
                }
            }
        "#;

        manager
            .load_app("test3".to_string(), code1.to_string(), AppConfig::default())
            .await
            .unwrap();

        let code2 = r#"
            globalThis.default = {
                fetch(request) {
                    return new Response("version 2");
                }
            }
        "#;

        let result = manager.update_app("test3", code2.to_string(), None).await;
        assert!(result.is_ok());

        let app = manager.get_app("test3").unwrap();
        assert_eq!(app.state, AppState::Running);

        // 清理
        let _ = manager.unload_app("test3").await;
    }

    #[tokio::test]
    async fn test_execute_app() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            globalThis.default = {
                fetch(request) {
                    const url = new URL(request.url);
                    return new Response(`Path: ${url.pathname}`);
                }
            }
        "#;

        manager
            .load_app("test4".to_string(), code.to_string(), AppConfig::default())
            .await
            .unwrap();

        let result = manager
            .execute_app(
                "test4",
                "http://localhost/test/path".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await;

        assert!(result.is_ok());

        let (status, _headers, body) = result.unwrap();
        assert_eq!(status, 200);
        assert!(body.unwrap().contains("/test/path"));

        // 清理
        let _ = manager.unload_app("test4").await;
    }

    #[tokio::test]
    async fn test_list_apps() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            globalThis.default = {
                fetch(request) {
                    return new Response("test");
                }
            }
        "#;

        manager
            .load_app("app1".to_string(), code.to_string(), AppConfig::default())
            .await
            .unwrap();

        manager
            .load_app("app2".to_string(), code.to_string(), AppConfig::default())
            .await
            .unwrap();

        let apps = manager.list_apps();
        assert!(apps.len() >= 2);

        let ids: Vec<String> = apps.iter().map(|a| a.id.clone()).collect();
        assert!(ids.contains(&"app1".to_string()));
        assert!(ids.contains(&"app2".to_string()));

        // 清理
        let _ = manager.unload_app("app1").await;
        let _ = manager.unload_app("app2").await;
    }

    #[tokio::test]
    async fn test_get_stats() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            globalThis.default = {
                fetch(request) {
                    return new Response("test");
                }
            }
        "#;

        manager
            .load_app("s1".to_string(), code.to_string(), AppConfig::default())
            .await
            .unwrap();

        manager
            .load_app("s2".to_string(), code.to_string(), AppConfig::default())
            .await
            .unwrap();

        let stats = manager.get_stats().await;
        assert!(stats["total"].as_u64().unwrap() >= 2);
        assert!(stats["running"].as_u64().unwrap() >= 2);

        // 清理
        let _ = manager.unload_app("s1").await;
        let _ = manager.unload_app("s2").await;
    }

    #[tokio::test]
    async fn test_app_with_kv() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            globalThis.default = {
                async fetch(request, env) {
                    const url = new URL(request.url);

                    if (url.pathname === "/set") {
                        await env.kv.set("counter", "42");
                        return new Response("set");
                    }

                    if (url.pathname === "/get") {
                        const val = await env.kv.get("counter");
                        return new Response(val || "not found");
                    }

                    return new Response("ok");
                }
            }
        "#;

        manager
            .load_app("kvtest".to_string(), code.to_string(), AppConfig::default())
            .await
            .unwrap();

        // Set value
        let (_status, _headers, _body) = manager
            .execute_app(
                "kvtest",
                "http://localhost/set".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await
            .unwrap();

        // Get value
        let (_status, _headers, body) = manager
            .execute_app(
                "kvtest",
                "http://localhost/get".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(body, Some("42".to_string()));

        // 清理
        let _ = manager.unload_app("kvtest").await;
    }
}
