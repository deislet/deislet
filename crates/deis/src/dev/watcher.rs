use anyhow::Result;
use colored::Colorize;
use deis_build::ProjectConfig;
use deis_build::parser::TypeScriptParser;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn};
use walkdir::WalkDir;

#[derive(Clone)]
pub struct RouteInfo {
    pub path: String,
    pub file_path: PathBuf,
    pub method: String,
}

pub async fn start_file_watcher(
    function_root: PathBuf,
    config: ProjectConfig,
    routes: Arc<RwLock<Vec<RouteInfo>>>,
) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);

    // Create watcher
    let mut watcher = RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                // Only process file modifications and creations
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                        let _ = tx.blocking_send(event);
                    }
                    _ => {}
                }
            }
        },
        Config::default().with_poll_interval(Duration::from_millis(500)),
    )?;

    // Watch the functions directory
    watcher.watch(&function_root, RecursiveMode::Recursive)?;

    info!("👀 Watching {} for changes...", function_root.display());

    // Keep watcher alive and handle events
    tokio::spawn(async move {
        let _watcher = watcher; // Keep watcher alive

        while let Some(event) = rx.recv().await {
            // Check if any of the paths are handler files
            let has_handler_change = event.paths.iter().any(|p| {
                if let Some(ext) = p.extension() {
                    matches!(ext.to_str(), Some("ts" | "js" | "tsx" | "jsx"))
                } else {
                    false
                }
            });

            if has_handler_change {
                // Debounce - wait a bit for multiple rapid changes
                tokio::time::sleep(Duration::from_millis(200)).await;

                // Clear any queued events
                while rx.try_recv().is_ok() {}

                // Reload routes
                match discover_routes(&config) {
                    Ok(new_routes) => {
                        let mut routes_lock = routes.write().await;
                        let old_count = routes_lock.len();
                        *routes_lock = new_routes.clone();
                        let new_count = routes_lock.len();

                        println!(
                            "\n{} {}",
                            "🔄 Routes reloaded:".bright_green().bold(),
                            format!("{} routes (was {})", new_count, old_count).cyan()
                        );

                        // Print changed routes
                        for route in new_routes.iter().take(5) {
                            println!("   {} {}", "→".dimmed(), route.path.green());
                        }
                        if new_routes.len() > 5 {
                            println!("   {} and {} more...", "→".dimmed(), new_routes.len() - 5);
                        }
                        println!();
                    }
                    Err(e) => {
                        warn!("❌ Failed to reload routes: {}", e);
                    }
                }
            }
        }
    });

    Ok(())
}

pub fn discover_routes(config: &ProjectConfig) -> Result<Vec<RouteInfo>> {
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
            if let Some(ext) = path.extension()
                && (ext == "ts" || ext == "js" || ext == "tsx" || ext == "jsx")
                    && let Ok(code) = std::fs::read_to_string(path)
                        && let Ok(handler_info) = parser.parse(&code, path.to_str().unwrap_or(""))
                            && handler_info.has_default_export {
                                let route = handler_info.route.clone();
                                routes.push(RouteInfo {
                                    path: route,
                                    file_path: path.to_path_buf(),
                                    method: "ALL".to_string(),
                                });
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
