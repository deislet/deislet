//! The rules a static site brought with it, compiled once per version.
//!
//! `_redirects`, `_headers` and the not-found behaviour are read at build time
//! and travel in the manifest (`deis_bundle::site`). This is the other half:
//! turning that text into tables a request can be run through, and running it.
//!
//! Compiled here rather than per request, for the same reason
//! [`deis_bundle::AssetIndex`] is built here rather than walked: this code is
//! on the path of every request that does not name a file, which on a static
//! site is every 404 and every client-side route. A request costs one decode
//! of its path, a linear scan of a bounded table of already-compiled patterns,
//! and nothing else.
//!
//! The order a request is decided in:
//!
//!   1. the asset index - a path that names a file is that file;
//!   2. `_redirects`, in the author's order, first match wins;
//!   3. the function routes the manifest declares - if one claims the path,
//!      the request belongs to the isolate and nothing here answers it;
//!   4. the not-found behaviour: the SPA shell with a 200, or the 404 page
//!      with a 404.
//!
//! Step 3 is what keeps an SPA fallback from swallowing an application's own
//! API. `_redirects` sits *above* it on purpose: a rule the author wrote by
//! hand is a decision they made, and a `/* /index.html 200` in that file is
//! meant to win. The fallback is not written by hand, so it defers.

use std::sync::atomic::{AtomicBool, Ordering};

use deis_bundle::{
    AssetIndex, CompiledPattern, HeaderField, HeaderTable, Manifest, NotFound, RedirectAction,
    RedirectTable,
};

use super::AssetError;

/// Who is asking, which decides whether the function routes get first refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// The HTTP entry, deciding whether to start an isolate at all. A path a
    /// function route claims belongs to the isolate, so the fallback stands
    /// aside for it.
    BeforeFunctions,
    /// `env.ASSETS.fetch()`, called from inside a function that has already
    /// decided it wants the asset layer's answer. The route table has had its
    /// turn, so the fallback applies whatever it says.
    FromFunction,
}

pub struct SiteRules {
    redirects: RedirectTable,
    headers: HeaderTable,
    not_found: NotFound,
    /// The asset the not-found behaviour serves, resolved once. `None` when
    /// the behaviour is `none`.
    fallback: Option<&'static str>,
    /// Function route patterns that would claim a GET, in manifest order.
    ///
    /// Only GET: the HTTP entry consults the asset layer for GET and HEAD
    /// alone, so a route restricted to POST cannot be the reason a fallback
    /// stood aside.
    function_routes: Vec<CompiledPattern>,
    /// A rewrite whose target is not in the bundle is worth an operator's
    /// attention once, not once per request - a crawler walking a catch-all
    /// would otherwise fill the log with the same line.
    rewrite_miss_reported: AtomicBool,
}

impl std::fmt::Debug for SiteRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiteRules")
            .field("redirects", &self.redirects.len())
            .field("headers", &self.headers.len())
            .field("not_found", &self.not_found)
            .finish()
    }
}

impl Default for SiteRules {
    fn default() -> Self {
        Self {
            redirects: RedirectTable::default(),
            headers: HeaderTable::default(),
            not_found: NotFound::None,
            fallback: None,
            function_routes: Vec::new(),
            rewrite_miss_reported: AtomicBool::new(false),
        }
    }
}

impl SiteRules {
    /// Compile the manifest's rules against the assets this version carries.
    ///
    /// The manifest was already checked by [`deis_bundle::Manifest::validate`],
    /// and this checks it again. That is not belt and braces for its own sake:
    /// a `Manifest` can be built in process as well as parsed, and the cost is
    /// one pass over a bounded table once per version load.
    pub fn compile(manifest: &Manifest, index: &AssetIndex) -> Result<Self, AssetError> {
        let redirects = RedirectTable::compile(&manifest.site.redirects).map_err(|e| {
            AssetError::UnusableRules {
                reason: e.to_string(),
            }
        })?;
        let headers = HeaderTable::compile(&manifest.site.headers).map_err(|e| {
            AssetError::UnusableRules {
                reason: e.to_string(),
            }
        })?;

        let not_found = manifest.site.not_found;
        let fallback = not_found.asset();
        if let Some(page) = fallback {
            if index.lookup_decoded(&format!("/{page}")).is_none() {
                return Err(AssetError::FallbackMissing { page });
            }
        }

        // A pattern that does not compile claims nothing rather than failing
        // the version: the route table is the isolate's business, and refusing
        // to load a version over it would take an application down for a
        // reason that has nothing to do with its static files. It is logged,
        // because the effect - a fallback answering a path a function was
        // meant to - is otherwise invisible.
        let mut function_routes = Vec::new();
        for route in &manifest.routes {
            if !claims_get(&route.methods) {
                continue;
            }
            match CompiledPattern::compile(&route.pattern) {
                Ok(pattern) => function_routes.push(pattern),
                Err(reason) => tracing::warn!(
                    pattern = %route.pattern,
                    "A route pattern in the manifest does not compile, so the static fallback \
                     will answer paths that route was meant to claim: {}",
                    reason
                ),
            }
        }

        Ok(Self {
            redirects,
            headers,
            not_found,
            fallback,
            function_routes,
            rewrite_miss_reported: AtomicBool::new(false),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.redirects.is_empty() && self.headers.is_empty() && self.not_found == NotFound::None
    }

    pub fn redirect_count(&self) -> usize {
        self.redirects.len()
    }

    pub fn header_block_count(&self) -> usize {
        self.headers.len()
    }

    /// Whether any `_headers` block exists at all.
    ///
    /// Asked before a path is split into segments, because the split is the
    /// only allocation the hit path would otherwise make and the answer is no
    /// for every site that ships no `_headers`.
    pub fn has_headers(&self) -> bool {
        !self.headers.is_empty()
    }

    pub fn not_found(&self) -> NotFound {
        self.not_found
    }

    /// What `_redirects` says about this path, if anything.
    pub fn redirect_for(&self, path_segments: &[String], query: &str) -> Option<RedirectAction> {
        self.redirects.apply(path_segments, query)
    }

    /// The headers `_headers` puts on a response for this path.
    pub fn headers_for(&self, path_segments: &[String]) -> Vec<HeaderField> {
        if self.headers.is_empty() {
            return Vec::new();
        }
        self.headers.apply(path_segments)
    }

    /// The asset and status the not-found behaviour answers with, if it
    /// applies to this path.
    pub fn fallback_for(&self, path_segments: &[String], scope: Lookup) -> Option<(&str, u16)> {
        let page = self.fallback?;
        if scope == Lookup::BeforeFunctions && self.claimed_by_a_function(path_segments) {
            return None;
        }
        Some((page, self.not_found.status()))
    }

    /// Whether a function route would answer this path.
    fn claimed_by_a_function(&self, path_segments: &[String]) -> bool {
        self.function_routes
            .iter()
            .any(|pattern| pattern.match_segments(path_segments).is_some())
    }

    /// A rewrite matched and its target is not in the bundle.
    ///
    /// Loud once, quiet afterwards. The build checks every target it can work
    /// out on its own, so reaching this means a target built out of a capture
    /// expanded to a file that is not there - real, and worth an operator
    /// seeing, but driven by request traffic and therefore unbounded.
    pub fn report_rewrite_miss(&self, request_path: &str, target: &str, app: &str) {
        if !self.rewrite_miss_reported.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                app_id = %app,
                request_path,
                target,
                "A _redirects rule matched and its target is not a file in this bundle, so the \
                 request fell through to the application instead. Check the rule's target; only \
                 targets built out of :captures reach this, because the build checks fixed ones. \
                 Later occurrences are logged at debug"
            );
        } else {
            tracing::debug!(
                app_id = %app,
                request_path,
                target,
                "A _redirects rewrite target is not in the bundle"
            );
        }
    }
}

/// Whether a route's method list would take a GET.
///
/// An empty list is the manifest's "any method", which is what `deis-build`
/// always writes today; the check is here because the field exists and a
/// manifest that fills it in must not be silently ignored.
fn claims_get(methods: &[String]) -> bool {
    methods.is_empty()
        || methods
            .iter()
            .any(|method| method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD"))
}
