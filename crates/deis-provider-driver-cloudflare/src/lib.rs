//! Cloudflare provider driver implementing `edge-canon-provider-driver-v1`.
//!
//! This crate is both a library (for testing) and a binary (the actual
//! driver process). The binary reads a JCS-encoded
//! [`deis_provider_driver::DriverProcessRequest`] from stdin, drives wrangler
//! 4.129.0 per the lock file, and writes a
//! [`deis_provider_driver::DriverProcessOutcome`] to stdout.
//!
//! ## Wrangler version
//!
//! This driver pins wrangler 4.129.0 as recorded in
//! `provider-drivers/official-tool-lock.json`. The binary is downloaded from
//! the npm registry, verified against both `npmIntegrity` (SHA-512 SRI) and
//! `distributionSha256`, and executed from a content-addressed directory. The
//! system PATH is never consulted.
//!
//! ## Credential transport
//!
//! The `EDGE_CANON_PROVIDER_CREDENTIAL` environment variable carries a JSON
//! object `{"api_token": "...", "account_id": "..."}`. The driver sets
//! `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` for the wrangler
//! subprocess.

pub mod commands;
pub mod credentials;
pub mod protocol;
pub mod wrangler;

/// The lock-entry values this driver hard-codes, so a build that drifts from
/// the lock file fails at compile time with a clear message.
pub const EXPECTED_WRANGLER_VERSION: &str = "4.129.0";
pub const EXPECTED_WRANGLER_NPM_INTEGRITY: &str =
    "sha512-PGPvs9UPoFrwxT0VogpESSZGvZIctAuTK3wGsLLPHtHsSgS85kNdvtpa2d14UzG8gwLWD64XUGFPGG9tOXG9VQ==";
pub const EXPECTED_WRANGLER_DISTRIBUTION_SHA256: &str =
    "08912bc89315a123bd22c8c29813bde70838e0b0e7eaee4369b8e0ba02dac81c";

/// The path inside the npm tarball where the wrangler binary lives.
///
/// npm packages for platform-specific binaries place them at well-known
/// paths. Wrangler ships a single binary at this location.
pub const WRANGLER_TARBALL_BINARY_PATH: &str = "package/wrangler";

/// The registry URL recorded in the lock file.
pub const WRANGLER_REGISTRY: &str = "https://registry.npmjs.org";
/// The tarball path within the registry.
pub const WRANGLER_TARBALL: &str =
    "https://registry.npmjs.org/wrangler/-/wrangler-4.129.0.tgz";