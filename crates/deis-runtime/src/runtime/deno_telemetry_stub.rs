//! Stub for Deno's `deno_telemetry` ESM module.
//!
//! Deno's own extensions import `ext:deno_telemetry/telemetry.ts` and
//! `ext:deno_telemetry/util.ts` unconditionally - `ext/fetch/26_fetch.js` and
//! `ext/http/00_serve.ts` among them - so an isolate built without those two
//! modules fails to instantiate. This crate does not want Deno's OpenTelemetry
//! machinery, so it registers two do-nothing modules under exactly those
//! specifiers and lets the imports resolve.
//!
//! It has nothing to do with deis-telemetry, the platform's log and metric
//! component. The half of this crate that talks to deis-telemetry is
//! `crate::logging::shipper`; the name here is Deno's and cannot be changed
//! without breaking the specifiers above.
//!
//! The two `.ts` files live in `deno_telemetry_stub/` beside this file. Their
//! specifiers are written out in full so that moving them never changes what
//! Deno's modules import: `dir` prefixes the specifier as well as the path.

use deno_core::extension;

extension!(
    deno_telemetry,
    lazy_loaded_js = [
        dir "src/runtime/deno_telemetry_stub",
        "ext:deno_telemetry/telemetry.ts" = "telemetry.ts",
        "ext:deno_telemetry/util.ts" = "util.ts",
    ]
);

pub fn init_deno_telemetry_extension() -> deno_core::Extension {
    deno_telemetry::init()
}
