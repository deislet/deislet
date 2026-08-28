//! Generated gRPC code.
//!
//! Server stubs are what the binary serves; client stubs exist so this
//! crate's own tests can drive the services over a real socket rather than
//! calling the handlers directly.

/// KV and SQL database, from `proto/storage.proto`.
pub mod storage {
    tonic::include_proto!("deis.storage");
}

/// Object storage, from `proto/blob.proto`.
pub mod blob {
    tonic::include_proto!("deis.blob");
}
