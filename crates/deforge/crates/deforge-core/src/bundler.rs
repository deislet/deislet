use anyhow::Result;
use rolldown::{Bundler, BundlerOptions, InputItem, OutputFormat, Platform};
use rolldown_common::Output;

pub enum TargetPlatform {
    Cloudflare,
    Deno,
    EdgeOne,
}

pub struct DeforgeBundler {
    options: BundlerOptions,
}

impl DeforgeBundler {
    pub fn new(entry: &str, platform: TargetPlatform) -> Self {
        let (platform_opt, format) = match platform {
            TargetPlatform::Cloudflare | TargetPlatform::EdgeOne => {
                (Some(Platform::Browser), Some(OutputFormat::Esm))
            }
            TargetPlatform::Deno => {
                // Deno supports Node-style resolution but runs in a browser-like environment
                (Some(Platform::Node), Some(OutputFormat::Esm))
            }
        };

        let options = BundlerOptions {
            input: Some(vec![InputItem {
                name: Some("index".to_string()),
                import: entry.to_string(),
            }]),
            platform: platform_opt,
            format,
            ..Default::default()
        };

        Self { options }
    }

    pub async fn bundle(&self) -> Result<String> {
        let mut bundler = Bundler::new(self.options.clone())?;
        let result = bundler.write().await?;

        let mut code = String::new();

        for asset in result.assets {
            match asset {
                Output::Chunk(chunk) => {
                    if chunk.filename.as_str().ends_with(".js")
                        || chunk.filename.as_str().ends_with(".mjs")
                    {
                        code.push_str(&chunk.code);
                        code.push('\n');
                    }
                }
                Output::Asset(_) => {
                    // Ignore assets for code bundle
                }
            }
        }

        Ok(code)
    }
}
