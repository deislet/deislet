//! REST surface of the product model: apps, versions, environments,
//! deployments, routes, secrets, and the two endpoints nodes use to work
//! out what they should be running (`assignments` and the bundle
//! download).
//!
//! Everything here is mounted inside the authenticated part of the API
//! router, nodes included: a node that fetches its assignments carries the
//! same API key as any other client.

pub mod apps;
pub mod assignments;
pub mod deployments;
pub mod environments;
pub mod routes;
pub mod secrets;
pub mod versions;

use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderMap, header},
    routing::{get, post, put},
};
use tracing::warn;

use crate::product::{BundleStore, ProductStore, SecretsBackend};

/// Largest bundle the control plane will accept in one upload. Bundles
/// carry the worker code plus every static asset, so this is generous;
/// what it stops is a client streaming an unbounded body into memory.
pub const MAX_BUNDLE_BYTES: usize = 64 * 1024 * 1024;

/// How the control plane names itself to nodes.
///
/// Nodes fetch bundles over HTTP from an absolute URL, so the assignment
/// list has to contain an address that is reachable from the node, not
/// from the control plane's own point of view.
#[derive(Debug, Clone)]
pub struct PublicAddress {
    /// Explicit `public_base_url` from the config. Always wins.
    pub configured: Option<String>,
    /// Scheme to pair with the request's Host header.
    pub scheme: &'static str,
    /// Last resort, derived from the bind address.
    pub fallback: String,
}

impl PublicAddress {
    /// Resolve the base URL bundle links are built from.
    ///
    /// Preference order: the configured value, then the `Host` header of
    /// the request being served (whatever the caller used to reach us is
    /// by definition reachable for them), then the bind address.
    pub fn resolve(&self, headers: &HeaderMap) -> String {
        if let Some(configured) = &self.configured {
            return configured.trim_end_matches('/').to_string();
        }
        if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok())
            && !host.is_empty()
        {
            return format!("{}://{}", self.scheme, host);
        }
        warn!(
            fallback = %self.fallback,
            "Request carried no usable Host header and public_base_url is unset; bundle URLs fall back to the bind address, which nodes on another host cannot reach"
        );
        self.fallback.clone()
    }
}

/// Shared state of every product endpoint.
#[derive(Clone)]
pub struct ProductState {
    pub store: Arc<ProductStore>,
    pub bundles: Arc<BundleStore>,
    pub secrets: Arc<SecretsBackend>,
    pub address: Arc<PublicAddress>,
}

/// Absolute URL a node uses to download one version's bundle.
pub fn bundle_url(base: &str, app_id: &str, version_id: &str) -> String {
    format!("{base}/api/apps/{app_id}/versions/{version_id}/bundle")
}

/// Build the product half of the API router. The caller merges it into the
/// authenticated router.
pub fn router(state: ProductState) -> Router {
    Router::new()
        .route("/api/apps", get(apps::list_apps).post(apps::create_app))
        .route(
            "/api/apps/:app_id",
            get(apps::get_app).delete(apps::delete_app),
        )
        .route(
            "/api/apps/:app_id/versions",
            get(versions::list_versions)
                .post(versions::upload_version)
                // Only this route accepts a multi-megabyte body.
                .layer(DefaultBodyLimit::max(MAX_BUNDLE_BYTES)),
        )
        .route(
            "/api/apps/:app_id/versions/:version_id",
            get(versions::get_version),
        )
        .route(
            "/api/apps/:app_id/versions/:version_id/bundle",
            get(versions::download_bundle),
        )
        .route(
            "/api/apps/:app_id/environments",
            get(environments::list_environments).post(environments::create_environment),
        )
        .route(
            "/api/apps/:app_id/environments/:env",
            get(environments::get_environment),
        )
        .route(
            "/api/apps/:app_id/environments/:env/deployment",
            get(deployments::current_deployment),
        )
        .route(
            "/api/apps/:app_id/environments/:env/deployments",
            get(deployments::list_deployments).post(deployments::deploy),
        )
        .route(
            "/api/apps/:app_id/environments/:env/rollback",
            post(deployments::rollback),
        )
        .route(
            "/api/apps/:app_id/environments/:env/routes",
            get(routes::list_routes).post(routes::create_route),
        )
        .route(
            "/api/apps/:app_id/environments/:env/routes/:route_id",
            put(routes::update_route).delete(routes::delete_route),
        )
        .route(
            "/api/apps/:app_id/environments/:env/secrets",
            get(secrets::list_secrets),
        )
        .route(
            "/api/apps/:app_id/environments/:env/secrets/:name",
            put(secrets::put_secret).delete(secrets::delete_secret),
        )
        .route("/api/routes", get(routes::list_all_routes))
        // The path parameter is named `:id` to match the node routes it
        // shares a prefix with; the router rejects two names in the same
        // position.
        .route("/api/nodes/:id/assignments", get(assignments::assignments))
        .with_state(state)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::database::Database;
    use crate::product::bundle::tests::sample_bundle;
    use crate::product::{SecretCipher, secrets::MASTER_KEY_LEN};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    pub(crate) struct Harness {
        pub router: Router,
        pub _bundle_dir: tempfile::TempDir,
    }

    pub(crate) async fn harness(secrets_ready: bool) -> Harness {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let bundle_dir = tempfile::tempdir().unwrap();
        let secrets = if secrets_ready {
            SecretsBackend::Ready(Box::new(
                SecretCipher::from_key_bytes(&[42u8; MASTER_KEY_LEN]).unwrap(),
            ))
        } else {
            SecretsBackend::Unavailable("no key file in this test".to_string())
        };
        let state = ProductState {
            store: Arc::new(ProductStore::new(database)),
            bundles: Arc::new(BundleStore::new(bundle_dir.path())),
            secrets: Arc::new(secrets),
            address: Arc::new(PublicAddress {
                configured: Some("http://control.test".to_string()),
                scheme: "http",
                fallback: "http://127.0.0.1:8080".to_string(),
            }),
        };
        Harness {
            router: router(state),
            _bundle_dir: bundle_dir,
        }
    }

    pub(crate) async fn call(
        router: &Router,
        method: &str,
        uri: &str,
        body: Body,
        content_type: &str,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", content_type)
            .body(body)
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), MAX_BUNDLE_BYTES)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    pub(crate) async fn json(
        router: &Router,
        method: &str,
        uri: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        // A null body means "no body at all", which is how the optional
        // request bodies are exercised.
        let body = if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        };
        call(router, method, uri, body, "application/json").await
    }

    /// The whole loop the acceptance criteria describe: create an app,
    /// upload a bundle, deploy it, roll back, and see the assignment list
    /// follow along.
    #[tokio::test]
    async fn full_deploy_and_rollback_loop() {
        let h = harness(true).await;
        let router = &h.router;

        let (status, _) = json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // Same name twice is a conflict, not a silent overwrite.
        let (status, _) = json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        let (status, body) = call(
            router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(sample_bundle("demo")),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "upload failed: {body}");
        let v1 = body["version"]["id"].as_str().unwrap().to_string();
        assert_eq!(body["version"]["number"], 1);
        assert_eq!(body["version"]["manifest"]["app"], "demo");

        // A second, different bundle becomes version 2.
        let bundle2 = crate::product::bundle::tests::bundle_with_manifest(
            r#"{"schema":1,"app":"demo","entry":"worker/index.js","env":{"LOG_LEVEL":"debug"}}"#,
            true,
        );
        let (status, body) = call(
            router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(bundle2),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let v2 = body["version"]["id"].as_str().unwrap().to_string();
        assert_eq!(body["version"]["number"], 2);

        let (status, body) = json(router, "GET", "/api/apps/demo/versions", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 2);

        // A bundle for another app must never land under this app.
        let (status, _) = call(
            router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(sample_bundle("someone-else")),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = json(
            router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // Nothing is deployed yet, so no assignment exists.
        let (status, body) =
            json(router, "GET", "/api/nodes/node-a/assignments", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 0);

        let (status, body) = json(
            router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": v1, "note": "first deploy"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "deploy failed: {body}");

        let (status, body) = json(
            router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_number": 2}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "deploy failed: {body}");
        assert_eq!(body["deployment"]["version_id"], v2);

        let (status, body) = json(
            router,
            "GET",
            "/api/apps/demo/environments/production/deployment",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["version"]["number"], 2);

        // The assignment list points at v2 with a downloadable bundle URL.
        let (status, body) =
            json(router, "GET", "/api/nodes/node-a/assignments", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let assignments = body.as_array().unwrap();
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0]["app_id"], "demo");
        assert_eq!(assignments[0]["environment"], "production");
        assert_eq!(assignments[0]["version_id"], v2);
        assert_eq!(
            assignments[0]["bundle_url"],
            format!("http://control.test/api/apps/demo/versions/{v2}/bundle")
        );
        assert!(assignments[0]["bundle_sha"].as_str().unwrap().len() == 64);

        // Rollback without a target goes back to the previous version and
        // leaves both deployment rows in place.
        let (status, body) = json(
            router,
            "POST",
            "/api/apps/demo/environments/production/rollback",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "rollback failed: {body}");
        assert_eq!(body["deployment"]["version_id"], v1);

        let (status, body) = json(
            router,
            "GET",
            "/api/apps/demo/environments/production/deployments",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 3, "rollback must append, never rewrite");

        let (status, body) =
            json(router, "GET", "/api/nodes/node-a/assignments", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["version_id"], v1);
    }

    #[tokio::test]
    async fn bundle_download_streams_the_stored_zip() {
        let h = harness(true).await;
        let router = &h.router;
        let bytes = sample_bundle("demo");

        json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let (_, body) = call(
            router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(bytes.clone()),
            "application/zip",
        )
        .await;
        let version_id = body["version"]["id"].as_str().unwrap().to_string();

        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/apps/demo/versions/{version_id}/bundle"))
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/zip"
        );
        let downloaded = to_bytes(response.into_body(), MAX_BUNDLE_BYTES)
            .await
            .unwrap();
        assert_eq!(downloaded.as_ref(), bytes.as_slice());
    }

    #[tokio::test]
    async fn routes_and_secrets_round_trip() {
        let h = harness(true).await;
        let router = &h.router;

        json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        json(
            router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;

        let (status, body) = json(
            router,
            "POST",
            "/api/apps/demo/environments/production/routes",
            serde_json::json!({"host": "example.com", "path_prefix": "/api", "priority": 10}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let route_id = body["route"]["id"].as_str().unwrap().to_string();

        let (status, body) = json(
            router,
            "PUT",
            &format!("/api/apps/demo/environments/production/routes/{route_id}"),
            serde_json::json!({"path_prefix": "/api/v2", "priority": 20}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["route"]["path_prefix"], "/api/v2");
        assert!(body["route"]["host"].is_null(), "omitted host clears it");

        let (status, body) = json(router, "GET", "/api/routes", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 1);

        // A relative prefix is a client mistake, not a server error.
        let (status, _) = json(
            router,
            "POST",
            "/api/apps/demo/environments/production/routes",
            serde_json::json!({"path_prefix": "api"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = json(
            router,
            "DELETE",
            &format!("/api/apps/demo/environments/production/routes/{route_id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Secrets: written encrypted, listed by name, never read back.
        let (status, body) = json(
            router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/API_KEY",
            serde_json::json!({"value": "super-secret"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !body.to_string().contains("super-secret"),
            "the response must not echo the value back"
        );

        let (status, body) = json(
            router,
            "GET",
            "/api/apps/demo/environments/production/secrets",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["secrets"][0]["name"], "API_KEY");
        assert!(!body.to_string().contains("super-secret"));

        let (status, _) = json(
            router,
            "DELETE",
            "/api/apps/demo/environments/production/secrets/API_KEY",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = json(
            router,
            "GET",
            "/api/apps/demo/environments/production/secrets",
            Value::Null,
        )
        .await;
        assert_eq!(body["total"], 0);
    }

    /// Without a master key the secret endpoints must refuse loudly rather
    /// than fall back to storing plaintext.
    #[tokio::test]
    async fn secrets_fail_loudly_without_a_master_key() {
        let h = harness(false).await;
        let router = &h.router;

        json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        json(
            router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;

        let (status, body) = json(
            router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/API_KEY",
            serde_json::json!({"value": "super-secret"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["error"].as_str().unwrap().contains("no key file"));

        // And nothing was stored.
        let (_, body) = json(
            router,
            "GET",
            "/api/apps/demo/environments/production/secrets",
            Value::Null,
        )
        .await;
        assert_eq!(body["total"], 0);
    }

    #[tokio::test]
    async fn deleting_an_app_clears_its_bundles_and_assignments() {
        let h = harness(true).await;
        let router = &h.router;

        json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let (_, body) = call(
            router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(sample_bundle("demo")),
            "application/zip",
        )
        .await;
        let version_id = body["version"]["id"].as_str().unwrap().to_string();
        json(
            router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        json(
            router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": version_id}),
        )
        .await;

        let bundle_dir = h._bundle_dir.path().join("demo");
        assert!(bundle_dir.exists());

        let (status, body) = json(router, "DELETE", "/api/apps/demo", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["removed"]["versions"], 1);
        assert_eq!(body["removed"]["deployments"], 1);
        assert_eq!(body["bundle_files_removed"], 1);

        let (status, _) = json(router, "GET", "/api/apps/demo", Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (_, body) = json(router, "GET", "/api/nodes/node-a/assignments", Value::Null).await;
        assert_eq!(body.as_array().unwrap().len(), 0);
    }

    #[test]
    fn public_address_prefers_configuration_then_host_header() {
        let address = PublicAddress {
            configured: Some("https://deis.example.com/".to_string()),
            scheme: "http",
            fallback: "http://127.0.0.1:8080".to_string(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "node-view:9000".parse().unwrap());
        assert_eq!(address.resolve(&headers), "https://deis.example.com");

        let address = PublicAddress {
            configured: None,
            scheme: "http",
            fallback: "http://127.0.0.1:8080".to_string(),
        };
        assert_eq!(address.resolve(&headers), "http://node-view:9000");
        assert_eq!(address.resolve(&HeaderMap::new()), "http://127.0.0.1:8080");
    }
}
