use anyhow::Result;
use deno_core::{Extension, JsRuntime};
use deno_runtime::{deno_crypto, deno_fetch, deno_net, deno_web, deno_webidl, deno_websocket};
use std::sync::Arc;

use super::blob_ops::init_blob_extension;
use super::cache_ops::init_cache_extension;
use super::db_ops::init_db_extension;
use super::denix_bootstrap::init_denix_bootstrap_extension;
use super::ops::init_ops_extension;
use super::queue_ops::init_queue_extension;
use super::telemetry_stub::init_telemetry_extension;
use super::tls_ops::init_tls_ops_extension;
use super::ws_server_ops::init_ws_server_extension;

pub fn build_runtime_extensions() -> Vec<Extension> {
    vec![
        init_denix_bootstrap_extension(),
        init_telemetry_extension(),
        deno_webidl::deno_webidl::init(),
        deno_web::deno_web::lazy_init(),
        deno_fetch::deno_fetch::lazy_init(),
        deno_websocket::deno_websocket::lazy_init(),
        deno_crypto::deno_crypto::lazy_init(),
        deno_runtime::deno_tls::deno_tls::init(),
        init_tls_ops_extension(),
        deno_net::deno_net::lazy_init(),
        init_ops_extension(),
        init_cache_extension(),
        init_db_extension(),
        init_blob_extension(),
        init_ws_server_extension(),
        init_queue_extension(),
    ]
}

pub fn init_lazy_extensions(runtime: &mut JsRuntime) -> Result<()> {
    let blob_store = Arc::new(deno_web::BlobStore::default());
    let broadcast_channel = deno_web::InMemoryBroadcastChannel::default();

    runtime.lazy_init_extensions(vec![
        deno_web::deno_web::args(
            blob_store,
            Some(deno_core::url::Url::parse("file:///").unwrap()),
            broadcast_channel,
        ),
        deno_fetch::deno_fetch::args(deno_fetch::Options {
            user_agent: "denix/1.0".to_string(),
            root_cert_store_provider: None,
            ..Default::default()
        }),
        deno_websocket::deno_websocket::args(),
        deno_crypto::deno_crypto::args(None),
        deno_net::deno_net::args(None, None),
    ])?;

    Ok(())
}
