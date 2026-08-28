pub mod app;
pub mod app_repo;
pub mod config;
pub mod config_store;
pub mod dispatcher;
pub mod logging;
pub mod management;
pub mod resource;
pub mod runtime;
pub mod trigger;

/// Maximum gRPC message size (encode and decode) for every client and server
/// this crate builds. Tonic's built-in default is 4 MB, which is too small
/// once manifests and batched payloads are involved; every gRPC endpoint in
/// this crate uses this single constant instead of repeating the literal so
/// the limit stays consistent across clients and servers.
pub const MAX_GRPC_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Header that names the application a request belongs to.
///
/// The same string on both transports: deis-proxy stamps it on the HTTP data
/// path (`deis_proxy::proxy::APP_ID_HEADER`), and deis-store reads it out of
/// gRPC metadata (`deis_store::tenant::APP_ID_METADATA_KEY`). Storage calls
/// that omit it are refused with PermissionDenied, so every storage client in
/// this crate sends it on every request.
pub const APP_ID_HEADER: &str = "x-deis-app-id";
