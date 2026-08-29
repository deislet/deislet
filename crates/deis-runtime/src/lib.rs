// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
//
// `APP_ID_HEADER` says which application a request is for and
// `ENVIRONMENT_HEADER` says which environment of it; absent means
// `DEFAULT_ENVIRONMENT`, so a caller that only knows about applications still
// reaches production.
pub use deis_wire::conventions::{
    APP_ID_HEADER, DEFAULT_ENVIRONMENT, ENVIRONMENT_HEADER, MAX_GRPC_MESSAGE_BYTES,
    STREAM_CHUNK_BYTES,
};

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
