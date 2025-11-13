use anyhow::Result;
use axum::{
    Router,
    extract::{Request, State},
    response::{Response, IntoResponse},
    http::StatusCode,
};
use colored::Colorize;
use denictl_core::ProjectConfig;
use denictl_parser::TypeScriptParser;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use walkdir::WalkDir;

pub async fn execute(port: u16, _watch: bool, _debug: bool) -> Result<()> {
    println!("{}", "Starting development server...".green());

    // Load project config
    let config = load_config()?;

    // Discover routes from functions directory
    let routes = discover_routes(&config)?;

    // Create shared state
    let app_state = Arc::new(AppState {
        config,
        routes: RwLock::new(routes.clone()),
        kv_store: RwLock::new(HashMap::new()),
    });

    // Print server info
    print_server_info(port, &routes);

    // Build router
    let app = Router::new()
        .fallback(handler)
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

#[derive(Clone)]
struct RouteInfo {
    path: String,
    file_path: PathBuf,
    method: String,
}

struct AppState {
    config: ProjectConfig,
    routes: RwLock<Vec<RouteInfo>>,
    kv_store: RwLock<HashMap<String, String>>,
}

async fn handler(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Response {
    let path = req.uri().path();
    let method = req.method().as_str();

    info!("{} {} - Handling request", method, path);

    // Find matching route
    let routes = state.routes.read().await;
    let route = routes.iter().find(|r| match_route(&r.path, path));

    if let Some(route_info) = route {
        // In a real implementation, we would:
        // 1. Read and parse the handler file
        // 2. Execute the handler with a mock context
        // 3. Return the response

        info!("  → Matched route: {} ({})", route_info.path, route_info.file_path.display());

        // For now, return a mock response
        let body = serde_json::json!({
            "message": "Dev server response",
            "route": route_info.path,
            "handler": route_info.file_path.display().to_string(),
            "mock": true
        }).to_string();

        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(body.into())
            .unwrap()
    } else {
        warn!("  → No route matched");

        (StatusCode::NOT_FOUND, "404 - Not Found").into_response()
    }
}

fn match_route(route: &str, path: &str) -> bool {
    // Exact match
    if route == path {
        return true;
    }

    // Dynamic parameter match
    let route_parts: Vec<&str> = route.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();

    if route_parts.len() != path_parts.len() {
        return false;
    }

    route_parts.iter().zip(path_parts.iter()).all(|(r, p)| {
        if r.starts_with(':') {
            true
        } else {
            r == p
        }
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
                    // Parse file to check for exports
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

    // Sort routes by specificity (more specific routes first)
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
    println!("{}{}{}", "│".bold(), " Routes:                                          ".bright_white(), "│".bold());

    for route in routes {
        println!(
            "{}{}{}",
            "│".bold(),
            format!("   {:4} {:<40}", route.method, route.path).green(),
            "│".bold()
        );
    }

    if routes.is_empty() {
        println!("{}{}{}", "│".bold(), "   No routes found                               ".yellow(), "│".bold());
    }

    println!("{}{}{}", "│".bold(), "                                                  ".bright_white(), "│".bold());
    println!("{}{}{}", "│".bold(), " Services:                                        ".bright_white(), "│".bold());
    println!("{}{}{}", "│".bold(), "   ✓ KV Store (in-memory mock)                    ".green(), "│".bold());
    println!("{}{}{}", "│".bold(), "   ✓ Cache (disabled)                             ".dimmed(), "│".bold());
    println!("{}", "├──────────────────────────────────────────────────────┤".bold());
    println!("{}{}{}", "│".bold(), " Ready! Press Ctrl+C to stop                      ".yellow(), "│".bold());
    println!("{}", "└──────────────────────────────────────────────────────┘".bold());
    println!();
}
