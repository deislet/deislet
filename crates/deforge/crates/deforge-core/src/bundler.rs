use anyhow::Result;
use rolldown::{Bundler, BundlerOptions, InputItem, Platform, OutputFormat};
use rolldown_common::Output;
use std::path::Path;
use std::sync::Arc;

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
        let mut options = BundlerOptions::default();
        
        // Input config
        options.input = Some(vec![InputItem {
            name: Some("index".to_string()),
            import: entry.to_string(),
        }]);

        // Platform config
        match platform {
            TargetPlatform::Cloudflare | TargetPlatform::EdgeOne => {
                options.platform = Some(Platform::Browser);
                options.format = Some(OutputFormat::Esm);
            }
            TargetPlatform::Deno => {
                // Deno supports Node-style resolution but runs in a browser-like environment
                options.platform = Some(Platform::Node); 
                options.format = Some(OutputFormat::Esm);
            }
        }
        
        Self { options }
    }

    pub async fn bundle(&self) -> Result<String> {
        let mut bundler = Bundler::new(self.options.clone())?;
        let result = bundler.write().await?;
        
        let mut code = String::new();
        
        for asset in result.assets {
            match asset {
                Output::Chunk(chunk) => {
                    if chunk.filename.as_str().ends_with(".js") || chunk.filename.as_str().ends_with(".mjs") {
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
