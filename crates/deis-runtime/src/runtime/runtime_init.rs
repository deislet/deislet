use anyhow::Result;
use deno_core::{Extension, JsRuntime};
use deno_runtime::{deno_crypto, deno_fetch, deno_net, deno_web, deno_webidl, deno_websocket};
use std::sync::{Arc, Once};

use super::blob_ops::init_blob_extension;
use super::bootstrap::init_bootstrap_extension;
use super::cache_ops::init_cache_extension;
use super::db_ops::init_db_extension;
use super::deno_telemetry_stub::init_deno_telemetry_extension;
use super::fetch_ops::init_fetch_guard_extension;
use super::fetch_policy::{FetchPolicy, PolicyResolver};
use super::ops::init_ops_extension;
use super::queue_ops::init_queue_extension;
use super::tls_ops::init_tls_ops_extension;
use super::ws_server_ops::init_ws_server_extension;
use assets_ops::init_assets_extension;

#[path = "assets_ops.rs"]
mod assets_ops;

#[path = "ws_client_ops.rs"]
mod ws_client_ops;

pub fn build_runtime_extensions() -> Vec<Extension> {
    vec![
        init_bootstrap_extension(),
        init_deno_telemetry_extension(),
        deno_webidl::deno_webidl::init(),
        deno_web::deno_web::lazy_init(),
        deno_fetch::deno_fetch::lazy_init(),
        deno_websocket::deno_websocket::lazy_init(),
        deno_crypto::deno_crypto::lazy_init(),
        deno_runtime::deno_tls::deno_tls::init(),
        init_tls_ops_extension(),
        deno_net::deno_net::lazy_init(),
        init_ops_extension(),
        init_fetch_guard_extension(),
        init_cache_extension(),
        init_db_extension(),
        init_blob_extension(),
        init_assets_extension(),
        init_ws_server_extension(),
        ws_client_ops::init_ws_client_extension(),
        init_queue_extension(),
    ]
}

/// Finish the extensions that need values only the caller has.
///
/// `policy` and `app_id` are what the outbound-fetch policy is built from. The
/// policy is installed in two places on purpose, and both are needed:
/// `js/bootstrap.js` calls `op_deis_fetch_guard` before every `fetch()`, which
/// is where a refusal gets its log line and its `TypeError`, and the resolver
/// below is inside the HTTP client, where it filters the addresses the socket
/// may actually be opened to. See [`super::fetch_policy`] for why one without
/// the other is not enough.
pub fn init_lazy_extensions(
    runtime: &mut JsRuntime,
    policy: &FetchPolicy,
    app_id: &str,
) -> Result<()> {
    install_tls_crypto_provider();

    let blob_store = Arc::new(deno_web::BlobStore::default());
    let broadcast_channel = deno_web::InMemoryBroadcastChannel::default();

    runtime.lazy_init_extensions(vec![
        deno_web::deno_web::args(
            blob_store,
            Some(deno_core::url::Url::parse("file:///").unwrap()),
            // The CSS parser behind the geometry types (DOMMatrix and the
            // `transform` strings it can be built from) stays off. Nothing on
            // an edge handler's path parses a stylesheet, and it is a parser
            // reached from application input, so leaving it out is one less
            // surface for no capability an application here would miss.
            false,
            broadcast_channel,
        ),
        deno_fetch::deno_fetch::args(deno_fetch::Options {
            user_agent: "deis_runtime/1.0".to_string(),
            root_cert_store_provider: None,
            resolver: PolicyResolver::new(policy.clone(), app_id).into_resolver(),
            ..Default::default()
        }),
        deno_websocket::deno_websocket::args(),
        deno_crypto::deno_crypto::args(None),
        deno_net::deno_net::args(None, None),
    ])?;

    Ok(())
}

/// Give rustls its cipher suites, once per process.
///
/// rustls has no default provider of its own: whoever embeds it has to install
/// one, and Deno's own binary does it in `main`. This runtime has no such
/// place, and the consequence of leaving it out is not a failed request. The
/// provider is looked up while deno_fetch builds its HTTPS client, deep inside
/// `op_fetch`, and it *panics* there -- inside a V8 callback, which cannot
/// unwind, so the panic aborts the process. One application's first
/// `fetch("https://...")` would take the whole node down and every application
/// on it with it.
///
/// Installed here because this function runs for every isolate that is built
/// and for the startup snapshot, so there is no path to a working fetch that
/// skips it. `install_default` refuses a second install; the result is ignored
/// because "someone already installed one" is the outcome this wants.
fn install_tls_crypto_provider() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let installed = deno_runtime::deno_tls::rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .is_ok();
        tracing::debug!(
            installed,
            "rustls crypto provider ready for outbound fetch (installed by this process: {})",
            installed
        );
    });
}
