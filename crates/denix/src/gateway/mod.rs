// HTTP 网关模块

pub mod handlers;

use crate::app::ApplicationManager;
use crate::config_store::ConfigStore;
use crate::management::ManagementServer;
use anyhow::Result;
use axum::{
    body::Body,
    http::{Request, header::CONTENT_TYPE},
    routing::{delete, get, post, put},
    Router,
};
use std::sync::Arc;
use tower::{Service, ServiceExt};
use tower_http::trace::TraceLayer;
use tracing::info;

/// 应用状态
#[derive(Clone)]
pub struct AppState {
    pub app_manager: Arc<ApplicationManager>,
}

/// 启动 HTTP + gRPC 服务器（复用同一端口）
pub async fn start_server(
    app_manager: Arc<ApplicationManager>,
    config_store: Arc<ConfigStore>,
    worker_id: String,
    port: u16,
) -> Result<()> {
    let state = AppState {
        app_manager: app_manager.clone(),
    };

    // 创建 gRPC 管理服务
    let grpc_service = ManagementServer::create_service(
        app_manager.clone(),
        config_store,
        worker_id,
    );

    // 构建 HTTP 路由
    let http_router = Router::new()
        // 根路径
        .route("/", get(handlers::handle_root))
        // 健康检查
        .route("/health", get(handlers::handle_health))
        // 管理 API
        .route("/api/apps", post(handlers::handle_create_app))
        .route("/api/apps", get(handlers::handle_list_apps))
        .route("/api/apps/:id", get(handlers::handle_get_app))
        .route("/api/apps/:id", put(handlers::handle_update_app))
        .route("/api/apps/:id", delete(handlers::handle_delete_app))
        // 应用路由 - 必须最后，因为它是通配符
        .route("/apps/:id", get(handlers::handle_app_request))
        .route("/apps/:id", post(handlers::handle_app_request))
        .route("/apps/:id", put(handlers::handle_app_request))
        .route("/apps/:id", delete(handlers::handle_app_request))
        .route("/apps/:id/*path", get(handlers::handle_app_request))
        .route("/apps/:id/*path", post(handlers::handle_app_request))
        .route("/apps/:id/*path", put(handlers::handle_app_request))
        .route("/apps/:id/*path", delete(handlers::handle_app_request))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    // 合并 HTTP 和 gRPC 服务
    // gRPC 请求通过 content-type: application/grpc 识别，不会与 HTTP 路由冲突
    let app = http_router.fallback_service(grpc_service);

    // 绑定地址
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!("====================================");
    info!("✅ Denix Platform is ready!");
    info!("📡 HTTP Server: http://localhost:{}", port);
    info!("📡 gRPC Management: localhost:{} (content-type: application/grpc)", port);
    info!("📊 Health Check: http://localhost:{}/health", port);
    info!("📚 API Docs: http://localhost:{}/", port);
    info!("====================================");

    // 启动服务器
    axum::serve(listener, app).await?;

    Ok(())
}
