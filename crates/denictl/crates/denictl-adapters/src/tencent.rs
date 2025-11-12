use denictl_core::ProjectConfig;
use std::fs;
use std::path::Path;

pub fn generate(output_dir: &Path, config: &ProjectConfig) -> anyhow::Result<()> {
    // Generate edgeone.toml
    let edgeone_toml = format!(
        r#"name = "{}"
language = "typescript"
runtime = "nodejs"
entry_point = "functions/index.ts"

[environments.staging]
url = "staging.{}.edgeone.ai"

[environments.production]
url = "{}.edgeone.ai"
"#,
        config.name, config.name, config.name
    );

    fs::write(output_dir.join("edgeone.toml"), edgeone_toml)?;

    // Create placeholder adapters directory
    let adapters_dir = output_dir.join("adapters");
    fs::create_dir_all(&adapters_dir)?;

    let readme = r#"# Tencent EdgeOne Adapters

Tencent EdgeOne adapter implementations would go here.
"#;

    fs::write(adapters_dir.join("README.md"), readme)?;

    Ok(())
}
