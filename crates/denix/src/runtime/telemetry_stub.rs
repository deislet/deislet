use deno_core::extension;

extension!(deno_telemetry, esm = ["telemetry.ts", "util.ts"]);

pub fn init_telemetry_extension() -> deno_core::Extension {
    deno_telemetry::init()
}
