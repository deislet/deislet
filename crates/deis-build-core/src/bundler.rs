use std::path::PathBuf;

use anyhow::Result;
use rolldown::{Bundler, BundlerOptions, InputItem, OutputFormat, Platform};
use rolldown_common::Output;

pub enum TargetPlatform {
    Cloudflare,
    Deno,
    EdgeOne,
    /// deis-runtime: a deno_core isolate with Web APIs and no Node built-ins,
    /// loading one ESM entry point.
    Deislet,
}

pub struct BuildBundler {
    options: BundlerOptions,
}

impl BuildBundler {
    pub fn new(entry: &str, platform: TargetPlatform) -> Self {
        let (platform_opt, format) = match platform {
            TargetPlatform::Cloudflare | TargetPlatform::EdgeOne | TargetPlatform::Deislet => {
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

    /// Resolve modules and print paths relative to `cwd` instead of the
    /// directory the build was launched from.
    ///
    /// Without this the bundler labels each chunk with a path relative to the
    /// process's working directory, so the same project built from two places
    /// produces two different files. A bundle whose bytes depend on where the
    /// developer happened to stand is not a bundle that can be content-
    /// addressed, which is what the platform does with it.
    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.options.cwd = Some(cwd);
        self
    }

    /// Bundle without touching the filesystem.
    ///
    /// [`Self::bundle`] writes the chunks to the output directory as a side
    /// effect of asking for them. A target that packs the result into an
    /// archive does not want those loose files: they would be left behind
    /// next to the archive, stale from the moment the next build ran.
    pub async fn generate(&self) -> Result<String> {
        let mut bundler = Bundler::new(self.options.clone())?;
        let result = bundler.generate().await?;
        Ok(Self::join_js_chunks(result.assets))
    }

    pub async fn bundle(&self) -> Result<String> {
        let mut bundler = Bundler::new(self.options.clone())?;
        let result = bundler.write().await?;
        Ok(Self::join_js_chunks(result.assets))
    }

    fn join_js_chunks(assets: Vec<Output>) -> String {
        let mut code = String::new();

        for asset in assets {
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

        code
    }
}
