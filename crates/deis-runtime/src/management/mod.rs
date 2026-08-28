// Management service module - gRPC interface for supervisor to call

mod server;
mod service;

pub use server::ManagementServer;
pub use service::RuntimeManagementService;

// Generated protobuf code
pub mod proto {
    tonic::include_proto!("deis.management");
}
