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

// ---- naming a namespace inside one tenant -----------------------------
//
// A tenant is one application in one environment, and that is the boundary
// the storage services enforce. Inside it an application may want more than
// one namespace - hot data apart from cold, two buckets, a second database -
// and that is what a *binding* names. These four items are the whole of the
// mechanism, and they live here because deis-bundle validates a name,
// deis-runtime stamps it on a call, and deis-store and deis-queue turn it
// into a column value, a file name and a directory. A disagreement about the
// separator would put one application's writes in another namespace of its
// own, silently.

/// Header naming which namespace inside the tenant a storage call wants.
///
/// Absent means the tenant's unnamed namespace - the one every application
/// had before bindings existed, and the one `context.services.kv` still
/// reaches. This header is deliberately *not* a credential and does not need
/// to be: [`STORAGE_TOKEN_HEADER`] authorises the tenant, and a binding is
/// always inside the tenant it is stamped beside, so the worst a forged value
/// can reach is another namespace of the caller's own data.
pub const BINDING_HEADER: &str = "x-deis-binding";

/// Separator between a tenant and a binding name inside a namespace id.
///
/// Deliberately a character [`is_valid_tenant_part`] and
/// [`is_valid_binding_name`] both refuse, which is what makes
/// [`namespace_id`] injective: no pair of (tenant, binding) can spell another
/// pair's namespace, and no namespace with a binding can spell a bare tenant.
/// It is also legal in a file name on every platform this runs on, because
/// deis-store turns a namespace id into a database file and a directory.
pub const BINDING_SEPARATOR: char = '@';

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
/// This function, together with [`is_valid_tenant_part`], is what carries the
/// path-safety argument for the *composed* form. A namespace id is at most
/// three of these parts joined by the two separators, and no part may hold a
/// `.`, a `/`, a `\` or a NUL, so `app.environment@BINDING`:
///
/// * is a single path component - the separators are not path separators, and
///   nothing inside a part can become one;
/// * can never spell `.` or `..`, since every part is non-empty and holds no
///   `.` of its own;
/// * can never be absolute, since no part may begin with `/`;
/// * is at most 194 bytes, so appending a suffix such as `.sqlite-journal`
///   still leaves it inside the 255-byte name limit.
///
/// A binding therefore widens what a tenant may address inside its own
/// directory and never where that directory is.
pub fn is_valid_binding_name(name: &str) -> bool {
    is_valid_tenant_part(name)
}

/// The namespace one storage call acts in: a tenant, and the binding inside
/// it when the call names one.
///
/// `None` returns the tenant unchanged, which is why an application deployed
/// before bindings existed keeps reading and writing exactly the rows, files
/// and objects it always had.
pub fn namespace_id(tenant: &str, binding: Option<&str>) -> String {
    match binding {
        Some(binding) => format!("{tenant}{BINDING_SEPARATOR}{binding}"),
        None => tenant.to_string(),
    }
}

/// Split a namespace id back into its tenant and its binding.
///
/// Returns `None` for anything that is not a legal tenant, optionally
/// followed by the separator and a legal binding name. Two separators, an
/// empty binding and a binding holding a `.` are all refused rather than
/// read loosely: this is what a storage service parses before it touches
/// anything on disk.
pub fn split_namespace(namespace: &str) -> Option<(&str, Option<&str>)> {
    match namespace.split_once(BINDING_SEPARATOR) {
        Some((tenant, binding)) => {
            if split_tenant(tenant).is_some() && is_valid_binding_name(binding) {
                Some((tenant, Some(binding)))
            } else {
                None
            }
        }
        None => split_tenant(namespace).map(|_| (namespace, None)),
    }
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

// ---- the node-facing control plane ------------------------------------
//
// These four names are the contract between deis-control (which serves them)
// and deis-runtime (which calls them). They live here, and not in either
// crate, because nothing checks that two crates spell an HTTP path the same
// way: a node that asked for `/register` while the control plane served
// `/registration` compiled perfectly, and the only symptom was a fleet that
// never learned about its own nodes.

/// Route template for a node's registration, in the `:id` form an axum
/// `Router` takes.
///
/// The client half is [`node_registration_path`]. They are separate because a
/// server registers one route for every node while a client addresses one.
pub const NODE_REGISTRATION_ROUTE: &str = "/api/nodes/:id/registration";

/// Route template for a node's assignment list. See
/// [`NODE_REGISTRATION_ROUTE`].
pub const NODE_ASSIGNMENTS_ROUTE: &str = "/api/nodes/:id/assignments";

/// Path the control plane serves its routing table on, and deis-proxy polls.
///
/// It carries no path parameter, so server and client spell the same string.
/// Here for the same reason as [`NODE_ASSIGNMENTS_ROUTE`]: the two crates do
/// not depend on each other, and a disagreement would show up as an edge that
/// keeps an empty routing table and sends every request to whatever the local
/// application list happens to match.
pub const ROUTING_TABLE_ROUTE: &str = "/api/routing-table";

/// Path a node registers itself at, and refreshes that registration at.
///
/// `PUT` is the method: a registration states what a node is rather than
/// asking to create something, so a node that lost contact and came back
/// says it again. `DELETE` on the same path is a node leaving on purpose.
///
/// `node_id` must already be percent-encoded - an id containing `/` would
/// otherwise address a different path than the one it registered under.
pub fn node_registration_path(encoded_node_id: &str) -> String {
    format!("/api/nodes/{encoded_node_id}/registration")
}

/// Path a node fetches its assignment list from. Same encoding rule as
/// [`node_registration_path`].
pub fn node_assignments_path(encoded_node_id: &str) -> String {
    format!("/api/nodes/{encoded_node_id}/assignments")
}

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

// ---- proving a storage caller may be the tenant it claims --------------
//
// The tenant header above says *which* namespace a call wants. On its own it
// is a claim and nothing more: a caller that can reach deis-store or
// deis-queue can name any tenant and be served. These three names are the
// proof that goes with the claim, and they live here for the usual reason -
// deis-control mints the token, deis-runtime carries it, and the two storage
// services check it, so a disagreement about the header name or the digest
// would show up as every storage call being refused.

/// Header carrying the capability token that proves a caller may act as the
/// tenant it names in [`TENANT_HEADER`].
///
/// Its own header rather than a field in an existing one: the tenant header
/// is a routing key that deis-runtime fills in from the request it is
/// serving, while this is a credential the node was handed. Keeping them
/// apart means a log line, a trace or a proxy rule that prints the tenant
/// does not thereby print the credential.
pub const STORAGE_TOKEN_HEADER: &str = "x-deis-storage-token";

/// Deployment-env key carrying the storage capability token for the tenant a
/// deployment belongs to.
///
/// Travels the same way as [`STORE_ENDPOINT_ENV`]: the control plane puts it
/// in the `env` map of an `ApplyDeployment`, and deis-runtime lifts it back
/// out before the application sees its own variables - an application that
/// could read this value could mint calls as its own tenant directly against
/// the storage port, bypassing every check the node makes on its behalf.
pub const STORAGE_TOKEN_ENV: &str = "DEIS_STORAGE_TOKEN";

/// The capability token for one tenant, derived from the shared secret.
///
/// `base64(HMAC-SHA256(secret, tenant))`, standard alphabet with padding.
/// HMAC rather than a plain hash so the secret cannot be recovered by
/// extending a digest, and per tenant rather than one bearer token for the
/// fleet so that a token which leaked out of one application is useless for
/// another: it only ever matches the one tenant it was derived for.
///
/// The token is not a session and does not expire. Rotating the secret
/// invalidates every token at once, which is the intended lever: change it on
/// the control plane and on both storage services, and every node picks up
/// new tokens with its next deployment.
///
/// `secret` is used as the HMAC key, so any length works, including the empty
/// string - which is why an unconfigured secret is a *refusal* on the
/// verifying side rather than an empty key that happens to compute.
pub fn storage_token(secret: &str, tenant: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = <Hmac<sha2::Sha256>>::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 accepts a key of any length");
    mac.update(tenant.as_bytes());
    base64_standard(&mac.finalize().into_bytes())
}

/// Whether a presented token is the expected one, in time that does not
/// depend on how many leading bytes matched.
///
/// The verifying side computes the expected token with [`storage_token`] for
/// the tenant *the request itself claims*, then compares here. Doing it in
/// that order is what makes a leaked token useless elsewhere: there is no
/// lookup from token to tenant, so a token only ever authorises the tenant it
/// was derived for.
///
/// The timing difference is small over a LAN and costs nothing to remove;
/// this is the whole authorisation of the storage channel.
pub fn storage_token_matches(expected: &str, presented: &str) -> bool {
    constant_time_eq(expected, presented)
}

// ---- proving a caller may drive a node's management plane --------------
//
// The three names below are the other direction of the node credential. The
// control plane serves its node-facing HTTP endpoints to a caller that
// presents one of its `node_preshared_keys`; deis-runtime serves
// `deis.management.RuntimeManagement` and `deis.trigger.Trigger` to a caller
// that presents the same value back. They live here because the node checks
// the string the control plane builds, and a disagreement about the metadata
// key or the `Bearer ` prefix would show up as a fleet whose every deployment
// push and every cron fire is refused - with the node logging "no credential"
// and the control plane logging "unauthenticated", neither naming the typo.

/// Metadata key carrying the node credential on a control-plane-to-node gRPC
/// call.
///
/// The same header the node already sends the other way on the assignment
/// fetch and the bundle download, so one credential is spelled one way in
/// both directions. It must be lowercase: tonic refuses a capitalised
/// metadata key at send time, which would turn a correct credential into a
/// call that never leaves the control plane.
pub const NODE_AUTH_METADATA: &str = "authorization";

/// The metadata value that presents a node key.
///
/// `Bearer <key>` rather than the bare key, matching what deis-runtime
/// already puts on `Authorization` when it fetches its assignments and what
/// deis-proxy puts on its catalog poll.
pub fn node_auth_value(secret: &str) -> String {
    format!("Bearer {secret}")
}

/// The key inside a presented [`NODE_AUTH_METADATA`] value.
///
/// The bare key is accepted as well as `Bearer <key>`, the same latitude
/// deis-control's HTTP node endpoints already give: an operator testing a
/// node by hand should not be refused over a prefix, and accepting both costs
/// nothing because the value is still compared against the configured key in
/// full.
pub fn node_auth_secret(value: &str) -> &str {
    value.strip_prefix("Bearer ").unwrap_or(value)
}

/// Whether a presented credential is the one the receiver accepts, in time
/// that does not depend on how many leading bytes matched.
///
/// The empty credential is never a match, in either position. An unconfigured
/// credential must be a refusal on the checking side, not an empty string
/// that happens to compare equal to another empty string - that is the
/// difference between a closed door and an open one.
///
/// # Why this is not named after the node key
///
/// Three unrelated credentials are checked here, and only one of them is the
/// fleet's node key: the storage token below, and deis-runtime's per-session
/// inspector token, which is minted by the node, lives in memory, expires,
/// and has nothing to do with any fleet-wide value. Naming the comparator
/// after one of its callers made the other two look like the same credential,
/// and it meant that retiring the node key would have taken an unrelated
/// check down with it.
pub fn secret_matches(expected: &str, presented: &str) -> bool {
    !expected.is_empty() && constant_time_eq(expected, presented)
}

/// Whether a presented node key is one the receiver accepts.
///
/// [`secret_matches`] under the fleet key's own name, kept for the callers
/// that really are checking that credential so the grep for it still finds
/// them all. It carries no rule of its own.
pub fn node_secret_matches(expected: &str, presented: &str) -> bool {
    secret_matches(expected, presented)
}

/// Compare two credentials in time that does not depend on how many leading
/// bytes matched.
///
/// Shared by the storage token and the node key because both are the whole
/// authorisation of their channel. The length is allowed to leak - it is a
/// property of the format, not of the secret - but nothing else is.
fn constant_time_eq(expected: &str, presented: &str) -> bool {
    let (expected, presented) = (expected.as_bytes(), presented.as_bytes());
    if expected.len() != presented.len() {
        return false;
    }
    expected
        .iter()
        .zip(presented)
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Standard base64 with padding (RFC 4648 section 4).
///
/// Written out here rather than pulled in as a dependency because it encodes
/// exactly one thing - a 32 byte digest - and because every component in the
/// platform has to produce the identical string for the same bytes. The
/// alphabet is chosen so the result is valid in a gRPC metadata value, which
/// admits only printable ASCII.
fn base64_standard(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = ((chunk[0] as u32) << 16)
            | ((*chunk.get(1).unwrap_or(&0) as u32) << 8)
            | (*chunk.get(2).unwrap_or(&0) as u32);
        out.push(ALPHABET[(group >> 18) as usize & 63] as char);
        out.push(ALPHABET[(group >> 12) as usize & 63] as char);
        // The tail is padded rather than truncated: a decoder that is handed
        // an unpadded string is entitled to reject it, and these values are
        // compared as strings, so one side dropping the padding would refuse
        // every call.
        out.push(match chunk.len() {
            1 => '=',
            _ => ALPHABET[(group >> 6) as usize & 63] as char,
        });
        out.push(match chunk.len() {
            3 => ALPHABET[group as usize & 63] as char,
            _ => '=',
        });
    }
    out
}

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
        for key in [STORE_ENDPOINT_ENV, QUEUE_ENDPOINT_ENV, STORAGE_TOKEN_ENV] {
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
    fn a_node_key_round_trips_through_the_metadata_value() {
        // The control plane builds the value and the node reads it back. A
        // mismatch here is a fleet where every push and every cron fire is
        // refused, so the pair is pinned rather than left to two call sites
        // that happen to agree today.
        let value = node_auth_value("node-psk");
        assert_eq!(value, "Bearer node-psk");
        assert_eq!(node_auth_secret(&value), "node-psk");
        assert_eq!(node_auth_secret("node-psk"), "node-psk");
        assert_eq!(NODE_AUTH_METADATA, NODE_AUTH_METADATA.to_ascii_lowercase());
    }

    #[test]
    fn an_empty_node_key_matches_nothing_at_all() {
        // The case that decides whether an unconfigured node is closed or
        // open: a receiver holding no key must refuse a caller presenting no
        // key, rather than finding two empty strings equal and letting it in.
        assert!(!node_secret_matches("", ""));
        assert!(!node_secret_matches("", "anything"));
        assert!(!node_secret_matches("node-psk", ""));
        assert!(node_secret_matches("node-psk", "node-psk"));
        assert!(!node_secret_matches("node-psk", "node-psl"));
        assert!(!node_secret_matches("node-psk", "node-psk-longer"));
    }

    /// The same rule, asserted at the name the credentials that are not the
    /// fleet key are checked under. Both spellings are pinned so that
    /// retiring the node key's wrapper cannot quietly take the rule with it -
    /// deis-runtime's per-session inspector token is checked here, and it
    /// outlives the fleet key by design.
    #[test]
    fn an_empty_credential_matches_nothing_at_all() {
        assert!(!secret_matches("", ""));
        assert!(!secret_matches("", "anything"));
        assert!(!secret_matches("session-token", ""));
        assert!(secret_matches("session-token", "session-token"));
        assert!(!secret_matches("session-token", "session-tokem"));
        assert!(!secret_matches("session-token", "session-token-longer"));
    }

    #[test]
    fn headers_are_lowercase() {
        // gRPC metadata keys must be lowercase; a capitalised key is rejected
        // by tonic at send time rather than ignored.
        for h in [
            APP_ID_HEADER,
            ENVIRONMENT_HEADER,
            TENANT_HEADER,
            STORAGE_TOKEN_HEADER,
        ] {
            assert_eq!(h, h.to_ascii_lowercase(), "{h} must be lowercase");
        }
    }

    #[test]
    fn a_binding_names_a_namespace_inside_one_tenant() {
        let tenant = tenant_id("shop", "production");
        assert_eq!(namespace_id(&tenant, None), "shop.production");
        assert_eq!(namespace_id(&tenant, Some("HOT")), "shop.production@HOT");
        assert_eq!(
            split_namespace("shop.production@HOT"),
            Some(("shop.production", Some("HOT")))
        );
        assert_eq!(
            split_namespace("shop.production"),
            Some(("shop.production", None))
        );
    }

    #[test]
    fn no_two_binding_names_can_spell_the_same_namespace() {
        // The property the whole mechanism rests on. The separator is refused
        // by both halves of a namespace id, so the only string a pair can
        // produce is its own - there is no (tenant, binding) that spells
        // another pair's namespace, and none that spells a bare tenant.
        assert!(!is_valid_tenant_part("a@b"));
        assert!(!is_valid_binding_name("a@b"));
        assert!(!is_valid_binding_name("a.b"));
        assert!(!is_valid_binding_name(""));

        let mut seen = std::collections::HashSet::new();
        for app in ["shop", "shop-eu", "shop_eu"] {
            for env in ["production", "preview-1"] {
                for binding in [None, Some("HOT"), Some("COLD"), Some("HOT-2")] {
                    let id = namespace_id(&tenant_id(app, env), binding);
                    assert!(seen.insert(id.clone()), "{id} was produced twice");
                    assert_eq!(
                        split_namespace(&id).map(|(t, b)| (t.to_string(), b.map(str::to_string))),
                        Some((tenant_id(app, env), binding.map(str::to_string))),
                        "{id} did not round trip"
                    );
                }
            }
        }
    }

    #[test]
    fn a_namespace_id_cannot_be_talked_into_another_tenant() {
        // The attempt an application would actually make: put a separator, a
        // dot or a traversal inside the name it declares, and reach a
        // namespace belonging to something else. Every one of them is refused
        // before it becomes a file name.
        for hostile in [
            "shop.production@..",
            "shop.production@a.b",
            "shop.production@a@b",
            "shop.production@",
            "shop.production@a/b",
            "shop@HOT",
            "@HOT",
            "shop.production.preview@HOT",
        ] {
            assert!(
                split_namespace(hostile).is_none(),
                "{hostile} must not parse as a namespace"
            );
        }
    }

    #[test]
    fn base64_agrees_with_rfc_4648() {
        // The token is compared as a string across three binaries, so the
        // encoder is pinned to the published vectors rather than to itself.
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_standard(input.as_bytes()), want, "for {input:?}");
        }
    }

    #[test]
    fn a_storage_token_is_the_published_hmac_of_the_tenant() {
        // RFC 4231 test case 2, so the digest is checked against something
        // outside this crate: a change of algorithm here would otherwise be
        // invisible until the control plane and the store disagreed.
        let expected = base64_standard(
            &hex::decode("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
                .unwrap(),
        );
        assert_eq!(
            storage_token("Jefe", "what do ya want for nothing?"),
            expected
        );
    }

    #[test]
    fn a_token_only_ever_matches_the_tenant_it_was_minted_for() {
        // The whole point of deriving per tenant: a token that leaked out of
        // one application authorises nothing in another, and neither does a
        // token minted under a different secret.
        let secret = "shared-secret";
        let mine = storage_token(secret, &tenant_id("shop", "production"));
        for other in [
            tenant_id("shop", "preview-7"),
            tenant_id("other-app", "production"),
        ] {
            assert!(
                !storage_token_matches(&storage_token(secret, &other), &mine),
                "a token for shop.production must not be accepted for {other}"
            );
        }
        assert!(!storage_token_matches(
            &storage_token("another-secret", &tenant_id("shop", "production")),
            &mine
        ));
        assert!(storage_token_matches(
            &storage_token(secret, &tenant_id("shop", "production")),
            &mine
        ));
    }

    #[test]
    fn a_token_is_printable_ascii_so_it_survives_grpc_metadata() {
        // tonic refuses a metadata value that is not visible ASCII at send
        // time, which would turn a correct token into a call that never
        // leaves the node.
        let token = storage_token("s", &tenant_id("app", "production"));
        assert!(
            token.bytes().all(|b| b.is_ascii_graphic() || b == b' '),
            "{token} cannot be sent as metadata"
        );
        // 32 digest bytes, so four characters per three bytes plus padding.
        assert_eq!(token.len(), 44);
    }

    #[test]
    fn comparing_tokens_is_length_safe() {
        // A truncated or extended presentation must be refused rather than
        // indexing past the end of either side.
        let token = storage_token("s", "app.production");
        assert!(!storage_token_matches(&token, ""));
        assert!(!storage_token_matches(&token, &token[..10]));
        assert!(!storage_token_matches(&token, &format!("{token}x")));
        assert!(storage_token_matches("", ""));
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
