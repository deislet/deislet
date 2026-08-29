//! Conventions every Deislet component agrees on when talking to another one.
//!
//! These are deliberately plain constants rather than a trait or a builder:
//! their whole purpose is that six independent binaries spell the same thing
//! the same way. A component that disagrees here fails at runtime in ways that
//! are tedious to trace - a request rejected for the wrong reason, or a payload
//! truncated on one side of a link that the other side considers fine.
//!
//! Services that carry no control-plane concepts (deis-store, deis-queue,
//! deis-telemetry) depend on this crate for this module alone.

/// Header carrying the tenant an inbound request belongs to.
///
/// Used in two places with the same spelling: as an HTTP header on the data
/// path (deis-proxy stamps it, deis-runtime reads it) and as gRPC metadata on
/// storage calls (deis-runtime stamps it, deis-store reads it). It is the only
/// tenant dimension the storage services understand, so a call without it is
/// rejected rather than served from some default namespace.
pub const APP_ID_HEADER: &str = "x-deis-app-id";

/// Ceiling for a single gRPC message, applied to both directions on every
/// service in the platform.
///
/// tonic defaults to 4 MB, which is too tight for batched log pushes and for
/// bundle manifests. Anything genuinely large - an application bundle, a blob -
/// is streamed instead of being sent as one message, so this ceiling bounds
/// per-message memory rather than per-object size.
pub const MAX_GRPC_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Chunk size for streamed payloads (blob upload and download).
///
/// Small enough that a chunk is cheap to buffer on either side, large enough
/// that a big object does not pay per-message overhead thousands of times.
pub const STREAM_CHUNK_BYTES: usize = 1024 * 1024;

/// Deployment-env key carrying the deis-store gRPC address, e.g.
/// `http://127.0.0.1:8090`.
///
/// The control plane owns the addresses of the storage services and hands
/// them to a node in the `env` map of an `ApplyDeployment`
/// (docs/architecture.zh.md section 6: the address is *not* written into a
/// node's own configuration file). deis-runtime lifts this key back out of
/// the map before an application sees it, so an app cannot point its own
/// storage client somewhere else by declaring a variable of the same name.
///
/// It lives here rather than in either component because both ends have to
/// spell it identically; a disagreement would show up as an application whose
/// `ctx.services.kv` silently answers "service not available".
pub const STORE_ENDPOINT_ENV: &str = "DEIS_STORE_ENDPOINT";

/// Deployment-env key carrying the deis-queue gRPC address, e.g.
/// `http://127.0.0.1:8091`. Same contract as [`STORE_ENDPOINT_ENV`].
pub const QUEUE_ENDPOINT_ENV: &str = "DEIS_QUEUE_ENDPOINT";

/// A single chunk plus its framing must never approach the ceiling, otherwise
/// streaming would start failing on the transport instead of being bounded by
/// our own logic. Checked at compile time so it cannot drift.
const _: () = assert!(STREAM_CHUNK_BYTES * 2 < MAX_GRPC_MESSAGE_BYTES);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endpoint_keys_look_like_environment_variables() {
        // They travel in the same map as an application's own variables, so
        // anything that is not a plain uppercase identifier would be a name a
        // handler could not read back.
        for key in [STORE_ENDPOINT_ENV, QUEUE_ENDPOINT_ENV] {
            assert!(
                key.starts_with("DEIS_")
                    && key
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit()),
                "{key} is not a usable environment variable name"
            );
        }
    }

    #[test]
    fn app_id_header_is_lowercase() {
        // gRPC metadata keys must be lowercase; a capitalised key is rejected
        // by tonic at send time rather than ignored.
        assert_eq!(APP_ID_HEADER, APP_ID_HEADER.to_ascii_lowercase());
    }
}
