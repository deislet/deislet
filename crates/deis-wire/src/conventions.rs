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

/// Immutable deployment generation selected by the trusted routing layer.
///
/// Application and environment say *which logical target* a request belongs
/// to; this value says which immutable deployment of that target must answer.
/// A proxy stamps it from one atomically accepted routing-table snapshot and a
/// runtime must never silently substitute whatever version happens to be
/// current on that node.  Keeping the name here makes the edge/runtime hop one
/// shared protocol rather than two matching string literals.
pub const DEPLOYMENT_GENERATION_HEADER: &str = "x-deis-deployment-generation";
/// Durable deployment plan associated with a generation-fenced queue
/// consumer. Kept in metadata rather than message bodies so existing queue
/// producers remain wire-compatible.
pub const DEPLOYMENT_ID_HEADER: &str = "x-deis-deployment-id";
/// Identity claimed by a proxy on the authenticated node channel.
pub const PROXY_ID_HEADER: &str = "x-deis-proxy-id";
/// Last routing table a proxy has fully validated and published locally.
pub const OBSERVED_ROUTING_TABLE_HEADER: &str = "x-deis-observed-routing-table";

/// The environment a request belongs to when nothing says otherwise.
pub const DEFAULT_ENVIRONMENT: &str = "production";

/// Separator between the application and the environment inside a tenant
/// label.
///
/// Neither an app id nor an environment name may contain it, so a label
/// splits unambiguously and no pair of legal names can collide with another
/// pair.
pub const TENANT_SEPARATOR: char = '.';

/// The `<app>.<environment>` label of one application environment.
///
/// A label, not a storage authority. deis-telemetry attributes records to
/// it and log lines print it, because an operator reads names. deis-store
/// and deis-queue never see it: they key everything by the canonical tenant
/// (`v1/<application id>/<environment id>`) the caller's capability names.
///
/// Callers must have validated both halves against [`is_valid_tenant_part`]
/// first; this function only joins them.
pub fn tenant_id(app_id: &str, environment: &str) -> String {
    format!("{app_id}{TENANT_SEPARATOR}{environment}")
}

/// Split a tenant label back into its application and environment.
///
/// Returns `None` for anything that is not exactly two valid parts: a value
/// with no separator, with two, or with a part outside the character set.
pub fn split_tenant(tenant: &str) -> Option<(&str, &str)> {
    let (app, env) = tenant.split_once(TENANT_SEPARATOR)?;
    if is_valid_tenant_part(app) && is_valid_tenant_part(env) {
        Some((app, env))
    } else {
        None
    }
}

/// Whether one half of a tenant label is legal.
///
/// The rule is deliberately narrow - no `.`, `/` or `\` - so that a label
/// is always safe to print, to use as a metric label, and to split.
pub fn is_valid_tenant_part(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 64
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

// ---- naming a namespace inside one tenant -----------------------------
//
// A tenant is one application in one environment, and that is the boundary
// the storage services enforce. Inside it an application may want more than
// one namespace - hot data apart from cold, two buckets, a second database -
// and that is what a *binding* names. The header and the name rule live here
// because deis-bundle validates a name, deis-runtime stamps it on a call, and
// deis-store turns it into part of a column value, a file name and a
// directory.

/// Header naming which namespace inside the tenant a storage call wants.
///
/// Absent means the tenant's unnamed namespace, the one
/// `context.services.kv` reaches. This header is deliberately *not* a
/// credential and does not need to be: the tenant is the one the caller's
/// capability names, never something the request says, and a binding only
/// selects a namespace inside that tenant, so the worst a forged value can
/// reach is another namespace of the caller's own data.
pub const BINDING_HEADER: &str = "x-deis-binding";

/// The one name on `context.env` the platform owns.
///
/// It holds the binding a version's own static files are reached through, and
/// the platform installs it after everything the application declared has been
/// copied, so a value under that name could never be read. Two components have
/// to agree on the spelling and neither can see the other's copy: deis-bundle
/// refuses a *logical resource* with this name, and deis-wire's environment
/// declarations refuse a *variable or secret* with it. Spelled once, here,
/// because two spellings that drifted apart would leave one of the two
/// refusals silently unenforced.
///
/// Bare, not [`RESERVED_PLATFORM_PREFIX`]-prefixed, because it is the name an
/// author writes; the spelling is Cloudflare's, so a Pages project's code
/// reads the same here.
pub const RESERVED_PLATFORM_BINDING_NAME: &str = "ASSETS";

/// Prefix reserved for the platform's own wiring on `context.env`.
///
/// Every key the node carries a deployment's plumbing in begins with it - the
/// storage endpoints, the storage token, the binding table, the queue
/// consumers - and each of those is lifted back out before the map becomes
/// `context.env`. A name an application chose that collided with one of them
/// would either be dropped without the author ever learning why, or, worse,
/// be read as the plumbing it is spelled like: an application naming the store
/// endpoint would choose where its own data goes.
pub const RESERVED_PLATFORM_PREFIX: &str = "DEIS_";

/// Whether a string may name a binding.
///
/// The same character set as [`is_valid_tenant_part`], for the same reason:
/// the value becomes part of a file name and a directory name in deis-store,
/// so it is checked hard rather than sanitised. Case is preserved - authors
/// write `HOT`, the way they would on Cloudflare - and uniqueness is enforced
/// case-insensitively where a manifest is validated, because two names that
/// differ only in case would be one directory on a case-insensitive
/// filesystem.
///
/// It carries the path-safety argument for the composed namespace key
/// (`ct1_<application id>_<environment id>@BINDING`, see
/// [`crate::DeisletCanonicalNamespaceKey`]): no part may hold a `.`, a `/`, a
/// `\` or a NUL, so the key is always a single path component, can never
/// spell `.` or `..`, and stays short enough for a `.sqlite-journal` suffix.
/// A binding therefore widens what a tenant may address inside its own
/// directory and never where that directory is.
pub fn is_valid_binding_name(name: &str) -> bool {
    is_valid_tenant_part(name)
}

/// The request methods no application on this platform can be built to answer.
///
/// The Fetch standard's forbidden-method list, exactly. TRACE and TRACK are
/// also the cross-site tracing pair -- a server that echoes a request echoes
/// its cookies -- and CONNECT asks a node to be a tunnel, which it is not. The
/// runtime builds a standard `Request` for a handler, and deno_fetch's
/// constructor throws `TypeError: Method is forbidden` on these three: a
/// request carrying one could never reach a handler that answered it, only a
/// 500 built from that throw.
///
/// It lives here, spelled once, because more than one door on this platform
/// has to refuse it before the throw: the inbound HTTP path in the dispatcher,
/// and the application-to-application service-call path. A second copy would
/// let the two drift, so a method refused at the edge could still be posted to
/// a callee. See [`method_is_forbidden`] for the comparison.
pub const FORBIDDEN_METHODS: [&str; 3] = ["CONNECT", "TRACE", "TRACK"];

/// Whether `method` is one [`FORBIDDEN_METHODS`] names.
///
/// Case-insensitive, because the standard compares methods that way and a
/// client sending `trace` means the same thing as one sending `TRACE`.
pub fn method_is_forbidden(method: &str) -> bool {
    FORBIDDEN_METHODS
        .iter()
        .any(|forbidden| method.eq_ignore_ascii_case(forbidden))
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

/// Response header on an assignment list saying whether the list is the whole
/// truth for that node.
///
/// `true` means the array is everything this node should run, so anything it
/// is running that is absent from the list must be stopped - that is how a
/// deleted application goes away on a node the delete never reached. Any other
/// value, **including the header being missing**, means the control plane
/// could not describe part of the answer, and the node may only add. Treating
/// a short list as complete would turn one undecryptable secret into a healthy
/// application being stopped.
pub const ASSIGNMENTS_COMPLETE_HEADER: &str = "x-deis-assignments-complete";

/// Response header counting the assignments the control plane could not
/// describe, and so left out. Non-zero always accompanies a `complete: false`.
pub const ASSIGNMENTS_WITHHELD_HEADER: &str = "x-deis-assignments-withheld";

/// The phrase the control plane puts in a registration refusal when the
/// node id is held by a channel it still hears from, and the phrase the
/// refused side looks for to decide the refusal is transient.
///
/// A second channel registering an id whose holder is live is one of two
/// things: two processes sharing one `associated_id` (a configuration
/// mistake that will never resolve on its own), or one process reconnecting,
/// after a network break or as the replacement in a zero-downtime upgrade,
/// before the control plane has noticed the old channel go. The refused side
/// cannot tell which from its own seat, but the second is the common case
/// and resolves within a heartbeat timeout, so it retries at the short
/// interval and logs at warn rather than error. One shared constant, because
/// a refusal spelled one way here and matched another way there would turn
/// every upgrade back into an error storm.
pub const NODE_ID_HELD_BY_LIVE_CHANNEL: &str = "is held by a live channel";

/// A single chunk plus its framing must never approach the ceiling, otherwise
/// streaming would start failing on the transport instead of being bounded by
/// our own logic. Checked at compile time so it cannot drift.
const _: () = assert!(STREAM_CHUNK_BYTES * 2 < MAX_GRPC_MESSAGE_BYTES);

/// One entry in a `FetchSecretValues` request: a deployment id and the set
/// of secret revision ids the calling node needs plaintext values for.
///
/// This is the wire type for the `entries_json` field of the gRPC request.
/// `revisions` must be non-empty and every one of them bound by the
/// deployment's snapshot; there is no "all secrets of this deployment" form.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SecretFetchEntry {
    pub deployment_id: String,
    pub revisions: Vec<String>,
}

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
        for h in [APP_ID_HEADER, ENVIRONMENT_HEADER, BINDING_HEADER] {
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
