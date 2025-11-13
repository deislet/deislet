use super::mock_kv::MockKVStore;
use anyhow::Result;
use axum::{
    Router,
    routing::{get, post, delete as axum_delete},
    extract::{Request, State, Path as AxumPath, Query},
    response::{Response, IntoResponse, Json},
    http::{StatusCode, HeaderMap},
};
use colored::Colorize;
use denictl_core::ProjectConfig;
use denictl_parser::TypeScriptParser;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use walkdir::WalkDir;

#[derive(Clone)]
struct RouteInfo {
    path: String,
    file_path: PathBuf,
    method: String,
}

struct AppState {
    config: ProjectConfig,
    routes: RwLock<Vec<RouteInfo>>,
    kv_store: MockKVStore,
}

pub async fn run_dev_server(port: u16, _watch: bool, _debug: bool) -> Result<()> {
    println!("{}", "Starting development server...".green());

    // Load project config
    let config = load_config()?;

    // Discover routes from functions directory
    let routes = discover_routes(&config)?;

    // Create shared state with Mock KV
    let app_state = Arc::new(AppState {
        config,
        routes: RwLock::new(routes.clone()),
        kv_store: MockKVStore::new(),
    });

    // Print server info
    print_server_info(port, &routes);

    // Build router with function routes and KV management API
    let app = Router::new()
        // KV Management API (for debugging)
        .route("/__dev/kv", get(kv_list_handler).post(kv_put_handler))
        .route("/__dev/kv/:key", get(kv_get_handler).delete(kv_delete_handler))
        .route("/__dev/kv/_clear", post(kv_clear_handler))
        .route("/__dev/kv/_stats", get(kv_stats_handler))
        // Function routes
        .fallback(function_handler)
        .layer(TraceLayer::new_for_http())
        .with_state(app_state);

    // Start server
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;

    info!("✓ Dev server listening on http://localhost:{}", port);

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;

    Ok(())
}

// Function route handler
async fn function_handler(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Response {
    let path = req.uri().path();
    let method = req.method().as_str();
    let query = req.uri().query().unwrap_or("");

    info!("{} {} {}", method.green(), path, if query.is_empty() { String::new() } else { format!("?{}", query).dimmed().to_string() });

    // Find matching route
    let routes = state.routes.read().await;
    let route = routes.iter().find(|r| match_route(&r.path, path));

    if let Some(route_info) = route {
        info!("  → Matched route: {} ({})", route_info.path.cyan(), route_info.file_path.display().to_string().dimmed());

        // Build mock context
        let context = build_mock_context(
            req.method().as_str(),
            req.uri(),
            req.headers(),
            &route_info.path,
            path,
            &state.kv_store,
        ).await;

        // Return mock response with context info
        let body = serde_json::json!({
            "dev_mode": true,
            "message": "Dev server mock response",
            "route": route_info.path,
            "handler": route_info.file_path.display().to_string(),
            "context": {
                "path": context.path,
                "method": context.method,
                "params": context.params,
                "query": context.query,
                "kv_available": context.kv_available,
            },
            "note": "To execute actual handler, implement JavaScript runtime integration"
        }).to_string();

        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("x-dev-server", "denictl")
            .body(body.into())
            .unwrap()
    } else {
        warn!("  → {} No route matched", "✗".red());

        (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "error": "Not Found",
            "path": path,
            "available_routes": routes.iter().map(|r| &r.path).collect::<Vec<_>>()
        }))).into_response()
    }
}

// KV Management API Handlers
#[derive(Deserialize)]
struct KVGetQuery {
    #[serde(default)]
    metadata: bool,
}

async fn kv_get_handler(
    State(state): State<Arc<AppState>>,
    AxumPath(key): AxumPath<String>,
    Query(params): Query<KVGetQuery>,
) -> impl IntoResponse {
    if params.metadata {
        if let Some((value, metadata)) = state.kv_store.get_with_metadata(&key).await {
            return (StatusCode::OK, Json(serde_json::json!({
                "value": value,
                "metadata": metadata,
            }))).into_response();
        }
    } else {
        if let Some(value) = state.kv_store.get(&key).await {
            return (StatusCode::OK, Json(serde_json::json!({
                "value": value,
            }))).into_response();
        }
    }

    (StatusCode::NOT_FOUND, Json(serde_json::json!({
        "error": "Key not found"
    }))).into_response()
}

#[derive(Deserialize)]
struct KVPutRequest {
    key: String,
    value: String,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    #[serde(default)]
    expiration_ttl: Option<u64>,
}

async fn kv_put_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<KVPutRequest>,
) -> impl IntoResponse {
    state.kv_store.put(req.key.clone(), req.value, req.metadata, req.expiration_ttl).await;

    (StatusCode::OK, Json(serde_json::json!({
        "success": true,
        "key": req.key,
    })))
}

async fn kv_delete_handler(
    State(state): State<Arc<AppState>>,
    AxumPath(key): AxumPath<String>,
) -> impl IntoResponse {
    let deleted = state.kv_store.delete(&key).await;

    (StatusCode::OK, Json(serde_json::json!({
        "deleted": deleted,
    })))
}

#[derive(Deserialize)]
struct KVListQuery {
    prefix: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

async fn kv_list_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<KVListQuery>,
) -> impl IntoResponse {
    let result = state.kv_store.list(
        params.prefix.as_deref(),
        params.limit,
        params.cursor.as_deref(),
    ).await;

    (StatusCode::OK, Json(result))
}

async fn kv_clear_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.kv_store.clear().await;

    (StatusCode::OK, Json(serde_json::json!({
        "success": true,
        "message": "All KV data cleared",
    })))
}

async fn kv_stats_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let count = state.kv_store.count().await;

    (StatusCode::OK, Json(serde_json::json!({
        "count": count,
    })))
}

// Mock Context
#[derive(Serialize)]
struct MockContext {
    path: String,
    method: String,
    params: serde_json::Value,
    query: serde_json::Value,
    kv_available: bool,
}

async fn build_mock_context(
    method: &str,
    uri: &axum::http::Uri,
    _headers: &HeaderMap,
    route: &str,
    path: &str,
    kv_store: &MockKVStore,
) -> MockContext {
    let params = extract_params(path, route);
    let query: serde_json::Value = if let Some(q) = uri.query() {
        let mut map = serde_json::Map::new();
        for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
            map.insert(k.to_string(), serde_json::Value::String(v.to_string()));
        }
        serde_json::Value::Object(map)
    } else {
        serde_json::json!({})
    };

    let kv_count = kv_store.count().await;

    MockContext {
        path: path.to_string(),
        method: method.to_string(),
        params: serde_json::json!(params),
        query,
        kv_available: kv_count > 0 || true, // Always show as available in dev mode
    }
}

fn extract_params(path: &str, route: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut params = serde_json::Map::new();
    let route_parts: Vec<&str> = route.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();

    route_parts.iter().zip(path_parts.iter()).for_each(|(r, p)| {
        if r.starts_with(':') {
            let key = r.trim_start_matches(':');
            params.insert(key.to_string(), serde_json::Value::String(p.to_string()));
        }
    });

    params
}

fn match_route(route: &str, path: &str) -> bool {
    if route == path {
        return true;
    }

    let route_parts: Vec<&str> = route.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();

    if route_parts.len() != path_parts.len() {
        return false;
    }

    route_parts.iter().zip(path_parts.iter()).all(|(r, p)| {
        r.starts_with(':') || r == p
    })
}

fn discover_routes(config: &ProjectConfig) -> Result<Vec<RouteInfo>> {
    let mut routes = Vec::new();
    let function_root = Path::new(&config.function_root);

    if !function_root.exists() {
        return Err(anyhow::anyhow!(
            "Functions directory not found: {}",
            function_root.display()
        ));
    }

    let parser = TypeScriptParser::new();

    for entry in WalkDir::new(function_root) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "ts" || ext == "js" || ext == "tsx" || ext == "jsx" {
                    if let Ok(code) = std::fs::read_to_string(path) {
                        if let Ok(handler_info) = parser.parse(&code, path.to_str().unwrap_or("")) {
                            if handler_info.has_default_export {
                                let route = handler_info.route.clone();
                                routes.push(RouteInfo {
                                    path: route,
                                    file_path: path.to_path_buf(),
                                    method: "ALL".to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    routes.sort_by(|a, b| {
        let a_parts = a.path.split('/').count();
        let b_parts = b.path.split('/').count();
        b_parts.cmp(&a_parts)
    });

    Ok(routes)
}

fn load_config() -> Result<ProjectConfig> {
    let config_path = Path::new(".config.json");
    if !config_path.exists() {
        return Err(anyhow::anyhow!(
            "No .config.json found. Run 'denictl init' first."
        ));
    }

    let config_str = std::fs::read_to_string(config_path)?;
    let config: ProjectConfig = serde_json::from_str(&config_str)?;
    Ok(config)
}

fn print_server_info(port: u16, routes: &[RouteInfo]) {
    println!("\n{}", "┌──────────────────────────────────────────────────────┐".bold());
    println!(
        "{}{}{}",
        "│".bold(),
        "              Edge Function Dev Server            ".cyan(),
        "│".bold()
    );
    println!("{}", "├──────────────────────────────────────────────────────┤".bold());
    println!(
        "{}{}{}",
        "│".bold(),
        format!(" Local:   http://localhost:{:<26}", port).bright_white(),
        "│".bold()
    );
    println!("{}", "├──────────────────────────────────────────────────────┤".bold());
    println!("{}{}{}", "│".bold(), " Function Routes:                                 ".bright_white(), "│".bold());

    for route in routes.iter().take(10) {
        println!(
            "{}{}{}",
            "│".bold(),
            format!("   {:4} {:<40}", route.method, route.path).green(),
            "│".bold()
        );
    }

    if routes.len() > 10 {
        println!(
            "{}{}{}",
            "│".bold(),
            format!("   ... and {} more routes", routes.len() - 10).yellow(),
            "│".bold()
        );
    }

    if routes.is_empty() {
        println!("{}{}{}", "│".bold(), "   No routes found                               ".yellow(), "│".bold());
    }

    println!("{}{}{}", "│".bold(), "                                                  ".bright_white(), "│".bold());
    println!("{}{}{}", "│".bold(), " Dev API:                                         ".bright_white(), "│".bold());
    println!("{}{}{}", "│".bold(), format!("   GET    http://localhost:{}/__dev/kv          ", port).dimmed(), "│".bold());
    println!("{}{}{}", "│".bold(), format!("   POST   http://localhost:{}/__dev/kv          ", port).dimmed(), "│".bold());
    println!("{}{}{}", "│".bold(), format!("   GET    http://localhost:{}/__dev/kv/:key     ", port).dimmed(), "│".bold());
    println!("{}{}{}", "│".bold(), "                                                  ".bright_white(), "│".bold());
    println!("{}{}{}", "│".bold(), " Services:                                        ".bright_white(), "│".bold());
    println!("{}{}{}", "│".bold(), "   ✓ KV Store (in-memory mock)                    ".green(), "│".bold());
    println!("{}{}{}", "│".bold(), "   ✓ Context Builder                              ".green(), "│".bold());
    println!("{}{}{}", "│".bold(), "   ✓ Request Logging                              ".green(), "│".bold());
    println!("{}", "├──────────────────────────────────────────────────────┤".bold());
    println!("{}{}{}", "│".bold(), " Ready! Press Ctrl+C to stop                      ".yellow(), "│".bold());
    println!("{}", "└──────────────────────────────────────────────────────┘".bold());
    println!();
}
