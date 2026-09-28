//! Cloudflare provider driver implementing `edge-canon-provider-driver-v1`.
//!
//! This crate is both a library (for testing) and a binary (the actual
//! driver process). The binary reads a JCS-encoded
//! [`deis_provider_driver::DriverProcessRequest`] from stdin, drives the
//! pinned wrangler, and writes a
//! [`deis_provider_driver::DriverProcessOutcome`] to stdout.
//!
//! ## Wrangler
//!
//! wrangler is a Node.js package. The driver installs the exact dependency
//! tree recorded in `toolchain/package-lock.json` (embedded at compile time)
//! into a content-addressed directory, verifying every tarball's `sha512`
//! integrity and the wrangler tarball's SHA-256 from
//! `provider-drivers/official-tool-lock.json`, and runs
//! `node node_modules/wrangler/bin/wrangler.js`. See [`toolchain`] and
//! `toolchain/README.md`.
//!
//! ## Credential transport
//!
//! The `EDGE_CANON_PROVIDER_CREDENTIAL` environment variable carries a JSON
//! object `{"api_token": "...", "account_id": "..."}`. The driver sets
//! `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` for the wrangler
//! subprocess only; neither ever reaches argv or a log line.

pub mod artifact;
pub mod commands;
pub mod credentials;
pub mod environment;
pub mod journal;
pub mod naming;
pub mod protocol;
pub mod toolchain;
pub mod wrangler;

/// The wrangler version this driver pins, equal to the `version` of the
/// Cloudflare entry in `provider-drivers/official-tool-lock.json`.
pub const EXPECTED_WRANGLER_VERSION: &str = "4.129.0";
/// The npm SRI of the wrangler tarball (`npmIntegrity` in the tool lock).
pub const EXPECTED_WRANGLER_NPM_INTEGRITY: &str = "sha512-PGPvs9UPoFrwxT0VogpESSZGvZIctAuTK3wGsLLPHtHsSgS85kNdvtpa2d14UzG8gwLWD64XUGFPGG9tOXG9VQ==";
/// SHA-256 of the whole wrangler tarball (`distributionSha256` in the tool
/// lock). It is a digest of the `.tgz` as downloaded, not of any file in it.
pub const EXPECTED_WRANGLER_DISTRIBUTION_SHA256: &str =
    "08912bc89315a123bd22c8c29813bde70838e0b0e7eaee4369b8e0ba02dac81c";
/// The registry every package of the toolchain lock must resolve from.
pub const WRANGLER_REGISTRY: &str = "https://registry.npmjs.org";
/// The wrangler tarball URL recorded in the tool lock.
pub const WRANGLER_TARBALL: &str = "https://registry.npmjs.org/wrangler/-/wrangler-4.129.0.tgz";
/// The lowest Node.js major version wrangler 4.129.0 supports
/// (`engines.node` is `>=22.0.0`).
pub const MINIMUM_NODE_MAJOR: u32 = 22;
