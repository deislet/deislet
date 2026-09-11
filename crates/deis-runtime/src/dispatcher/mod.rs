// HTTP Gateway Module

pub mod auth;
pub mod handlers;
pub mod machine;

use crate::app::ApplicationManager;
use crate::deployment::DeploymentManager;
use crate::health::NodeHealth;
use crate::management::ManagementServer;
use crate::trigger::TriggerServer;
use anyhow::{Context, Result};
use auth::ManagementAuth;
use axum::Router;
pub use machine::MachineManagement;
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

    /// Largest request body this node will read into memory, from
    /// `max_request_body_bytes`. It reaches the handler through the state
    /// because that is the only thing a handler is given besides the request
    /// itself; it used to be read from the process environment inside the
    /// handler, which put a node setting somewhere `RuntimeConfig` could not
    /// reach and the node's TOML could not set.
    pub max_request_body_bytes: usize,
}

impl AppState {
    pub fn new(app_manager: Arc<ApplicationManager>, max_request_body_bytes: usize) -> Self {
        Self {
            app_manager,
            max_request_body_bytes,
        }
    }
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

    /// Largest request body this node will read into memory, from
    /// `max_request_body_bytes`. Carried here rather than read where it is
    /// used so that the value the node serves on is the one its configuration
    /// was validated against.
    pub max_request_body_bytes: usize,

    pub machine_management: Option<MachineManagement>,
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
        max_request_body_bytes,
        machine_management,
    } = options;

    let state = AppState::new(app_manager.clone(), max_request_body_bytes);

    // Create gRPC services
    let management_service = ManagementServer::create_service(
        app_manager.clone(),
        deployer,
        health,
        node_id,
        started_at,
    );
    let trigger_service = TriggerServer::create_service(app_manager.clone());

    // The entrance both gRPC services are dispatched from, built once per
    // listener that carries them.
    //
    // Still a factory, though there is one listener to build it for. What it
    // buys is that the dispatch below - which service answers which path, and
    // what an unknown path gets - is written once, so a second management
    // listener added later cannot quietly answer differently from this one.
    let entrance = |auth: ManagementAuth| {
        let management_service = management_service.clone();
        let trigger_service = trigger_service.clone();
        tower::service_fn(move |req: axum::extract::Request| {
            let mut management_service = management_service.clone();
            let mut trigger_service = trigger_service.clone();
            let management_auth = auth.clone();
            async move {
                let path = req.uri().path();
                // Nothing is checked here, and that is not an omission.
                //
                // The caller was identified before this request existed - it
                // proved a certificate naming this node's control plane during
                // the handshake, and `machine::ControlPlaneAcceptor` closed the
                // connection if the name was anything else. A request that
                // reaches this line arrived on a connection that could not have
                // been opened by anyone else, so reading a header would be
                // asking a proven caller to also say who it is.
                //
                // There used to be a credential check here, against a value the
                // whole fleet shared, and a refusal was answered as gRPC rather
                // than as a bare HTTP 401 so that whoever debugged it looked at
                // the credential instead of at the transport. Both the value and
                // the refusal are gone: a caller who cannot prove the name never
                // gets a connection to refuse on.
                let _ = &management_auth;
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
        })
    };

    // Build HTTP routes - every request is routed by the x-deis-app-id and
    // x-deis-environment headers.
    // gRPC services must be mounted on their explicit path prefixes BEFORE
    // the wildcard, otherwise `/*path` swallows every gRPC call and the
    // management plane is unreachable.
    //
    // Nothing administrative is mounted here. The two gRPC prefixes used to
    // be, on this same `0.0.0.0` port that public application traffic arrives
    // on, guarded by a value the whole fleet shared; both the value and that
    // door are gone. Their paths now fall through to the wildcard below and
    // are answered as any other request that names no application is - 400 for
    // want of an `x-deis-app-id` header - which is a public port having
    // nothing to say about a management call, not a management call being
    // refused.
    let app = Router::new()
        .route("/*path", axum::routing::any(handlers::handle_request))
        .route("/", axum::routing::any(handlers::handle_request))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    // Bind address
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    // The mutually authenticated management plane's socket, when this node
    // has one. Bound here for the same reason as the one above: before any
    // application is loaded, so an address already in use ends startup rather
    // than leaving a node that looks healthy while its control plane's
    // connections are refused by whatever else holds the port.
    //
    // The caller on this socket proved a certificate naming exactly this
    // node's control plane before the connection existed, so nothing on the
    // request is read - see `auth::ManagementAuth::for_machine_listener`.
    let machine_serving = match machine_management {
        Some(machine) => {
            let listener = std::net::TcpListener::bind(machine.listen).with_context(|| {
                format!(
                    "cannot bind the mutually authenticated management plane on \
                     machine_identity.management_listen {}",
                    machine.listen
                )
            })?;
            let auth = ManagementAuth::for_machine_listener(
                machine.expected_control.clone(),
                machine.listen,
            );
            let entrance = entrance(auth);
            let app = Router::new()
                .route_service("/deis.management.RuntimeManagement/*rpc", entrance.clone())
                .route_service("/deis.trigger.Trigger/*rpc", entrance)
                .layer(TraceLayer::new_for_http());
            Some((listener, app, machine))
        }
        None => None,
    };

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
        max_request_body_bytes,
        "📦 Request bodies larger than this are answered 413 and never reach an application"
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

    // The mutually authenticated plane drains on the same edge, and for the
    // same reason: a node that had stopped taking public traffic but kept
    // accepting deployment pushes would apply a version it is on its way to
    // not serving.
    let mut machine_drain = None;
    let mut machine_task = None;
    if let Some((listener, app, machine)) = machine_serving {
        let (tx, rx) = oneshot::channel::<()>();
        machine_drain = Some(tx);
        machine_task = Some(tokio::spawn(async move {
            // The outcome, good or bad, is logged inside `machine::serve`
            // against the address it was serving, which is more than could be
            // said here.
            let _ = machine::serve(
                listener,
                app,
                machine.identity,
                machine.expected_control,
                shutdown_grace,
                async move {
                    let _ = rx.await;
                },
            )
            .await;
        }));
    }

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
            if let Some(tx) = machine_drain.take() {
                let _ = tx.send(());
            }

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

    // Reached from both arms, so the management listener is closed whether the
    // node was signalled or the data listener stopped by itself. `drain` has
    // already been sent in the signal case; sending it here covers the other.
    if let Some(tx) = machine_drain.take() {
        let _ = tx.send(());
    }

    if let Some(task) = machine_task {
        // Awaited without a timeout of its own, which is where this differs
        // from the two listeners above. Their graceful shutdown is unbounded,
        // so the deadline has to be imposed from out here; `machine::serve`
        // is handed the grace period and stops on it from the inside. A
        // second deadline here would expire on the same edge as that one and
        // report, roughly half the time, a listener that had in fact drained.
        match task.await {
            Ok(()) => info!("The mutually authenticated management listener drained"),
            Err(e) => warn!(
                error = %e,
                "The mutually authenticated management listener did not finish cleanly; a \
                 deployment push or cron fire that was in flight may have been cut off, and the \
                 control plane will see it as a failure and retry"
            ),
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "handlers_test.rs"]
mod handlers_test;
