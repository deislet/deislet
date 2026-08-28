use anyhow::{Result, bail};
use colored::Colorize;

/// Deployment is not wired to any platform API yet. Report the request and
/// fail with a non-zero exit code instead of pretending to succeed.
pub async fn execute(
    vendors: Vec<String>,
    env: String,
    tag: Option<String>,
    promote: bool,
) -> Result<()> {
    println!("\n{}", "Deploying Edge Function...".bold());
    println!("  environment: {}", env);
    println!("  vendors:     {}", vendors.join(", "));
    if let Some(tag) = &tag {
        println!("  tag:         {}", tag);
    }
    if promote {
        println!("  promote:     yes");
    }
    println!();

    bail!(
        "deploy is not implemented: no platform API integration exists yet. \
         Run `build` to produce vendor artifacts and deploy them with each vendor's own tooling."
    )
}
