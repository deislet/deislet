// HTTP Gateway Module

pub mod handlers;

use crate::app::ApplicationManager;
use crate::app_repo::AppRepo;
use crate::config_store::ConfigStore;
use crate::management::ManagementServer;
use crate::trigger::TriggerServer;
use anyhow::Result;
use axum::Router;
use std::sync::Arc;

use tower_http::trace::TraceLayer;
use tracing::info;

/// Application State
#[derive(Clone)]
pub struct AppState {
    pub app_manager: Arc<ApplicationManager>,
}

/// Start HTTP + gRPC server (reuse the same port)
pub async fn start_server(
    app_manager: Arc<ApplicationManager>,
    config_store: Arc<ConfigStore>,
    app_repo: Arc<AppRepo>,
    worker_id: String,
    port: u16,
) -> Result<()> {
    let state = AppState {
        app_manager: app_manager.clone(),
    };

    // Create gRPC services
    let management_service =
        ManagementServer::create_service(app_manager.clone(), config_store, app_repo, worker_id);
    let trigger_service = TriggerServer::create_service(app_manager.clone());

    // Combine gRPC services
    let grpc_service = tower::service_fn(move |req: axum::extract::Request| {
        let mut management_service = management_service.clone();
        let mut trigger_service = trigger_service.clone();
        async move {
            let path = req.uri().path();
            if path.starts_with("/denix.management.") {
                match tower::Service::call(&mut management_service, req).await {
                    Ok(res) => {
                        let (parts, body) = res.into_parts();
                        let body = axum::body::Body::new(body);
                        Ok::<_, std::convert::Infallible>(axum::http::Response::from_parts(
                            parts, body,
                        ))
                    }
                    Err(_) => Ok(axum::http::Response::builder()
                        .status(500)
                        .body(axum::body::Body::empty())
                        .unwrap()),
                }
            } else if path.starts_with("/denix.trigger.") {
                match tower::Service::call(&mut trigger_service, req).await {
                    Ok(res) => {
                        let (parts, body) = res.into_parts();
                        let body = axum::body::Body::new(body);
                        Ok::<_, std::convert::Infallible>(axum::http::Response::from_parts(
                            parts, body,
                        ))
                    }
                    Err(_) => Ok(axum::http::Response::builder()
                        .status(500)
                        .body(axum::body::Body::empty())
                        .unwrap()),
                }
            } else {
                Ok(axum::http::Response::builder()
                    .status(404)
                    .body(axum::body::Body::empty())
                    .unwrap())
            }
        }
    });

    // Build HTTP routes - 所有请求通过 x-deis-app-id header 路由
    // 添加通配符路由来处理所有 HTTP 请求
    let http_router = Router::new()
        .route("/*path", axum::routing::any(handlers::handle_request))
        .route("/", axum::routing::any(handlers::handle_request))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    // Merge HTTP and gRPC services
    // HTTP 路由优先，gRPC 作为 fallback（但实际上 HTTP 会处理所有非 gRPC 的请求）
    let app = http_router.fallback_service(grpc_service);

    // Bind address
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    // 预加载系统管理应用
    let system_app_code = include_str!("system_app.js");
    match app_manager
        .load_app(
            "_system".to_string(),
            system_app_code.to_string(),
            crate::app::AppConfig::default(),
            None,
        )
        .await
    {
        Ok(_) => info!("✅ 系统管理应用已加载 (app_id: _system)"),
        Err(e) => {
            tracing::error!("❌ 加载系统管理应用失败: {}", e);
            anyhow::bail!("Failed to load system app: {}", e);
        }
    }

    // 预加载测试应用（可选）
    let test_app_code = r#"
 export default function handler(context) {
   return new Response("Hello from x-deis-app-id: hello! Path: " + context.request.url);
 }
"#;
    match app_manager
        .load_app(
            "hello".to_string(),
            test_app_code.to_string(),
            crate::app::AppConfig::default(),
            None,
        )
        .await
    {
        Ok(_) => info!("✅ 测试应用已加载 (app_id: hello)"),
        Err(e) => tracing::warn!("⚠️ 加载测试应用失败: {}", e),
    }

    info!("====================================");
    info!("✅ Denix Platform is ready!");
    info!("📡 HTTP Server: http://localhost:{}", port);
    info!(
        "📡 gRPC Management: localhost:{} (content-type: application/grpc)",
        port
    );
    info!("🎯 New Routing: x-deis-app-id header based");
    info!(
        "📊 Health Check: curl -H \"x-deis-app-id: _system\" http://localhost:{}/health",
        port
    );
    info!(
        "📚 Platform Info: curl -H \"x-deis-app-id: _system\" http://localhost:{}/",
        port
    );
    info!("====================================");

    // Start server
    axum::serve(listener, app).await?;

    Ok(())
}
