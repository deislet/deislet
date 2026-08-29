//! `ctx.services.blob`: object storage, backed by deis-store's blob service.
//!
//! Errors are returned as [`JsErrorBox`] rather than `std::io::Error`, because
//! only the former's message survives the trip into JavaScript; see
//! `super::ops::service_unavailable`.

use super::blob_client::BlobClient;
use super::ops::{service_call_failed, service_unavailable};
use deno_core::{extension, op2, OpState};
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

#[op2(async)]
#[serde]
async fn op_blob_get(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<Option<Vec<u8>>, JsErrorBox> {
    client(&state)?
        .get(key)
        .await
        .map_err(|e| service_call_failed("blob.get", e))
}

#[op2(async)]
#[serde]
async fn op_blob_put(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[serde] data: Vec<u8>,
) -> Result<(), JsErrorBox> {
    client(&state)?
        .put(key, data)
        .await
        .map_err(|e| service_call_failed("blob.put", e))
}

#[op2(async)]
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
