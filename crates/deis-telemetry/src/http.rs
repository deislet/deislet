//! The Prometheus scrape endpoint.
//!
//! It listens on its own port, separate from gRPC, so a scraper can be given
//! access to the exported numbers without being given access to the ingest
//! and query RPCs. Separate does not mean unguarded: the page carries every
//! application's metric series, so the same capability check runs here, as
//! one layer over the whole router rather than inside the handler - a second
//! route added later must not be able to become a second unguarded door.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tracing::{debug, error};

use crate::auth::TelemetryAuth;
use crate::now_us;
use crate::prom;
use crate::stats::{Access, ServiceStats};
use crate::store::TelemetryStore;

/// The exposition format version this endpoint speaks.
const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[derive(Clone)]
pub struct MetricsState {
    store: TelemetryStore,
    stats: Arc<ServiceStats>,
    /// How far back to look for the latest sample of each series.
    window_secs: u64,
    /// The capability verifier whose T8 state (`deis_auth_*`) is exported
    /// beside this process's own counters.
    verifier: Option<deis_capability::SharedVerifier>,
}

impl MetricsState {
    pub fn new(store: TelemetryStore, stats: Arc<ServiceStats>, window_secs: u64) -> Self {
        Self {
            store,
            stats,
            window_secs,
            verifier: None,
        }
    }

    /// Export `verifier`'s degraded-mode and grace metrics on every scrape.
    pub fn with_verifier(mut self, verifier: deis_capability::SharedVerifier) -> Self {
        self.verifier = Some(verifier);
        self
    }
}

/// Build the scrape router, behind the capability check.
pub fn router(state: MetricsState, auth: TelemetryAuth) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .layer(middleware::from_fn_with_state(auth, guard))
        .with_state(state)
}

/// Refuse anything on this port that does not present the token.
///
/// 401 with a `WWW-Authenticate` header rather than a bare 403, because the
/// caller is a scraper and that is the answer telling it which credential it
/// was supposed to send. With no secret configured this passes everything
/// through unchanged; see [`crate::auth`].
async fn guard(State(auth): State<TelemetryAuth>, request: Request, next: Next) -> Response {
    match auth.verify_headers(request.headers()).await {
        Ok(_) => return next.run(request).await,
        Err(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                [
                    (header::WWW_AUTHENTICATE, "Bearer"),
                    (header::CONTENT_TYPE, CONTENT_TYPE),
                ],
                "# deis-telemetry requires a capability token on this endpoint\n",
            )
                .into_response();
        }
    }
}

async fn scrape(State(state): State<MetricsState>) -> impl IntoResponse {
    ServiceStats::incr(&state.stats.metric_scrapes);
    let since_us = now_us() - (state.window_secs as i64).saturating_mul(1_000_000);
    let store = state.store.clone();
    // Both failures below are storage failures and are counted as such, so
    // that `storage_failures_total` is one number covering every path into
    // the database. The scrape-specific counter is what tells an operator
    // reading a later, successful scrape that earlier ones returned nothing -
    // a 500 carries no body a scraper keeps, so without it the only trace of
    // a failed export is a hole in somebody else's graph.
    let samples = match tokio::task::spawn_blocking(move || store.latest_metric_samples(since_us))
        .await
    {
        Ok(Ok(samples)) => {
            state.stats.store_health.record_success(Access::Read);
            samples
        }
        Ok(Err(e)) => {
            ServiceStats::incr(&state.stats.storage_failures);
            ServiceStats::incr(&state.stats.metric_scrapes_failed);
            state
                .stats
                .store_health
                .record_failure(Access::Read, "read metric samples for export");
            error!(error = ?e, "cannot read metric samples, this scrape returns no data and the series will show a gap");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, CONTENT_TYPE)],
                "# deis-telemetry could not read its metric store\n".to_string(),
            );
        }
        Err(e) => {
            ServiceStats::incr(&state.stats.storage_failures);
            ServiceStats::incr(&state.stats.metric_scrapes_failed);
            state
                .stats
                .store_health
                .record_failure(Access::Read, "read metric samples for export");
            error!(error = %e, "metric export worker panicked or was cancelled, this scrape returns no data");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, CONTENT_TYPE)],
                "# deis-telemetry could not complete the export\n".to_string(),
            );
        }
    };
    debug!(
        series = samples.len(),
        window_secs = state.window_secs,
        "serving a Prometheus scrape"
    );
    let mut body = prom::render(&samples, &state.stats);
    if let Some(verifier) = &state.verifier {
        body.push_str(
            &verifier
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .render_metrics("telemetry"),
        );
    }
    (StatusCode::OK, [(header::CONTENT_TYPE, CONTENT_TYPE)], body)
}
