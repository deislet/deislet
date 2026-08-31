//! `env.ASSETS.fetch()`: a function asking for its own version's static files.
//!
//! The other half of asset-first. The HTTP entry consults
//! [`crate::assets::AssetBundle`] *before* it starts an isolate, so a path that
//! names a file never costs one; this is the path back in, for the handler that
//! ran anyway and wants the asset layer's answer for a path of its own
//! choosing - a server-rendered page falling back to the SPA shell, a handler
//! that authenticates a request and then serves the file behind it, a catch-all
//! that answers `/` from `index.html`.
//!
//! # Why it is a binding and not a service
//!
//! `ctx.services.*` is what an application *declares*: a manifest asks for kv
//! or a queue, the control plane hands down an address, and a client is dialled.
//! Nothing is declared or dialled here. The bundle is already in memory, it
//! belongs to this exact version, and every application has one. So it arrives
//! the way Cloudflare's does, as a name on the environment table -
//! `env.ASSETS` - and `js/runtime.js` installs it there.
//!
//! # Why the name cannot be taken
//!
//! [`crate::runtime::bindings::ASSETS_BINDING_KEY`] is a reserved env key, removed from
//! a deployment's map before the map becomes `ctx.env`, exactly as the storage
//! endpoints are. An application that ships `ASSETS=whatever` gets the binding,
//! not its string, and cannot decide by ordering which of the two an author's
//! `env.ASSETS.fetch()` reaches.
//!
//! # What "the answer" is
//!
//! [`crate::assets::AssetBundle::fetch_asset`] applies the same pipeline the
//! HTTP entry uses - the asset index, `_redirects`, then the not-found
//! behaviour - with the one difference that the function routes have already
//! had their turn, and always answers something. So this op never fails: a
//! path that names nothing comes back as a 404 the handler can return, rather
//! than as an exception every handler would have to invent a response for.

use std::sync::Arc;

use deno_core::{extension, op2, OpState, ToJsBuffer};
use serde::Serialize;

use crate::assets::AssetBundle;
use crate::runtime::ops::AppId;

/// One answer, in the shape `js/runtime.js` turns into a `Response`.
///
/// The body is a buffer rather than a `#[serde] Vec<u8>` for the reason
/// `blob_ops.rs` records: serde hands JavaScript an array of numbers, which
/// has no `byteLength`, cannot be sliced, and costs one object per byte. An
/// asset is arbitrary bytes - a PNG, a font, a gzip frame - so it crosses as
/// bytes.
#[derive(Serialize)]
struct FetchedAsset {
    status: u16,
    /// Lower-case names in the order they should be set, `_headers` already
    /// applied. A `Headers` built from these pairs is what the site declared.
    headers: Vec<(String, String)>,
    body: ToJsBuffer,
}

/// Answer one `env.ASSETS.fetch()`.
///
/// `target` is the path and query of the request the caller passed, which is
/// all the asset layer reads: the host is the caller's own, and a bundle is
/// only ever asked about its own paths.
///
/// Synchronous because nothing is awaited - the bytes have been in memory
/// since the version loaded. The JavaScript side still hands back a promise,
/// so an author writes `await env.ASSETS.fetch(request)` the way they would
/// against any other `fetch`.
#[op2]
#[serde]
fn op_deis_assets_fetch(state: &mut OpState, #[string] target: String) -> FetchedAsset {
    let app_id = state
        .try_borrow::<AppId>()
        .map(|id| id.0.clone())
        .unwrap_or_default();

    let Some(bundle) = state.try_borrow::<Arc<AssetBundle>>().cloned() else {
        // An isolate built by a path that carries no bundle: the system app,
        // and anything loaded through `ApplicationManager::load_app` rather
        // than as a deployed version. Answering 404 is what an empty bundle
        // would answer, so a handler written against the binding behaves the
        // same either way instead of having to test for its absence.
        tracing::debug!(
            app_id = %app_id,
            target = %target,
            "env.ASSETS.fetch() was called in an isolate that was loaded without an asset \
             bundle, so it answers 404; only a deployed version carries one"
        );
        return FetchedAsset {
            status: 404,
            headers: vec![(
                "content-type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            )],
            body: ToJsBuffer::from(b"Not Found".to_vec()),
        };
    };

    let answer = bundle.fetch_asset(&target);
    tracing::debug!(
        app_id = %app_id,
        target = %target,
        status = answer.status,
        bytes = answer.body.len(),
        "Answered env.ASSETS.fetch() from this version's bundle"
    );
    FetchedAsset {
        status: answer.status,
        headers: answer.headers,
        body: ToJsBuffer::from(answer.body.to_vec()),
    }
}

extension!(deis_assets_ops, ops = [op_deis_assets_fetch],);

/// The extension that carries `env.ASSETS`'s only op.
pub fn init_assets_extension() -> deno_core::Extension {
    deis_assets_ops::init()
}

#[cfg(test)]
mod tests {
    use crate::runtime::bindings::ASSETS_BINDING_KEY;

    /// The binding's name is spelled in Rust, where it is reserved, and in
    /// JavaScript, where it is installed. Nothing type-checks across that
    /// boundary, and the two spellings failing to agree is silent in both
    /// directions: the key would reach the application as an ordinary
    /// variable, and the binding would be published under a name no author
    /// writes.
    #[test]
    fn the_runtime_installs_the_binding_under_the_name_rust_reserves() {
        let runtime_js = include_str!("js/runtime.js");
        assert!(
            runtime_js.contains(&format!("\"{ASSETS_BINDING_KEY}\"")),
            "js/runtime.js does not mention {ASSETS_BINDING_KEY:?}, so the reserved key and the \
             installed binding are not the same name"
        );
    }
}
