//! The whole routing table, in the shape the edge consumes it.
//!
//! `GET /api/routes` next door answers the dashboard: it is a list of route
//! rows, keyed by the catalog's internal `env_id`, and it says nothing about
//! environments that have no route of their own. That is the wrong shape for
//! a data plane, which needs three things this endpoint adds:
//!
//! * the environment **name** (`production`, `pr-42`), because that is what
//!   travels on the wire in `x-deis-environment` and what the runtime
//!   branches on - an `env_id` reaching an application as a uuid would be a
//!   silent mismatch;
//! * every environment that exists, route or not, so the edge can offer an
//!   automatic address for one (see the preview subdomain in
//!   `deis-proxy`'s `catalog` module) without inventing catalog rows here;
//! * a **version** for the table as a whole, so an edge that polls can tell
//!   "nothing changed" from "changed" without diffing, and so the log line
//!   an operator reads names the table it is talking about.
//!
//! The table is served on [`super::node_router`] rather than the human API.
//! deis-proxy is not a runtime node, but it holds the same credential - the
//! `[control] secret` in its own file is a `node_preshared_keys` value,
//! because that is what its gRPC control channel authenticates with - and
//! this is the channel whose authentication `api_auth_enabled` cannot switch
//! off. An unauthenticated routing table would let anyone who can reach the
//! port enumerate every application id and every hostname the platform
//! serves, which is a map of the whole installation.
//!
//! Nothing secret is in the answer: hostnames, path prefixes, app ids and
//! environment names, all of which are already visible to anyone who can
//! make a request through the edge.

use std::collections::BTreeMap;

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use super::CatalogState;
use crate::api::error::ApiResult;
use crate::catalog::CatalogStore;

/// Path this table is served on.
///
/// Defined once in `deis_wire::conventions`, which deis-proxy reads the same
/// constant from: the two crates do not depend on each other, so a path each
/// spelled for itself could drift without anything failing to compile.
/// Re-exported here so this module still names the route it mounts.
pub use deis_wire::conventions::ROUTING_TABLE_ROUTE;

/// One route the edge should honour.
///
/// `host: None` means "any domain", exactly as the catalog row says it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingEntry {
    pub app_id: String,
    /// Environment *name*, never the id. See the module comment.
    pub environment: String,
    pub host: Option<String>,
    pub path_prefix: String,
    /// Higher wins when several routes match the same request.
    pub priority: i64,
}

/// One environment that exists, whether or not any route points at it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentRef {
    pub app_id: String,
    pub environment: String,
}

/// The complete answer. There is no partial form on purpose: an edge that
/// applies half a routing table sends live traffic to the wrong application,
/// so either the whole document parses or none of it is used.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingTable {
    /// Digest of everything below except [`Self::generated_at`], so a table
    /// that has not changed keeps the same version across polls.
    pub version: String,
    /// Unix seconds this answer was built. Deliberately outside the version:
    /// it is for a human reading a log line, not for change detection.
    pub generated_at: i64,
    pub routes: Vec<RoutingEntry>,
    pub environments: Vec<EnvironmentRef>,
}

/// GET `/api/routing-table`
pub async fn routing_table(State(state): State<CatalogState>) -> ApiResult<Json<RoutingTable>> {
    let table = build(&state.store).await?;
    debug!(
        version = %table.version,
        routes = table.routes.len(),
        environments = table.environments.len(),
        "Served the routing table"
    );
    Ok(Json(table))
}

/// Read the catalog and assemble the table.
///
/// The cost is one query for the apps, one per app for its environments, and
/// one for every route. That is deliberate: the alternative is a join in
/// `CatalogStore`, and the row count here is "applications on one private
/// platform" against a local SQLite file, polled every few seconds. If a
/// platform ever grows to where this shows up in a profile, the fix is a
/// single joined query in the store, not a cache with its own staleness.
pub async fn build(store: &CatalogStore) -> Result<RoutingTable, crate::catalog::CatalogError> {
    let apps = store.list_apps().await?;

    // env_id -> (app_id, environment name), the join `/api/routes` is
    // missing.
    let mut names: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut environments = Vec::new();
    for app in &apps {
        for env in store.list_environments(&app.id).await? {
            names.insert(env.id.clone(), (app.id.clone(), env.name.clone()));
            environments.push(EnvironmentRef {
                app_id: app.id.clone(),
                environment: env.name.clone(),
            });
        }
    }

    let mut routes = Vec::new();
    for route in store.list_routes(None, None).await? {
        let Some((app_id, environment)) = names.get(&route.env_id) else {
            // A route row whose environment is gone. The catalog deletes
            // routes with their environment, so this is a repair case, not a
            // normal one; dropping the row is right because there is nothing
            // to route *to*, and it is worth a line because it means the two
            // tables disagree.
            warn!(
                route_id = %route.id,
                env_id = %route.env_id,
                app_id = %route.app_id,
                "Route names an environment the catalog no longer has; it is left out of the routing table and traffic matching it reaches no application"
            );
            continue;
        };
        routes.push(RoutingEntry {
            app_id: app_id.clone(),
            environment: environment.clone(),
            host: route.host,
            path_prefix: route.path_prefix,
            priority: route.priority,
        });
    }

    // Sorted so the digest depends on the content and not on the order
    // SQLite happened to return rows in. The edge sorts again for matching;
    // this order exists only to make the version stable.
    routes.sort_by(|a, b| {
        a.host
            .cmp(&b.host)
            .then_with(|| a.path_prefix.cmp(&b.path_prefix))
            .then_with(|| b.priority.cmp(&a.priority))
            .then_with(|| a.app_id.cmp(&b.app_id))
            .then_with(|| a.environment.cmp(&b.environment))
    });
    environments.sort_by(|a, b| {
        a.app_id
            .cmp(&b.app_id)
            .then_with(|| a.environment.cmp(&b.environment))
    });

    let version = digest(&routes, &environments);
    Ok(RoutingTable {
        version,
        generated_at: chrono::Utc::now().timestamp(),
        routes,
        environments,
    })
}

/// Content digest of the table.
///
/// The separators are control characters that none of the fields can
/// contain (app ids, environment names, hostnames and path prefixes are all
/// validated in `catalog::model`), so two different tables cannot hash the
/// same way by running their fields together.
fn digest(routes: &[RoutingEntry], environments: &[EnvironmentRef]) -> String {
    const FIELD: &[u8] = b"\x1f";
    const RECORD: &[u8] = b"\x1e";

    let mut hasher = Sha256::new();
    for route in routes {
        hasher.update(route.app_id.as_bytes());
        hasher.update(FIELD);
        hasher.update(route.environment.as_bytes());
        hasher.update(FIELD);
        hasher.update(route.host.as_deref().unwrap_or("").as_bytes());
        hasher.update(FIELD);
        hasher.update(route.path_prefix.as_bytes());
        hasher.update(FIELD);
        hasher.update(route.priority.to_string().as_bytes());
        hasher.update(RECORD);
    }
    // A different marker between the two sections, so moving a name from one
    // to the other cannot leave the digest unchanged.
    hasher.update(b"\x1d");
    for env in environments {
        hasher.update(env.app_id.as_bytes());
        hasher.update(FIELD);
        hasher.update(env.environment.as_bytes());
        hasher.update(RECORD);
    }
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::catalog::tests::{TEST_NODE_KEY, harness};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn get_table(router: &axum::Router, key: Option<&str>) -> (StatusCode, RoutingTable) {
        let mut request = Request::builder().uri(ROUTING_TABLE_ROUTE);
        if let Some(key) = key {
            request = request.header("X-API-Key", key);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        if status != StatusCode::OK {
            return (
                status,
                RoutingTable {
                    version: String::new(),
                    generated_at: 0,
                    routes: Vec::new(),
                    environments: Vec::new(),
                },
            );
        }
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    async fn create_app(router: &axum::Router, id: &str) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/apps")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"id":"{id}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    async fn create_environment(router: &axum::Router, app: &str, name: &str) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/apps/{app}/environments"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"name":"{name}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    async fn create_route(router: &axum::Router, app: &str, env: &str, body: &str) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/apps/{app}/environments/{env}/routes"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn the_table_names_environments_not_ids() {
        let harness = harness(true).await;
        create_app(&harness.router, "shop").await;
        create_environment(&harness.router, "shop", "production").await;
        create_environment(&harness.router, "shop", "pr-42").await;
        create_route(
            &harness.router,
            "shop",
            "pr-42",
            r#"{"host":"example.com","path_prefix":"/api","priority":10}"#,
        )
        .await;

        let (status, table) = get_table(&harness.router, Some(TEST_NODE_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            table.routes,
            vec![RoutingEntry {
                app_id: "shop".to_string(),
                environment: "pr-42".to_string(),
                host: Some("example.com".to_string()),
                path_prefix: "/api".to_string(),
                priority: 10,
            }]
        );
        // Both environments are listed, including production, which has no
        // route of its own: that is what an automatic preview address is
        // built from.
        assert_eq!(
            table.environments,
            vec![
                EnvironmentRef {
                    app_id: "shop".to_string(),
                    environment: "pr-42".to_string(),
                },
                EnvironmentRef {
                    app_id: "shop".to_string(),
                    environment: "production".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn the_version_tracks_content_and_not_the_clock() {
        let harness = harness(true).await;
        create_app(&harness.router, "shop").await;
        create_environment(&harness.router, "shop", "production").await;

        let (_, first) = get_table(&harness.router, Some(TEST_NODE_KEY)).await;
        let (_, again) = get_table(&harness.router, Some(TEST_NODE_KEY)).await;
        assert_eq!(
            first.version, again.version,
            "an unchanged catalog must produce the same version, or every poll would look like a change"
        );

        create_route(
            &harness.router,
            "shop",
            "production",
            r#"{"path_prefix":"/"}"#,
        )
        .await;
        let (_, after) = get_table(&harness.router, Some(TEST_NODE_KEY)).await;
        assert_ne!(first.version, after.version);
    }

    /// The half of the contract this crate owns.
    ///
    /// deis-proxy spells the same path and the same field names in its
    /// `catalog::wire`, and nothing type-checks the two against each other -
    /// they are in crates that do not depend on one another. Until the names
    /// move to `deis_wire::conventions`, this test and its twin over there
    /// are what hold them together: change either literal and one of them
    /// fails naming the other.
    #[tokio::test]
    async fn the_wire_contract_is_the_one_deis_proxy_reads() {
        assert_eq!(
            ROUTING_TABLE_ROUTE, "/api/routing-table",
            "deis-proxy dials this path from the same deis_wire constant"
        );

        let harness = harness(true).await;
        create_app(&harness.router, "shop").await;
        create_environment(&harness.router, "shop", "production").await;
        create_route(
            &harness.router,
            "shop",
            "production",
            r#"{"host":"example.com","path_prefix":"/","priority":3}"#,
        )
        .await;

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(ROUTING_TABLE_ROUTE)
                    .header("X-API-Key", TEST_NODE_KEY)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let mut top: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        top.sort_unstable();
        assert_eq!(top, ["environments", "generated_at", "routes", "version"]);

        let mut route: Vec<&str> = value["routes"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        route.sort_unstable();
        assert_eq!(
            route,
            ["app_id", "environment", "host", "path_prefix", "priority"]
        );
    }

    #[tokio::test]
    async fn the_table_needs_a_node_credential() {
        let harness = harness(true).await;
        let (status, _) = get_table(&harness.router, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = get_table(&harness.router, Some("not-the-node-key")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
