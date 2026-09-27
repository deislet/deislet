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
        AppKey, ApplicationManager,
    };
    use once_cell::sync::Lazy;
    use std::sync::{Arc, Mutex};

    /// Everything `emit` writes to `tracing`, message and fields together, as
    /// one string per event.
    ///
    /// A layer of its own rather than the shipper's [`crate::logging::layer`]:
    /// that one keeps `target: "app"` events, and what has to be read here is
    /// a line the node writes about itself. What the check needs is only
    /// "did this text appear anywhere in what was logged", so the visitor
    /// flattens the whole event and the assertion searches it.
    fn logged(emit: impl FnOnce()) -> Vec<String> {
        use std::fmt::Write as _;
        use tracing_subscriber::prelude::*;

        #[derive(Default)]
        struct Flatten(String);
        impl tracing::field::Visit for Flatten {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let _ = write!(self.0, " {}={:?}", field.name(), value);
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                let _ = write!(self.0, " {}={}", field.name(), value);
            }
        }

        struct Collector(Arc<Mutex<Vec<String>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Collector {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut flat = Flatten::default();
                event.record(&mut flat);
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(flat.0);
            }
        }

        let lines = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collector(lines.clone()));
        tracing::subscriber::with_default(subscriber, emit);
        let taken = lines.lock().unwrap_or_else(|e| e.into_inner()).clone();
        taken
    }

    // Use global singleton ApplicationManager to avoid multiple RuntimePool creations
    static TEST_MANAGER: Lazy<Mutex<ApplicationManager>> =
        Lazy::new(|| Mutex::new(ApplicationManager::with_threads(Some(2))));

    fn key(app_id: &str) -> AppKey {
        AppKey::production(app_id)
    }

    #[tokio::test]
    async fn test_load_app() {
        let manager = TEST_MANAGER.lock().unwrap();

        let code = r#"
            export default function handler(context) {
                return new Response("test app");
            }
        "#;

        let result = manager
            .load_app(key("test1"), code.to_string(), AppConfig::default(), None)
            .await;

        assert!(result.is_ok());

        let app = manager.get_app(&key("test1"));
        assert!(app.is_some());

        let app = app.unwrap();
        assert_eq!(app.id, "test1");
        assert_eq!(app.environment, "production");
        assert_eq!(app.state, AppState::Running);

        // Cleanup
        let _ = manager.unload_app(&key("test1")).await;
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
            .load_app(key("dup"), code.to_string(), AppConfig::default(), None)
            .await
            .unwrap();

        let result = manager
            .load_app(key("dup"), code.to_string(), AppConfig::default(), None)
            .await;

        assert!(result.is_err());

        // Cleanup
        let _ = manager.unload_app(&key("dup")).await;
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
            .load_app(key("test2"), code.to_string(), AppConfig::default(), None)
            .await
            .unwrap();

        assert!(manager.get_app(&key("test2")).is_some());

        let result = manager.unload_app(&key("test2")).await;
        assert!(result.is_ok());

        assert!(manager.get_app(&key("test2")).is_none());
    }

    /// The identity of a running application is (app, environment).
    ///
    /// Before this was true, deploying a preview of an app replaced its
    /// production copy - same code slot, same secrets - and nothing said so.
    #[tokio::test]
    async fn two_environments_of_one_app_run_side_by_side() {
        let manager = TEST_MANAGER.lock().unwrap();

        let production = AppKey::new("two-envs", "production");
        let preview = AppKey::new("two-envs", "preview-1");

        for (target, body) in [(&production, "from production"), (&preview, "from preview")] {
            let code = format!(
                r#"
                export default function handler(context) {{
                    return new Response("{body}");
                }}
                "#
            );
            manager
                .load_app(target.clone(), code, AppConfig::default(), None)
                .await
                .unwrap_or_else(|e| panic!("loading {target} failed: {e}"));
        }

        // Both are loaded, and each answers with its own code.
        async fn answer(manager: &ApplicationManager, target: &AppKey) -> String {
            let (_status, _headers, body) = manager
                .execute_app(
                    target,
                    "http://localhost/".to_string(),
                    "GET".to_string(),
                    Vec::new(),
                    None,
                )
                .await
                .expect("the app is loaded");
            String::from_utf8(body.into_bytes().unwrap_or_default())
                .expect("the handler answers text")
        }
        assert_eq!(answer(&manager, &production).await, "from production");
        assert_eq!(answer(&manager, &preview).await, "from preview");

        let mut environments = manager.environments_of("two-envs");
        environments.sort();
        assert_eq!(environments, vec![preview.clone(), production.clone()]);

        // Unloading one leaves the other serving.
        manager.unload_app(&preview).await.unwrap();
        assert!(manager.get_app(&preview).is_none());
        assert_eq!(answer(&manager, &production).await, "from production");

        // Cleanup
        let _ = manager.unload_app(&production).await;
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
            .load_app(key("test4"), code.to_string(), AppConfig::default(), None)
            .await
            .unwrap();

        let result = manager
            .execute_app(
                &key("test4"),
                "http://localhost/test/path".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await;

        assert!(result.is_ok());

        let (status, _headers, body) = result.unwrap();
        assert_eq!(status, 200);
        assert!(String::from_utf8(body.into_bytes().unwrap())
            .unwrap()
            .contains("/test/path"));

        // Cleanup
        let _ = manager.unload_app(&key("test4")).await;
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
            .load_app(key("app1"), code.to_string(), AppConfig::default(), None)
            .await
            .unwrap();

        manager
            .load_app(key("app2"), code.to_string(), AppConfig::default(), None)
            .await
            .unwrap();

        let apps = manager.list_apps();
        assert!(apps.len() >= 2);

        let ids: Vec<String> = apps.iter().map(|a| a.id.clone()).collect();
        assert!(ids.contains(&"app1".to_string()));
        assert!(ids.contains(&"app2".to_string()));

        // Cleanup
        let _ = manager.unload_app(&key("app1")).await;
        let _ = manager.unload_app(&key("app2")).await;
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
            .load_app(key("s1"), code.to_string(), AppConfig::default(), None)
            .await
            .unwrap();

        manager
            .load_app(key("s2"), code.to_string(), AppConfig::default(), None)
            .await
            .unwrap();

        let stats = manager.get_stats().await;
        assert!(stats["total"].as_u64().unwrap() >= 2);
        assert!(stats["running"].as_u64().unwrap() >= 2);

        // Cleanup
        let _ = manager.unload_app(&key("s1")).await;
        let _ = manager.unload_app(&key("s2")).await;
    }

    /// KV through the manager, not through `AppRuntime` directly.
    ///
    /// This was `#[ignore]`d with the note "KV now requires remote gRPC",
    /// which was true and left the path untested: the manager has no wiring
    /// parameter, so the store's address has to ride in the deployment's env
    /// map exactly the way the control plane sends it, and nothing checked
    /// that the pool still lifted it back out on the way to the isolate.
    #[tokio::test]
    async fn test_app_with_kv() {
        let store = crate::runtime::test_store::TestStore::start().await;
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

        let config = AppConfig {
            env: {
                let mut env = std::collections::HashMap::new();
                store.kv_wiring_for("kvtest.production").write_to_env(&mut env);
                env
            },
            ..AppConfig::default()
        };
        manager
            .load_app(key("kvtest"), code.to_string(), config, None)
            .await
            .unwrap();

        // Set value
        let (_status, _headers, _body) = manager
            .execute_app(
                &key("kvtest"),
                "http://localhost/set".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .unwrap();

        // Get value
        let (_status, _headers, body) = manager
            .execute_app(
                &key("kvtest"),
                "http://localhost/get".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(body.into_bytes().as_deref(), Some(&b"42"[..]));

        // Cleanup
        let _ = manager.unload_app(&key("kvtest")).await;
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
            .load_app(key("params_app"), code.to_string(), config, None)
            .await
            .unwrap();

        let result = manager
            .execute_app(
                &key("params_app"),
                "http://localhost/users/42".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await;

        assert!(result.is_ok());
        let (status, _headers, body) = result.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body.into_bytes().as_deref(), Some(&br#"{"id":"42"}"#[..]));

        // Cleanup
        let _ = manager.unload_app(&key("params_app")).await;
    }

    #[test]
    fn test_effective_limits_uses_platform_default_when_app_has_none() {
        let manager = ApplicationManager::with_threads(Some(1));
        let effective = manager.effective_limits(&key("app_a"), None);
        assert_eq!(effective, manager.default_limits);
    }

    #[test]
    fn test_effective_limits_applies_per_app_override() {
        let manager = ApplicationManager::with_threads(Some(1));
        let limits = ResourceLimits {
            memory_mb: 0,
            resource_table_max: None,
            timeout_ms: Some(5_000),
            rate_limit_ops_per_sec: Some(10.0),
            cpu_time_limit_ms: None,
            max_subrequests: None,
            max_outbound_connections: None,
        };
        let effective = manager.effective_limits(&key("app_b"), Some(&limits));
        assert_eq!(effective.timeout_ms, 5_000);
        assert_eq!(effective.rate_limit_ops_per_sec, 10.0);
    }

    #[test]
    fn test_effective_limits_falls_back_per_field() {
        // An app may override only one of the two limits; the other should
        // still fall back to the platform default rather than, say, zero.
        let manager = ApplicationManager::with_threads(Some(1));
        let limits = ResourceLimits {
            memory_mb: 0,
            resource_table_max: None,
            timeout_ms: Some(5_000),
            rate_limit_ops_per_sec: None,
            cpu_time_limit_ms: None,
            max_subrequests: None,
            max_outbound_connections: None,
        };
        let effective = manager.effective_limits(&key("app_c"), Some(&limits));
        assert_eq!(effective.timeout_ms, 5_000);
        assert_eq!(
            effective.rate_limit_ops_per_sec,
            manager.default_limits.rate_limit_ops_per_sec
        );
    }

    /// The inspector session token is the whole authorisation for attaching a
    /// debugger to a tenant's isolate, and a debugger reads that tenant's
    /// decrypted environment secrets and the bodies of requests it is serving.
    /// It used to be written into the node's log at INFO, where it reaches the
    /// node's stdout, the log buffer, and -- for anyone holding the telemetry
    /// channel token -- deis-telemetry, none of which is where a credential
    /// may be.
    #[tokio::test]
    async fn a_debug_session_never_writes_its_token_to_the_log() {
        let manager = TEST_MANAGER.lock().unwrap();
        let code = r#"export default function handler() { return new Response("ok"); }"#;
        manager
            .load_app(
                key("inspect_secrecy"),
                code.to_string(),
                AppConfig::default(),
                None,
            )
            .await
            .unwrap();

        let mut minted = String::new();
        let lines = logged(|| {
            let (_, token) = manager
                .start_inspect(key("inspect_secrecy"), 60)
                .expect("the session starts");
            minted = token;
        });

        assert!(!minted.is_empty(), "the session has to mint a token at all");
        assert!(
            !lines.is_empty(),
            "the node has to say a debug session was started"
        );
        for line in &lines {
            assert!(
                !line.contains(&minted),
                "the session token reached the log: {line}"
            );
        }

        // Cleanup
        let _ = manager.unload_app(&key("inspect_secrecy")).await;
    }

    /// A token that is not the minted one is refused however many of its
    /// leading bytes are right.
    #[tokio::test]
    async fn a_debug_token_is_only_accepted_when_it_is_the_whole_token() {
        let manager = TEST_MANAGER.lock().unwrap();
        let code = r#"export default function handler() { return new Response("ok"); }"#;
        manager
            .load_app(
                key("inspect_compare"),
                code.to_string(),
                AppConfig::default(),
                None,
            )
            .await
            .unwrap();

        let (_, token) = manager
            .start_inspect(key("inspect_compare"), 60)
            .expect("the session starts");

        assert!(manager.validate_inspect_token(&key("inspect_compare"), &token));
        assert!(!manager.validate_inspect_token(&key("inspect_compare"), &token[..token.len() - 1]));
        assert!(!manager.validate_inspect_token(&key("inspect_compare"), &format!("{token}x")));
        assert!(!manager.validate_inspect_token(&key("inspect_compare"), ""));
        assert!(
            !manager.validate_inspect_token(&key("no_such_session"), &token),
            "a token is for one application's session, not for the node"
        );

        // Cleanup
        let _ = manager.unload_app(&key("inspect_compare")).await;
    }
}
