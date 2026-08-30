//! What a static site can say about itself without writing a function.
//!
//! Three files' worth of behaviour - `_redirects`, `_headers`, and the choice
//! between an SPA fallback and a custom 404 page - are defined here, in the
//! crate deis-build and deis-runtime already share. deis-build parses the
//! files at build time and puts the result in the manifest; deis-runtime
//! compiles the manifest's copy once when a version loads and applies it on
//! every request that misses the asset index. Neither side owns the format.
//!
//! A rule that is wrong fails the *build*, with the file, the line number and
//! the line's text. An author who typoed a redirect has to find out when they
//! build, not when a visitor does - which is why [`RuleError`] carries a
//! position at all, and why the runtime revalidates rather than trusting a
//! manifest: a bundle is author-controlled input, not something this process
//! wrote.

use serde::{Deserialize, Serialize};

/// The file a static site puts its redirect table in, at the root of the
/// assets directory.
pub const REDIRECTS_FILE: &str = "_redirects";

/// The file a static site puts its response headers in, at the root of the
/// assets directory.
pub const HEADERS_FILE: &str = "_headers";

/// One rule the platform refused to accept, and where it came from.
///
/// `line` is 1-based and 0 when the rule did not come from a file - the
/// runtime revalidating a manifest has rule indices, not lines, and says so
/// rather than inventing a line number that would send a reader to the wrong
/// place in their own file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleError {
    pub file: &'static str,
    pub line: usize,
    pub text: String,
    pub reason: String,
}

impl RuleError {
    pub fn at(
        file: &'static str,
        line: usize,
        text: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            file,
            line,
            text: text.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            write!(f, "{} rule {:?}: {}", self.file, self.text, self.reason)
        } else {
            write!(
                f,
                "{} line {}: {:?} - {}",
                self.file, self.line, self.text, self.reason
            )
        }
    }
}

impl std::error::Error for RuleError {}

/// What to answer for a path that named no asset, matched no redirect rule and
/// belongs to no function route.
///
/// The two behaviours are genuinely different and an author has to pick:
/// a single-page app wants its shell under the requested URL with a 200, so
/// the client-side router can read the path; a static site wants its own
/// error page with a 404, so crawlers and monitoring see the failure.
///
/// The names are the ones Cloudflare's `not_found_handling` uses, because an
/// author arriving from Pages already knows them and a second vocabulary for
/// the same three choices helps nobody.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotFound {
    /// Let the request through to the isolate, which answers 404 itself.
    #[default]
    #[serde(rename = "none")]
    None,
    /// Serve `404.html` with status 404.
    #[serde(rename = "404-page")]
    Page,
    /// Serve `index.html` with status 200.
    #[serde(rename = "single-page-app")]
    SinglePageApp,
}

impl NotFound {
    /// The asset each choice serves, or `None` for [`NotFound::None`].
    pub fn asset(&self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Page => Some(NOT_FOUND_PAGE),
            Self::SinglePageApp => Some(SPA_SHELL),
        }
    }

    /// The status each choice answers with.
    pub fn status(&self) -> u16 {
        match self {
            Self::None => 404,
            Self::Page => 404,
            Self::SinglePageApp => 200,
        }
    }

    /// Parse the spelling a project config uses.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.trim() {
            "none" | "" => Ok(Self::None),
            "404-page" | "404" => Ok(Self::Page),
            "single-page-app" | "single-page-application" | "spa" => Ok(Self::SinglePageApp),
            other => Err(format!(
                "{other:?} is not a not-found behaviour; write \"none\", \"404-page\" or \
                 \"single-page-app\""
            )),
        }
    }
}

/// The page [`NotFound::Page`] serves, relative to the assets root.
pub const NOT_FOUND_PAGE: &str = "404.html";

/// The shell [`NotFound::SinglePageApp`] serves, relative to the assets root.
pub const SPA_SHELL: &str = "index.html";

/// Everything the static half of a bundle declares, as it appears in the
/// manifest.
///
/// A bundle from a project with none of these files carries an empty one, and
/// `#[serde(default)]` on every field means a manifest written before this
/// existed still parses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteConfig {
    /// `_redirects`, in the order the author wrote them. Order is the
    /// semantics: first match wins.
    #[serde(default)]
    pub redirects: Vec<crate::redirects::RedirectRule>,
    /// `_headers`, in the order the author wrote them. All matching blocks
    /// apply and a later one wins a conflict.
    #[serde(default)]
    pub headers: Vec<crate::headers::HeaderRule>,
    #[serde(default)]
    pub not_found: NotFound,
}

impl SiteConfig {
    pub fn is_empty(&self) -> bool {
        self.redirects.is_empty() && self.headers.is_empty() && self.not_found == NotFound::None
    }

    /// Check every rule without keeping the compiled form.
    ///
    /// Called from [`crate::Manifest::validate`], so a manifest with a rule
    /// nothing could apply is rejected wherever a manifest is read - at
    /// upload by the control plane and at load by a node - instead of being
    /// discovered on the one request that would have matched it.
    pub fn validate(&self) -> Result<(), RuleError> {
        crate::redirects::RedirectTable::compile(&self.redirects)?;
        crate::headers::HeaderTable::compile(&self.headers)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rule_error_says_where_to_look() {
        let err = RuleError::at(REDIRECTS_FILE, 12, "/a /b 999", "status 999 is not allowed");
        assert_eq!(
            err.to_string(),
            "_redirects line 12: \"/a /b 999\" - status 999 is not allowed"
        );
    }

    #[test]
    fn a_rule_that_came_from_a_manifest_does_not_invent_a_line() {
        let err = RuleError::at(REDIRECTS_FILE, 0, "/a", "no target");
        assert_eq!(err.to_string(), "_redirects rule \"/a\": no target");
    }

    #[test]
    fn the_not_found_spellings_round_trip_through_json() {
        for (value, json) in [
            (NotFound::None, "\"none\""),
            (NotFound::Page, "\"404-page\""),
            (NotFound::SinglePageApp, "\"single-page-app\""),
        ] {
            assert_eq!(serde_json::to_string(&value).unwrap(), json);
            assert_eq!(serde_json::from_str::<NotFound>(json).unwrap(), value);
        }
    }

    #[test]
    fn an_unknown_not_found_choice_names_the_ones_that_exist() {
        let err = NotFound::parse("spa-ish").unwrap_err();
        assert!(err.contains("single-page-app"), "{err}");
    }
}
