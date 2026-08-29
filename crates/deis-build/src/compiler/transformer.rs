use deis_build_core::ProjectConfig;

pub struct Transformer {}

impl Default for Transformer {
    fn default() -> Self {
        Self::new()
    }
}

impl Transformer {
    pub fn new() -> Self {
        Self {}
    }

    pub fn transform(
        &self,
        code: &str,
        _filename: &str,
        _vendor: &str,
        _config: &ProjectConfig,
    ) -> anyhow::Result<String> {
        // Since we moved Context bridging and Adapter logic to the entry point (index.ts/main.ts),
        // and we use Rolldown for bundling, the Transformer no longer needs to wrap user code.
        // It should just pass through the code, or optionally perform SWC transforms (e.g. for strip types).
        // But since Rolldown handles TS, we can just pass through.
        Ok(code.to_string())
    }

    /// The route a handler file answers.
    ///
    /// Every target has to agree on this, or an app routes differently
    /// depending on where it was deployed, so the derivation itself lives in
    /// `deis_build_core::routes` and this is only the call site that strips
    /// the function root off the path first.
    pub fn filename_to_route(filename: &str, function_root: &str) -> String {
        let relative = filename
            .trim_start_matches(function_root)
            .trim_start_matches('/');

        deis_build_core::routes::route_pattern(relative)
    }
}
