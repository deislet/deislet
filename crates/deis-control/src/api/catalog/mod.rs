//! REST surface of the catalog: apps, versions, environments,
//! deployments, routes, secrets, and the two endpoints nodes use to work
//! out what they should be running (`assignments` and the bundle
//! download).
//!
//! It is built as two routers, because the two halves answer to different
//! credentials:
//!
//! * [`router`] is the human surface - dashboard and CLI. It is merged into
//!   the part of the API guarded by the API key, which `api_auth_enabled`
//!   turns on and off.
//! * [`node_router`] is what a runtime node calls. It carries its own
//!   authentication ([`crate::api::middleware::node_auth`]) and ignores
//!   `api_auth_enabled` entirely: the assignment list hands out every
//!   environment's decrypted secrets, so that channel cannot be left open by
//!   a flag whose purpose is to make a dashboard convenient.

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
    routing::{get, post, put},
};

use crate::api::middleware::{NodeAuthConfig, node_auth};
use crate::catalog::{BundleStore, CatalogStore, SecretsBackend};
use crate::delivery::DeploymentDeliverer;

/// Largest bundle the control plane will accept in one upload. Bundles
/// carry the worker code plus every static asset, so this is generous;
/// what it stops is a client streaming an unbounded body into memory.
pub const MAX_BUNDLE_BYTES: usize = 64 * 1024 * 1024;

/// Shared state of every catalog endpoint.
#[derive(Clone)]
pub struct CatalogState {
    pub store: Arc<CatalogStore>,
    pub bundles: Arc<BundleStore>,
    pub secrets: Arc<SecretsBackend>,
    /// Pushes a deployment out to the nodes. Deploy, rollback and secret
    /// change all end here; see [`crate::delivery`].
    pub deliverer: Arc<DeploymentDeliverer>,
    /// The credential the node endpoints demand.
    ///
    /// Handlers read it to learn whether this control plane can tell a node
    /// from anyone else at all: [`node_router`] layers the check, so when a
    /// key is configured every request that reaches a handler is from a
    /// caller that presented one, and when none is configured every caller
    /// is anonymous and no secret value may be served.
    pub node_auth: NodeAuthConfig,
}

/// Build the human half of the catalog router: everything the dashboard
/// and the CLI call. The caller merges it into the API-key-guarded router.
pub fn router(state: CatalogState) -> Router {
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
        .with_state(state)
}

/// Build the node half: the assignment list and the bundle download.
///
/// The authentication layer is applied here rather than by the caller, so
/// there is no way to mount these two routes without it. Both exist for
/// runtime nodes; a dashboard API key is not accepted, because the list one
/// of them returns contains secret values that the dashboard is deliberately
/// unable to read back (docs/architecture.zh.md section 8).
pub fn node_router(state: CatalogState) -> Router {
    let auth = state.node_auth.clone();
    Router::new()
        // The path parameter is named `:id` to match the node routes it
        // shares a prefix with; the router rejects two names in the same
        // position.
        .route(
            deis_wire::conventions::NODE_ASSIGNMENTS_ROUTE,
            get(assignments::for_node),
        )
        .route(
            "/api/apps/:app_id/versions/:version_id/bundle",
            get(versions::download_bundle),
        )
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(auth, node_auth))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::catalog::bundle::tests::sample_bundle;
    use crate::catalog::{SecretCipher, secrets::MASTER_KEY_LEN};
    use crate::database::Database;
    use crate::delivery::PublicAddress;
    use crate::runtime_client::RuntimeNodeManager;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    pub(crate) struct Harness {
        pub router: Router,
        pub _bundle_dir: tempfile::TempDir,
    }

    /// The node key every harness below is built with. There is no
    /// credential-less variant on purpose: a control plane without a node
    /// key serves none of these routes at all.
    pub(crate) const TEST_NODE_KEY: &str = "node-psk";

    /// A router with the ordinary shape: one node key, so the node
    /// endpoints answer a caller that presents it and nobody else.
    pub(crate) async fn harness(secrets_ready: bool) -> Harness {
        harness_with(secrets_ready, vec![TEST_NODE_KEY.to_string()]).await
    }

    pub(crate) async fn harness_with(secrets_ready: bool, node_keys: Vec<String>) -> Harness {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let bundle_dir = tempfile::tempdir().unwrap();
        let secrets = if secrets_ready {
            SecretsBackend::Ready(Box::new(
                SecretCipher::from_key_bytes(&[42u8; MASTER_KEY_LEN]).unwrap(),
            ))
        } else {
            SecretsBackend::Unavailable("no key file in this test".to_string())
        };
        let store = Arc::new(CatalogStore::new(database));
        let secrets = Arc::new(secrets);
        let address = Arc::new(PublicAddress {
            configured: Some("http://control.test".to_string()),
            scheme: "http",
            fallback: "http://127.0.0.1:8080".to_string(),
        });
        let state = CatalogState {
            store: Arc::clone(&store),
            bundles: Arc::new(BundleStore::new(bundle_dir.path())),
            secrets: Arc::clone(&secrets),
            // No runtime nodes: a delivery is reported as skipped, which
            // is exactly the shape these tests want to see travel through
            // the deploy and secret endpoints.
            deliverer: Arc::new(DeploymentDeliverer::new(
                store,
                secrets,
                Arc::new(RuntimeNodeManager::new(Vec::new(), 15)),
                address,
                // Built the way the server builds it, so the harness cannot
                // drift from the real assembly of a delivery.
                crate::delivery::StorageWiring::from_config(&crate::config::ControlConfig {
                    store_endpoint: Some("http://127.0.0.1:8090".to_string()),
                    ..Default::default()
                }),
            )),
            node_auth: NodeAuthConfig::new(node_keys),
        };
        Harness {
            // Both halves, mounted the way the API mounts them: the node
            // half brings its own authentication layer, so a test that
            // calls it is exercising the real check.
            router: router(state.clone()).merge(node_router(state)),
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

    /// GET as a node, keeping the response headers.
    ///
    /// The assignment list says whether it is the whole truth in a header,
    /// and that bit is what decides whether a node may unload anything, so
    /// it has to be asserted on rather than dropped with the rest.
    async fn get_as_node_full(
        router: &Router,
        uri: &str,
        credential: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, Value) {
        let mut builder = Request::builder().method("GET").uri(uri);
        if let Some(credential) = credential {
            builder = builder.header("Authorization", format!("Bearer {credential}"));
        }
        let response = router
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), MAX_BUNDLE_BYTES)
            .await
            .unwrap();
        (
            status,
            headers,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// What the assignment list said about its own completeness: the flag,
    /// and how many rows it could not describe.
    fn completeness(headers: &axum::http::HeaderMap) -> (bool, usize) {
        let complete = headers
            .get(assignments::ASSIGNMENTS_COMPLETE_HEADER)
            .expect("the list must always say whether it is complete")
            .to_str()
            .unwrap()
            == "true";
        let withheld = headers
            .get(assignments::ASSIGNMENTS_WITHHELD_HEADER)
            .expect("the list must always say how much it left out")
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        (complete, withheld)
    }

    /// GET with an optional node credential, in the header a node sends.
    async fn get_as_node(
        router: &Router,
        uri: &str,
        credential: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method("GET").uri(uri);
        if let Some(credential) = credential {
            builder = builder.header("Authorization", format!("Bearer {credential}"));
        }
        let response = router
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), MAX_BUNDLE_BYTES)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Deploy one version of `app` and store `secret` in its production
    /// environment when one is given. Returns nothing: every assertion is
    /// about what the node endpoints answer afterwards.
    async fn deployed_app(router: &Router, app: &str, secret: Option<(&str, &str)>) {
        json(router, "POST", "/api/apps", serde_json::json!({"id": app})).await;
        let (_, body) = call(
            router,
            "POST",
            &format!("/api/apps/{app}/versions"),
            Body::from(sample_bundle(app)),
            "application/zip",
        )
        .await;
        let version_id = body["version"]["id"].as_str().unwrap().to_string();
        json(
            router,
            "POST",
            &format!("/api/apps/{app}/environments"),
            serde_json::json!({"name": "production"}),
        )
        .await;
        if let Some((name, value)) = secret {
            let (status, _) = json(
                router,
                "PUT",
                &format!("/api/apps/{app}/environments/production/secrets/{name}"),
                serde_json::json!({"value": value}),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
        let (status, _) = json(
            router,
            "POST",
            &format!("/api/apps/{app}/environments/production/deployments"),
            serde_json::json!({"version_id": version_id}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    /// The assignment list is the one read API that answers with decrypted
    /// secrets, so it authenticates the caller as a node - and does it
    /// whatever `api_auth_enabled` says, which is a dashboard setting this
    /// router does not even see.
    #[tokio::test]
    async fn the_assignment_list_serves_secrets_only_to_an_authenticated_node() {
        let h = harness_with(true, vec!["node-psk".to_string()]).await;
        let router = &h.router;
        deployed_app(router, "demo", Some(("API_KEY", "super-secret"))).await;

        let (status, body) = get_as_node(router, "/api/nodes/node-a/assignments", None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "an anonymous caller must not receive the fleet's secrets"
        );
        assert!(!body.to_string().contains("super-secret"));

        let (status, body) =
            get_as_node(router, "/api/nodes/node-a/assignments", Some("wrong-key")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!body.to_string().contains("super-secret"));

        // The node's own credential works, and the value it needs is there.
        let (status, body) =
            get_as_node(router, "/api/nodes/node-a/assignments", Some("node-psk")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["env"]["API_KEY"], "super-secret");

        // The bundle download is the other half of the same channel.
        let (status, _) = get_as_node(
            router,
            &format!(
                "/api/apps/demo/versions/{}/bundle",
                body[0]["version_id"].as_str().unwrap()
            ),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// With no node credential nobody can prove they are a node, so the
    /// channel is shut rather than served anonymously. There is no
    /// half-answer to build: the list carries decrypted secrets, and the
    /// bundle download beside it is the code those secrets belong to.
    #[tokio::test]
    async fn a_control_plane_without_a_node_key_serves_no_node_endpoint() {
        let h = harness_with(true, Vec::new()).await;
        let router = &h.router;
        deployed_app(router, "with-secret", Some(("API_KEY", "super-secret"))).await;

        for credential in [None, Some("guess")] {
            let (status, body) =
                get_as_node(router, "/api/nodes/node-a/assignments", credential).await;
            assert_eq!(
                status,
                StatusCode::SERVICE_UNAVAILABLE,
                "an unconfigured node channel must be shut, not open"
            );
            assert!(!body.to_string().contains("super-secret"), "got {body}");
        }
    }

    /// The other half of deletion. Push cannot reach a node that is down, so
    /// the assignment list has to be usable as the full expected set: what is
    /// not in it is not supposed to be running. That only holds when the list
    /// says it is complete, which is what a node keys its removals off.
    #[tokio::test]
    async fn the_assignment_list_is_authoritative_so_a_deleted_app_leaves_the_node() {
        let h = harness_with(true, vec!["node-psk".to_string()]).await;
        let router = &h.router;
        deployed_app(router, "demo", Some(("API_KEY", "super-secret"))).await;
        deployed_app(router, "other", None).await;

        let (status, headers, body) =
            get_as_node_full(router, "/api/nodes/node-a/assignments", Some("node-psk")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 2);
        assert_eq!(
            completeness(&headers),
            (true, 0),
            "every assignment could be described, so this answer is the whole expected set"
        );

        let (status, _) = json(router, "DELETE", "/api/apps/demo", Value::Null).await;
        assert_eq!(status, StatusCode::OK);

        // The deleted application is simply absent, and the answer still
        // claims to be complete - which is what turns that absence into
        // "stop serving it" on a node that missed the unload push.
        let (_, headers, body) =
            get_as_node_full(router, "/api/nodes/node-a/assignments", Some("node-psk")).await;
        let apps: Vec<&str> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["app_id"].as_str().unwrap())
            .collect();
        assert_eq!(apps, ["other"]);
        assert_eq!(completeness(&headers), (true, 0));

        // An empty list is an instruction too, not a "nothing to say".
        let (status, _) = json(router, "DELETE", "/api/apps/other", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let (_, headers, body) =
            get_as_node_full(router, "/api/nodes/node-a/assignments", Some("node-psk")).await;
        assert_eq!(body.as_array().unwrap().len(), 0);
        assert_eq!(completeness(&headers), (true, 0));
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
        let bundle2 = crate::catalog::bundle::tests::bundle_with_manifest(
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
            get_as_node(router, "/api/nodes/node-a/assignments", Some(TEST_NODE_KEY)).await;
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
            get_as_node(router, "/api/nodes/node-a/assignments", Some(TEST_NODE_KEY)).await;
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
        // The list carries the environment a push would have carried. Without
        // it a node that restarts loads the right code with no storage
        // address and no secret, and only its ctx.services calls tell it so.
        assert_eq!(
            assignments[0]["env"][deis_wire::conventions::STORE_ENDPOINT_ENV],
            "http://127.0.0.1:8090"
        );

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
            get_as_node(router, "/api/nodes/node-a/assignments", Some(TEST_NODE_KEY)).await;
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
            .header("Authorization", format!("Bearer {TEST_NODE_KEY}"))
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

        let (_, body) =
            get_as_node(router, "/api/nodes/node-a/assignments", Some(TEST_NODE_KEY)).await;
        assert_eq!(body.as_array().unwrap().len(), 0);
    }
}
