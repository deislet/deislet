//! Stub for Deno's `deno_telemetry` ESM module.
//!
//! This exists only so app code that imports Deno's OpenTelemetry module
//! resolves; it has nothing to do with deis-telemetry, the platform's log and
//! metric component. The half of this crate that talks to deis-telemetry is
//! `crate::logging::shipper`.

use deno_core::extension;

extension!(deno_telemetry, esm = ["telemetry.ts", "util.ts"]);

pub fn init_deno_telemetry_extension() -> deno_core::Extension {
    deno_telemetry::init()
}
