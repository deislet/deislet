//! Generated gRPC code.
//!
//! Server stubs are what the binary serves; client stubs exist so this
//! crate's own tests can drive the service over a real socket rather than
//! calling the handlers directly.

/// The queue service, from `proto/queue.proto`.
pub mod queue {
    tonic::include_proto!("deis.queue");
}
