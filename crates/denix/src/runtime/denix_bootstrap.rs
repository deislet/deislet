use deno_core::extension;

extension!(
    denix_bootstrap,
    esm_entry_point = "ext:denix_bootstrap/src/runtime/js/bootstrap.js",
    esm = ["src/runtime/js/bootstrap.js"]
);

pub fn init_denix_bootstrap_extension() -> deno_core::Extension {
    denix_bootstrap::init()
}
