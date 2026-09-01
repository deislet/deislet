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
    ///
    /// The root comes off *once*. `trim_start_matches`, which this used, takes
    /// a prefix off as many times as it appears, so a handler at
    /// `functions/functions/index.ts` came back as `index.ts` and answered `/`
    /// rather than `/functions`. It is the same mistake that made a file under
    /// a directory called `src` into the whole application's queue consumer;
    /// see `deis_build_core::routes::staged_handler_path`.
    pub fn filename_to_route(filename: &str, function_root: &str) -> String {
        let relative = filename.strip_prefix(function_root).unwrap_or(filename);
        let relative = relative.strip_prefix('/').unwrap_or(relative);

        deis_build_core::routes::route_pattern(relative)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_function_root_comes_off_once() {
        assert_eq!(Transformer::filename_to_route("index.ts", ""), "/");
        assert_eq!(
            Transformer::filename_to_route("functions/api/hello.ts", "functions"),
            "/api/hello"
        );
        // Repeated stripping turned this into `/`, so a real page answered the
        // root and whatever belonged at the root was shadowed.
        assert_eq!(
            Transformer::filename_to_route("functions/functions/index.ts", "functions"),
            "/functions"
        );
    }
}
