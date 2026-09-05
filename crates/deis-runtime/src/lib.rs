// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
//
// `APP_ID_HEADER` says which application a request is for and
// `ENVIRONMENT_HEADER` says which environment of it; absent means
// `DEFAULT_ENVIRONMENT`, so a caller that only knows about applications still
// reaches production.
pub use deis_wire::conventions::{
    APP_ID_HEADER, DEFAULT_ENVIRONMENT, DEPLOYMENT_GENERATION_HEADER, ENVIRONMENT_HEADER,
    MAX_GRPC_MESSAGE_BYTES, STREAM_CHUNK_BYTES,
};

/// The prefix the platform reserves for headers it writes about itself.
///
/// Everything under it is one component of this platform telling another
/// something, and neither a client nor an application is part of that
/// conversation: `x-deis-app-id` and `x-deis-environment` are a routing
/// decision, `x-deis-caller` is this node's record of which application made a
/// service call, and `x-deis-tenant`, `x-deis-binding` and
/// `x-deis-storage-token` name and authorise a storage tenant. They are
/// documented as evidence a handler may branch on rather than a claim it has
/// to verify, and that is only true while nothing outside the platform can
/// write one. So every way into a handler drops this whole namespace and puts
/// back only what the platform itself decided: see
/// `dispatcher::handlers::handle_request` for a request off the wire and
/// `runtime::service_ops` for one application calling another.
///
/// A prefix rather than a list of names, because a list is only right until
/// the next header is added: the header a future component invents would
/// arrive forgeable, and nothing would fail until something trusted it.
///
/// deis-proxy spells the same string as `proxy::PLATFORM_HEADER_PREFIX` and
/// applies the same rule at the edge; deis-bundle spells it as
/// `headers::PLATFORM_PREFIX` and refuses a bundle that sets one of these on a
/// *response*. The copies exist because those crates do not depend on this
/// one; one shared constant belongs in `deis_wire::conventions`, beside
/// [`APP_ID_HEADER`].
pub const PLATFORM_HEADER_PREFIX: &str = "x-deis-";

/// Correlates one answered HTTP request with its invocation trace.
///
/// The application never supplies this value. The isolate removes the whole
/// platform header namespace from the application's answer and stamps the id
/// that its log lines and telemetry record carry. Provider tooling can then
/// query the exact invocation instead of guessing from timestamps or URLs.
pub const TRACE_ID_HEADER: &str = "x-deis-trace-id";

/// Whether a header name is in the platform's own namespace.
///
/// Case-insensitive, because a header name is: a client sending
/// `X-Deis-Caller` and an application putting `X-Deis-Caller` on a service
/// call both mean the same header, and both end up lower-cased in the
/// `Headers` the handler reads. A comparison that missed the spelling would
/// be a way through the sweep rather than an untidiness.
pub fn is_platform_header(name: &str) -> bool {
    name.get(..PLATFORM_HEADER_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PLATFORM_HEADER_PREFIX))
}

pub mod app;
pub mod app_repo;
pub mod assets;
pub mod config;
pub mod deployment;
pub mod dispatcher;
pub mod egress_proxy;
pub mod health;
pub mod logging;
pub mod management;
pub mod resource;
pub mod runtime;
pub mod telemetry;
pub mod trigger;
