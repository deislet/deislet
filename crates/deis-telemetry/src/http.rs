//! The Prometheus scrape endpoint.
//!
//! It listens on its own port, separate from gRPC, so a scraper can be given
//! access to the exported numbers without being given access to the ingest
//! and query RPCs.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use tracing::{debug, error};

use crate::now_us;
use crate::prom;
use crate::stats::ServiceStats;
use crate::store::TelemetryStore;

/// The exposition format version this endpoint speaks.
const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[derive(Clone)]
pub struct MetricsState {
    store: TelemetryStore,
    stats: Arc<ServiceStats>,
    /// How far back to look for the latest sample of each series.
    window_secs: u64,
}

impl MetricsState {
    pub fn new(store: TelemetryStore, stats: Arc<ServiceStats>, window_secs: u64) -> Self {
        Self {
            store,
            stats,
            window_secs,
        }
    }
}

/// Build the scrape router.
pub fn router(state: MetricsState) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(state)
}

async fn scrape(State(state): State<MetricsState>) -> impl IntoResponse {
    let since_us = now_us() - (state.window_secs as i64).saturating_mul(1_000_000);
    let store = state.store.clone();
    let samples = match tokio::task::spawn_blocking(move || store.latest_metric_samples(since_us))
        .await
    {
        Ok(Ok(samples)) => samples,
        Ok(Err(e)) => {
            error!(error = ?e, "cannot read metric samples, this scrape returns no data and the series will show a gap");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, CONTENT_TYPE)],
                "# deis-telemetry could not read its metric store\n".to_string(),
            );
        }
        Err(e) => {
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
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, CONTENT_TYPE)],
        prom::render(&samples, &state.stats),
    )
}
