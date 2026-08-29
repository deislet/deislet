mod server;
mod service;

pub use server::TriggerServer;
pub use service::HANDLER_FAILED_PREFIX;

pub mod proto {
    tonic::include_proto!("deis.trigger");
}
