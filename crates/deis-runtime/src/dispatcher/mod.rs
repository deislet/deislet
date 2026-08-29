// HTTP Gateway Module

pub mod handlers;

use crate::app::ApplicationManager;
use crate::deployment::DeploymentManager;
use crate::health::NodeHealth;
use crate::management::ManagementServer;
use crate::trigger::TriggerServer;
use anyhow::Result;
use axum::Router;
use std::future::IntoFuture;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

/// Application State
#[derive(Clone)]
pub struct AppState {
    pub app_manager: Arc<ApplicationManager>,
}

/// Everything `start_server` needs to stand the node up.
///
/// Grouped rather than passed positionally so that adding a knob does not turn
/// into another same-typed argument at a call site.
pub struct ServerOptions {
    pub app_manager: Arc<ApplicationManager>,
    /// Takes delivery of deployments pushed to this node.
    pub deployer: Arc<DeploymentManager>,
    /// Readiness, which the `HealthCheck` RPC reports.
    pub health: Arc<NodeHealth>,
    pub node_id: String,
    pub port: u16,

    /// When this node's process started. Passed in rather than taken here so
    /// that the uptime the management API reports is the process's, not the
    /// listener's.
    pub started_at: std::time::Instant,

    /// How long in-flight requests get to finish after a shutdown signal
    /// before the node stops waiting.
    pub shutdown_grace: Duration,
}

/// Wait for the first shutdown signal and name it.
///
/// SIGTERM is what a supervisor (systemd, a container runtime, `deis` itself)
/// sends on a restart or a rolling deploy, and SIGINT is Ctrl-C in a terminal.
/// Both mean the same thing here: stop taking new work, let what is running
/// finish.
async fn shutdown_signal() -> &'static str {
    let interrupt = async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => "SIGINT",
            Err(e) => {
                error!(
                    error = %e,
                    "Could not install the SIGINT handler; Ctrl-C will kill this node \
                     outright instead of draining, and in-flight requests will be \
                     cut off"
                );
                std::future::pending().await
            }
        }
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
                "SIGTERM"
            }
            Err(e) => {
                error!(
                    error = %e,
                    "Could not install the SIGTERM handler; a supervisor restart will kill \
                     this node outright instead of draining, and in-flight requests \
                     will be cut off"
                );
                std::future::pending().await
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<&'static str>();

    tokio::select! {
        signal = interrupt => signal,
        signal = terminate => signal,
    }
}

/// Start HTTP + gRPC server (reuse the same port) and serve until a shutdown
/// signal arrives.
///
/// Returns once the listener is closed and either every in-flight request has
/// finished or the grace period has run out.
pub async fn start_server(options: ServerOptions) -> Result<()> {
    let ServerOptions {
        app_manager,
        deployer,
        health,
        node_id,
        port,
        started_at,
        shutdown_grace,
    } = options;

    let state = AppState {
        app_manager: app_manager.clone(),
    };

    // Create gRPC services
    let management_service = ManagementServer::create_service(
        app_manager.clone(),
        deployer,
        health,
        node_id,
        started_at,
    );
    let trigger_service = TriggerServer::create_service(app_manager.clone());

    // Combine gRPC services
    let grpc_service = tower::service_fn(move |req: axum::extract::Request| {
        let mut management_service = management_service.clone();
        let mut trigger_service = trigger_service.clone();
        async move {
            let path = req.uri().path();
            if path.starts_with("/deis.management.") {
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
            } else if path.starts_with("/deis.trigger.") {
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

    // Build HTTP routes - every request is routed by the x-deis-app-id and
    // x-deis-environment headers.
    // gRPC services must be mounted on their explicit path prefixes BEFORE
    // the wildcard, otherwise `/*path` swallows every gRPC call and the
    // management plane is unreachable.
    let app = Router::new()
        .route_service(
            "/deis.management.RuntimeManagement/*rpc",
            grpc_service.clone(),
        )
        .route_service("/deis.trigger.Trigger/*rpc", grpc_service)
        .route("/*path", axum::routing::any(handlers::handle_request))
        .route("/", axum::routing::any(handlers::handle_request))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    // Bind address
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    // Preload system management application. It is not a deployment and has
    // no previews, so it lives in the default environment and is reached by a
    // request that names no environment at all.
    let system_app_code = include_str!("system_app.js");
    match app_manager
        .load_app(
            crate::app::AppKey::production("_system"),
            system_app_code.to_string(),
            crate::app::AppConfig::default(),
            None,
        )
        .await
    {
        Ok(_) => info!("✅ System management app loaded (app_id: _system)"),
        Err(e) => {
            tracing::error!("❌ Failed to load system management app: {}", e);
            anyhow::bail!("Failed to load system app: {}", e);
        }
    }

    info!("====================================");
    info!("✅ Runtime Platform is ready!");
    info!("📡 HTTP Server: http://localhost:{}", port);
    info!(
        "📡 gRPC Management: localhost:{} (content-type: application/grpc)",
        port
    );
    info!(
        "🎯 Routing: {} names the application, {} the environment (absent = {})",
        crate::APP_ID_HEADER,
        crate::ENVIRONMENT_HEADER,
        crate::DEFAULT_ENVIRONMENT
    );
    info!(
        "📊 Health Check: curl -H \"x-deis-app-id: _system\" http://localhost:{}/health",
        port
    );
    info!(
        "📚 Platform Info: curl -H \"x-deis-app-id: _system\" http://localhost:{}/",
        port
    );
    info!("====================================");

    // Serve until a signal arrives, then drain.
    //
    // `with_graceful_shutdown` stops accepting new connections and lets the
    // ones already in flight run to completion, which is what keeps an isolate
    // from being torn down half way through a request. The deadline exists so
    // that one stuck handler cannot hold the whole node open forever.
    let (drain_tx, drain_rx) = oneshot::channel::<()>();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = drain_rx.await;
        })
        .into_future();
    tokio::pin!(server);

    tokio::select! {
        result = &mut server => {
            result?;
            info!("HTTP server stopped on its own");
        }
        signal = shutdown_signal() => {
            info!(
                signal,
                grace_secs = shutdown_grace.as_secs(),
                "Shutdown signal received; no longer accepting connections, in-flight requests \
                 have the grace period to finish"
            );
            let _ = drain_tx.send(());

            match tokio::time::timeout(shutdown_grace, &mut server).await {
                Ok(Ok(())) => info!("All in-flight requests finished; the node drained cleanly"),
                Ok(Err(e)) => warn!(
                    error = %e,
                    "The server reported an error while draining; some requests may have been cut \
                     off"
                ),
                Err(_) => error!(
                    grace_secs = shutdown_grace.as_secs(),
                    "Grace period expired with requests still running; those isolates are being \
                     torn down mid-request and their clients will see a dropped connection"
                ),
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "handlers_test.rs"]
mod handlers_test;
