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

/// Header naming the application an inbound request is for.
///
/// Stamped by deis-proxy on the HTTP data path and read by deis-runtime. It
/// says *which application*, not *which copy of it* - see
/// [`ENVIRONMENT_HEADER`].
pub const APP_ID_HEADER: &str = "x-deis-app-id";

/// Header naming which environment of that application to serve.
///
/// An application has a production environment and any number of previews,
/// each with its own version pointer, bindings and secrets. A node runs all of
/// them at once, so the pair (app, environment) - not the app alone - is what
/// identifies a running application on a node.
///
/// Absent means [`DEFAULT_ENVIRONMENT`], so an older caller that only stamps
/// the app id still reaches production rather than being refused.
pub const ENVIRONMENT_HEADER: &str = "x-deis-environment";

/// The environment a request belongs to when nothing says otherwise.
pub const DEFAULT_ENVIRONMENT: &str = "production";

/// Header carrying the tenant on calls into the storage services.
///
/// Deliberately a different header from [`APP_ID_HEADER`], because it carries
/// a different thing: a tenant is one application *in one environment*, so a
/// preview deployment cannot read or overwrite what production stored. The
/// value is built by [`tenant_id`] and the storage services treat it as one
/// opaque key - they still understand exactly one tenant dimension and know
/// nothing about environments.
pub const TENANT_HEADER: &str = "x-deis-tenant";

/// Separator between the application and the environment inside a tenant id.
///
/// Neither an app id nor an environment name may contain it, so a tenant id
/// splits unambiguously and no pair of legal names can collide with another
/// pair. It is also safe in a file name, which matters because deis-store
/// turns a tenant id into a directory and a database file name.
pub const TENANT_SEPARATOR: char = '.';

/// Build the tenant a storage call belongs to.
///
/// Callers must have validated both halves against [`is_valid_tenant_part`]
/// first; this function only joins them.
pub fn tenant_id(app_id: &str, environment: &str) -> String {
    format!("{app_id}{TENANT_SEPARATOR}{environment}")
}

/// Split a tenant id back into its application and environment.
///
/// Returns `None` for anything that is not exactly two valid parts, which is
/// what a storage service should reject: a value with no separator, with two,
/// or with a part that could name something outside its own directory.
pub fn split_tenant(tenant: &str) -> Option<(&str, &str)> {
    let (app, env) = tenant.split_once(TENANT_SEPARATOR)?;
    if is_valid_tenant_part(app) && is_valid_tenant_part(env) {
        Some((app, env))
    } else {
        None
    }
}

/// Whether one half of a tenant id is legal.
///
/// The rule is deliberately narrow. A tenant id becomes a path on disk in
/// deis-store, so this is a hard check rather than a sanitising pass: the
/// character set has no `.`, `/` or `\`, which means no combination of legal
/// parts can express a parent directory.
pub fn is_valid_tenant_part(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 64
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

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
    fn headers_are_lowercase() {
        // gRPC metadata keys must be lowercase; a capitalised key is rejected
        // by tonic at send time rather than ignored.
        for h in [APP_ID_HEADER, ENVIRONMENT_HEADER, TENANT_HEADER] {
            assert_eq!(h, h.to_ascii_lowercase(), "{h} must be lowercase");
        }
    }

    #[test]
    fn a_tenant_id_round_trips() {
        let t = tenant_id("demo", "production");
        assert_eq!(t, "demo.production");
        assert_eq!(split_tenant(&t), Some(("demo", "production")));
    }

    #[test]
    fn no_two_legal_pairs_can_produce_the_same_tenant_id() {
        // The separator is outside the legal character set, so a name cannot
        // smuggle one in and be mistaken for a different pair.
        assert!(!is_valid_tenant_part("demo.production"));
        assert_ne!(tenant_id("a", "b-c"), tenant_id("a-b", "c"));
    }

    #[test]
    fn a_tenant_id_cannot_name_a_parent_directory() {
        // deis-store turns a tenant id into a path, so this is the check that
        // stops one tenant reaching another tenant's files.
        for bad in ["..", ".", "a/b", "a\\b", "", "a.b"] {
            assert!(!is_valid_tenant_part(bad), "{bad:?} must be rejected");
        }
        assert_eq!(split_tenant("...."), None);
        assert_eq!(split_tenant("demo"), None, "a bare app id is not a tenant");
        assert_eq!(split_tenant("a.b.c"), None, "three parts is not a tenant");
    }

    #[test]
    fn a_tenant_part_is_length_limited() {
        assert!(is_valid_tenant_part(&"a".repeat(64)));
        assert!(!is_valid_tenant_part(&"a".repeat(65)));
    }
}
