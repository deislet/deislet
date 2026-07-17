mod server;
mod service;

pub use server::TriggerServer;

pub mod proto {
    tonic::include_proto!("denix.trigger");
}
