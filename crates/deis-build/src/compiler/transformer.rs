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

    pub fn filename_to_route(filename: &str, function_root: &str) -> String {
        // Remove function root prefix
        let relative = filename
            .trim_start_matches(function_root)
            .trim_start_matches('/');

        // Remove file extension
        let without_ext = relative
            .trim_end_matches(".ts")
            .trim_end_matches(".js")
            .trim_end_matches(".tsx")
            .trim_end_matches(".jsx");

        // Handle index files
        let route = if without_ext == "index" || without_ext.is_empty() {
            "/"
        } else if without_ext.ends_with("/index") {
            without_ext.trim_end_matches("/index")
        } else {
            without_ext
        };

        // Convert dynamic parameters: [id] -> :id
        let route = route.replace("[", ":").replace("]", "");

        // Ensure leading slash
        if route.starts_with('/') {
            route.to_string()
        } else {
            format!("/{route}")
        }
    }
}
