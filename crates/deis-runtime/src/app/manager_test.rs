// Application Manager unit tests

#[cfg(test)]
// Holding the std MutexGuard across await points is intentional here: the global
// TEST_MANAGER mutex exists precisely to serialize these tests against a shared
// singleton ApplicationManager. Tests are independent tokio runtimes, so there is
// no lock-order inversion, and dropping the guard early would defeat the purpose.
#[allow(clippy::await_holding_lock)]
mod tests {
    use crate::app::{
        types::{AppConfig, AppState, ResourceLimits},
        ApplicationManager,
    };
    use once_cell::sync::Lazy;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // Use global singleton ApplicationManager to avoid multiple RuntimePool creations
    static TEST_MANAGER: Lazy<Mutex<ApplicationManager>> =
        Lazy::new(|| Mutex::new(ApplicationManager::with_threads(Some(2))));

    #[tokio::test]
    async fn test_load_app() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            export default function handler(context) {
                return new Response("test app");
            }
        "#;

        let result = manager
            .load_app(
                "test1".to_string(),
                code.to_string(),
                AppConfig::default(),
                None,
            )
            .await;

        assert!(result.is_ok());

        let app = manager.get_app("test1");
        assert!(app.is_some());

        let app = app.unwrap();
        assert_eq!(app.id, "test1");
        assert_eq!(app.state, AppState::Running);

        // Cleanup
        let _ = manager.unload_app("test1").await;
    }

    #[tokio::test]
    async fn test_load_duplicate_app() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            export default function handler(context) {
                return new Response("test");
            }
        "#;

        manager
            .load_app(
                "dup".to_string(),
                code.to_string(),
                AppConfig::default(),
                None,
            )
            .await
            .unwrap();

        let result = manager
            .load_app(
                "dup".to_string(),
                code.to_string(),
                AppConfig::default(),
                None,
            )
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));

        // Cleanup
        let _ = manager.unload_app("dup").await;
    }

    #[tokio::test]
    async fn test_unload_app() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            export default function handler(context) {
                return new Response("test");
            }
        "#;

        manager
            .load_app(
                "test2".to_string(),
                code.to_string(),
                AppConfig::default(),
                None,
            )
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
            export default function handler(context) {
                return new Response("version 1");
            }
        "#;

        manager
            .load_app(
                "test3".to_string(),
                code1.to_string(),
                AppConfig::default(),
                None,
            )
            .await
            .unwrap();

        let code2 = r#"
            export default function handler(context) {
                return new Response("version 2");
            }
        "#;

        let result = manager
            .update_app("test3", code2.to_string(), None, None)
            .await;
        assert!(result.is_ok());

        let app = manager.get_app("test3").unwrap();
        assert_eq!(app.state, AppState::Running);

        // Cleanup
        let _ = manager.unload_app("test3").await;
    }

    #[tokio::test]
    async fn test_execute_app() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            export default function handler(context) {
                const url = new URL(context.request.url);
                return new Response(`Path: ${url.pathname}`);
            }
        "#;

        manager
            .load_app(
                "test4".to_string(),
                code.to_string(),
                AppConfig::default(),
                None,
            )
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

        // Cleanup
        let _ = manager.unload_app("test4").await;
    }

    #[tokio::test]
    async fn test_list_apps() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            export default function handler(context) {
                return new Response("test");
            }
        "#;

        manager
            .load_app(
                "app1".to_string(),
                code.to_string(),
                AppConfig::default(),
                None,
            )
            .await
            .unwrap();

        manager
            .load_app(
                "app2".to_string(),
                code.to_string(),
                AppConfig::default(),
                None,
            )
            .await
            .unwrap();

        let apps = manager.list_apps();
        assert!(apps.len() >= 2);

        let ids: Vec<String> = apps.iter().map(|a| a.id.clone()).collect();
        assert!(ids.contains(&"app1".to_string()));
        assert!(ids.contains(&"app2".to_string()));

        // Cleanup
        let _ = manager.unload_app("app1").await;
        let _ = manager.unload_app("app2").await;
    }

    #[tokio::test]
    async fn test_get_stats() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            export default function handler(context) {
                return new Response("test");
            }
        "#;

        manager
            .load_app(
                "s1".to_string(),
                code.to_string(),
                AppConfig::default(),
                None,
            )
            .await
            .unwrap();

        manager
            .load_app(
                "s2".to_string(),
                code.to_string(),
                AppConfig::default(),
                None,
            )
            .await
            .unwrap();

        let stats = manager.get_stats().await;
        assert!(stats["total"].as_u64().unwrap() >= 2);
        assert!(stats["running"].as_u64().unwrap() >= 2);

        // Cleanup
        let _ = manager.unload_app("s1").await;
        let _ = manager.unload_app("s2").await;
    }

    #[tokio::test]
    #[ignore] // KV now requires remote gRPC
    async fn test_app_with_kv() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            export default async function handler(context) {
                const url = new URL(context.request.url);

                if (url.pathname === "/set") {
                    await context.services.kv.put("counter", "42");
                    return new Response("set");
                }

                if (url.pathname === "/get") {
                    const val = await context.services.kv.get("counter");
                    return new Response(val || "not found");
                }

                return new Response("ok");
            }
        "#;

        manager
            .load_app(
                "kvtest".to_string(),
                code.to_string(),
                AppConfig::default(),
                None,
            )
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

        // Cleanup
        let _ = manager.unload_app("kvtest").await;
    }

    #[tokio::test]
    async fn test_execute_app_populates_route_params_end_to_end() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            export default function handler(context) {
                return new Response(JSON.stringify(context.params));
            }
        "#;

        let config = AppConfig {
            routes: vec!["/users/:id".to_string()],
            ..Default::default()
        };

        manager
            .load_app("params_app".to_string(), code.to_string(), config, None)
            .await
            .unwrap();

        let result = manager
            .execute_app(
                "params_app",
                "http://localhost/users/42".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await;

        assert!(result.is_ok());
        let (status, _headers, body) = result.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, Some(r#"{"id":"42"}"#.to_string()));

        // Cleanup
        let _ = manager.unload_app("params_app").await;
    }

    #[test]
    fn test_effective_limits_uses_platform_default_when_app_has_none() {
        let manager = ApplicationManager::with_threads(Some(1));
        let (timeout_ms, rate_limit) = manager.effective_limits("app_a", None);
        assert_eq!(timeout_ms, manager.default_limits.timeout_ms);
        assert_eq!(rate_limit, manager.default_limits.rate_limit_ops_per_sec);
    }

    #[test]
    fn test_effective_limits_applies_per_app_override() {
        let manager = ApplicationManager::with_threads(Some(1));
        let limits = ResourceLimits {
            memory_mb: 0,
            cpu_milli: 0,
            resource_table_max: None,
            timeout_ms: Some(5_000),
            rate_limit_ops_per_sec: Some(10.0),
        };
        let (timeout_ms, rate_limit) = manager.effective_limits("app_b", Some(&limits));
        assert_eq!(timeout_ms, 5_000);
        assert_eq!(rate_limit, 10.0);
    }

    #[test]
    fn test_effective_limits_falls_back_per_field() {
        // An app may override only one of the two limits; the other should
        // still fall back to the platform default rather than, say, zero.
        let manager = ApplicationManager::with_threads(Some(1));
        let limits = ResourceLimits {
            memory_mb: 0,
            cpu_milli: 0,
            resource_table_max: None,
            timeout_ms: Some(5_000),
            rate_limit_ops_per_sec: None,
        };
        let (timeout_ms, rate_limit) = manager.effective_limits("app_c", Some(&limits));
        assert_eq!(timeout_ms, 5_000);
        assert_eq!(rate_limit, manager.default_limits.rate_limit_ops_per_sec);
    }
}
