//! `ctx.services.blob`: object storage, backed by deis-store's blob service.
//!
//! Errors are returned as [`JsErrorBox`] rather than `std::io::Error`, because
//! only the former's message survives the trip into JavaScript; see
//! `super::ops::service_unavailable`.

use super::blob_client::BlobClient;
use super::ops::{service_call_failed, service_unavailable};
use deno_core::{extension, op2, JsBuffer, OpState, ToJsBuffer};
use deno_error::JsErrorBox;
use std::cell::RefCell;
use std::rc::Rc;

fn client(state: &Rc<RefCell<OpState>>) -> Result<BlobClient, JsErrorBox> {
    let state_borrow = state.borrow();
    state_borrow
        .try_borrow::<BlobClient>()
        .cloned()
        .ok_or_else(|| service_unavailable("blob"))
}

/// Answers a `Uint8Array`, or null when the object is not there.
///
/// The bytes cross as a buffer rather than through `#[serde]`. Serde turns a
/// `Vec<u8>` into a JavaScript array of numbers, so `blob.get` used to hand
/// the application something that looked like data but had no `.byteLength`,
/// could not be sliced and cost one JS object per byte.
#[op2]
#[serde]
async fn op_blob_get(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<Option<ToJsBuffer>, JsErrorBox> {
    let found = client(&state)?
        .get(key)
        .await
        .map_err(|e| service_call_failed("blob.get", e))?;
    Ok(found.map(ToJsBuffer::from))
}

/// Takes a `Uint8Array` (or any `ArrayBufferView`) straight from V8.
///
/// `#[serde] Vec<u8>` here refused every typed array with "invalid type", so
/// the only way through was an array of plain numbers - which is not what any
/// caller building bytes would pass.
#[op2]
#[serde]
async fn op_blob_put(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[buffer] data: JsBuffer,
) -> Result<(), JsErrorBox> {
    client(&state)?
        .put(key, data.to_vec())
        .await
        .map_err(|e| service_call_failed("blob.put", e))
}

#[op2]
#[serde]
async fn op_blob_delete(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<(), JsErrorBox> {
    client(&state)?
        .delete(key)
        .await
        .map_err(|e| service_call_failed("blob.delete", e))
}

extension!(
    deis_runtime_blob_ops,
    ops = [op_blob_get, op_blob_put, op_blob_delete],
);

pub fn init_blob_extension() -> deno_core::Extension {
    deis_runtime_blob_ops::init()
}
