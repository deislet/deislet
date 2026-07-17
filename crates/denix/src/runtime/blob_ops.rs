use super::blob_client::BlobClient;
use deno_core::{extension, op2, OpState};
use std::cell::RefCell;
use std::rc::Rc;

#[op2(async)]
#[serde]
async fn op_blob_get(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<Option<Vec<u8>>, std::io::Error> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<BlobClient>().cloned()
    };

    if let Some(client) = client_opt {
        return client
            .get(key)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()));
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "Blob service not available",
    ))
}

#[op2(async)]
#[serde]
async fn op_blob_put(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[serde] data: Vec<u8>,
) -> Result<(), std::io::Error> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<BlobClient>().cloned()
    };

    if let Some(client) = client_opt {
        return client
            .put(key, data)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()));
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "Blob service not available",
    ))
}

#[op2(async)]
#[serde]
async fn op_blob_delete(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<(), std::io::Error> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<BlobClient>().cloned()
    };

    if let Some(client) = client_opt {
        return client
            .delete(key)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()));
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "Blob service not available",
    ))
}

extension!(
    denix_blob_ops,
    ops = [op_blob_get, op_blob_put, op_blob_delete],
);

pub fn init_blob_extension() -> deno_core::Extension {
    denix_blob_ops::init()
}
