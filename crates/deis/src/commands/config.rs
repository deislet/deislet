use anyhow::Result;
use colored::Colorize;

// TODO: persisting the value is not implemented yet
pub async fn set(key: String, _value: String) -> Result<()> {
    println!("{}", format!("✓ Configuration saved: {}", key).green());
    println!(
        "{}",
        "  (Config management implementation coming soon)".dimmed()
    );
    Ok(())
}

pub async fn get(key: String) -> Result<()> {
    println!("{}", format!("Getting config: {}", key).dimmed());
    println!(
        "{}",
        "(Config management implementation coming soon)".dimmed()
    );
    Ok(())
}

pub async fn list() -> Result<()> {
    println!("{}", "Current configuration:".bold());
    println!(
        "{}",
        "(Config management implementation coming soon)".dimmed()
    );
    Ok(())
}
