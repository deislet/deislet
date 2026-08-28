use deno_core::extension;

extension!(
    bootstrap,
    esm_entry_point = "ext:bootstrap/src/runtime/js/bootstrap.js",
    esm = ["src/runtime/js/bootstrap.js"]
);

pub fn init_bootstrap_extension() -> deno_core::Extension {
    bootstrap::init()
}
