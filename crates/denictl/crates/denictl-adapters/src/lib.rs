mod cloudflare;
mod deno;
mod tencent;

use denictl_core::ProjectConfig;
use std::fs;
use std::path::Path;

pub fn generate_vendor_files(
    output_dir: &Path,
    vendor: &str,
    config: &ProjectConfig,
) -> anyhow::Result<()> {
    match vendor {
        "cloudflare" => cloudflare::generate(output_dir, config)?,
        "deno" => deno::generate(output_dir, config)?,
        "tencent" => tencent::generate(output_dir, config)?,
        _ => return Err(anyhow::anyhow!("Unknown vendor: {}", vendor)),
    }

    Ok(())
}
