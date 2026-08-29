// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::{APP_ID_HEADER, MAX_GRPC_MESSAGE_BYTES, STREAM_CHUNK_BYTES};

pub mod app;
pub mod app_repo;
pub mod assets;
pub mod config;
pub mod config_store;
pub mod deployment;
pub mod dispatcher;
pub mod health;
pub mod logging;
pub mod management;
pub mod resource;
pub mod runtime;
pub mod trigger;
