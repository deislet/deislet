//! `_redirects`: one rule per line, first match wins.
//!
//! ```text
//! # comment
//! /old-post            /blog/new-post        301
//! /blog/*              /news/:splat          301
//! /news/:year/:slug    /archive/:year/:slug  302
//! # 200 is a rewrite, not a redirect: the URL stays put
//! /app/*               /app/index.html       200
//! /private/*           /404.html             404
//! ```
//!
//! The order in the file **is** the semantics. Nothing here sorts the table:
//! an author who put a broad rule above a narrow one meant the broad one to
//! win, and re-ordering by specificity - which is right for the *route* table,
//! see [`crate::routing`] - would silently change what their site does.
//!
//! Three statuses do something other than redirect:
//!
//!   * **200** is a rewrite. The requested URL stays in the address bar and
//!     the target's bytes are served under it. This is how an SPA fallback is
//!     written by hand, and how a pretty URL is served from a real file.
//!   * **404** serves the target with a 404 status, which is what a static
//!     site wants for a path it knows is gone.
//!   * every other allowed status is an ordinary redirect with a `Location`.
//!
//! ## Off-site targets
//!
//! An absolute `http(s)://` target is **allowed for a redirect and refused for
//! a rewrite**, and only when the host it names is a constant.
//!
//! Allowing it at all is a decision, because a bundle is author-controlled and
//! not necessarily trusted. It is allowed because moving a path to another
//! host is an ordinary thing for a site to do, and refusing it would push the
//! author into writing a function - the exact outcome this whole file exists
//! to avoid. What is refused is the shape that turns a redirect rule into
//! somebody *else's* redirector:
//!
//!   * a capture anywhere in the scheme or the authority, so a visitor can
//!     never choose the host they are sent to (`/r/:host  https://:host/`);
//!   * userinfo, because `https://trusted.example@evil.example` reads as the
//!     trusted host to a person and resolves to the other one;
//!   * a scheme that is not http or https, which is where `javascript:` and
//!     `data:` targets would otherwise live;
//!   * a protocol-relative `//host/path`, and `/\host/path`, which several
//!     browsers read the same way - both are the classic typo for a path and
//!     would send visitors off-site without the author noticing.
//!
//! A rewrite to an absolute URL is refused outright rather than restricted: a
//! rewrite serves bytes out of *this* bundle, so honouring one would mean the
//! node opening a socket the author picked, from the asset path, bypassing
//! `fetch_policy.rs` - the platform's single outbound decision point.

use serde::{Deserialize, Serialize};

use crate::path::percent_encode_uri;
use crate::routing::{Captures, CompiledPattern};
use crate::site::{REDIRECTS_FILE, RuleError};

/// The status a rule with no third field means.
///
/// 302, matching every other static host: a redirect an author writes without
/// thinking about caching should be the one a browser will ask about again.
pub const DEFAULT_STATUS: u16 = 302;

/// The statuses a rule may carry.
///
/// 200 and 404 are here because they are not redirects at all - see the module
/// documentation. 300, 305 and 306 are not: none of them names a target the
/// way a `Location` does.
pub const ALLOWED_STATUSES: [u16; 7] = [200, 301, 302, 303, 307, 308, 404];

/// Whether this build can carry out a rule with this status.
///
/// Its own function because the two readers of the answer want opposite
/// things: [`parse_redirects`] is looking at a file an author has open and
/// refuses what is not here, while [`RedirectTable::compile`] is reading a
/// manifest a possibly newer builder wrote and drops the rule instead. See
/// [`crate::forward`] for why those have to differ.
pub fn status_is_known(status: u16) -> bool {
    ALLOWED_STATUSES.contains(&status)
}

/// How many rules one bundle may carry.
///
/// The table is scanned linearly on every request that misses the asset index,
/// so its length is a per-request cost paid by every visitor. Two thousand is
/// far past what a generated `_redirects` reaches and still bounded; a build
/// that exceeds it fails and says so rather than shipping a site whose 404s
/// are measurably slow.
pub const MAX_RULES: usize = 2000;

/// The capture name a trailing `*` fills in a target.
pub const SPLAT_NAME: &str = "splat";

/// One line of `_redirects`, as it travels in the manifest.
///
/// Text rather than a pre-compiled form on purpose: a manifest is something an
/// operator reads and the control plane stores verbatim, and the compiled
/// shape is derived from this in one place ([`RedirectTable::compile`]) that
/// both the build and the node run. Storing the compiled form would mean a
/// node trusting a structure it did not build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedirectRule {
    pub from: String,
    pub to: String,
    #[serde(default = "default_status")]
    pub status: u16,
}

fn default_status() -> u16 {
    DEFAULT_STATUS
}

/// What a matching rule says to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectAction {
    /// Send the client elsewhere. `location` is already escaped for a header.
    Redirect { status: u16, location: String },
    /// Serve a different asset under the requested URL. `status` is 200 for a
    /// rewrite and 404 for an explicit not-found rule.
    Rewrite { status: u16, target: String },
}

/// One rule, compiled: the pattern matched against a path and the pieces its
/// target is rebuilt from.
#[derive(Debug, Clone)]
struct Compiled {
    pattern: CompiledPattern,
    target: Vec<TargetPart>,
    status: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetPart {
    Literal(String),
    /// A `:name` from the pattern, or `:splat`.
    Capture(String),
}

/// The whole table, compiled once.
#[derive(Debug, Clone, Default)]
pub struct RedirectTable {
    rules: Vec<Compiled>,
}

impl RedirectTable {
    /// Compile a table, or say which rule is wrong and why.
    ///
    /// Runs at build time on what the author wrote and again on a node when a
    /// version loads. The second run is not redundant: the manifest arrives
    /// from outside the process, and a rule that only the build had checked
    /// would be a rule a crafted bundle could smuggle past.
    ///
    /// A rule whose status this build does not know is **dropped** rather than
    /// refused, and it is the one thing this differs from
    /// [`parse_redirects`] on. [`ALLOWED_STATUSES`] is an allow-list that
    /// grows, so refusing here cost the whole bundle on any node one release
    /// behind the builder - see [`crate::forward`]. Dropping the rule is the
    /// narrow reading: the asset index is consulted before this table, so a
    /// dropped rule cannot uncover a file it was hiding, and what answers
    /// instead is another of the author's own rules, the application, or the
    /// not-found behaviour. Substituting a status this build does know would
    /// be worse than any of those - a 301 answered as a 302, or the reverse,
    /// is a wrong entry in a visitor's browser cache that outlives the
    /// rollback that produced it.
    pub fn compile(rules: &[RedirectRule]) -> Result<Self, RuleError> {
        if rules.len() > MAX_RULES {
            return Err(RuleError::at(
                REDIRECTS_FILE,
                0,
                format!("{} rules", rules.len()),
                format!(
                    "a bundle may carry at most {MAX_RULES} redirect rules; the table is scanned \
                     on every request that does not name a file, so its length is a cost every \
                     visitor pays"
                ),
            ));
        }

        let mut compiled = Vec::with_capacity(rules.len());
        for rule in rules {
            if !status_is_known(rule.status) {
                continue;
            }
            compiled.push(compile_rule(rule, 0)?);
        }
        Ok(Self { rules: compiled })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// The first rule that matches, applied.
    ///
    /// `path_segments` comes from [`crate::routing::decoded_segments`], so one
    /// request costs one decode however long the table is. `query` is the
    /// request's query string without the `?`, and is appended to a redirect
    /// target that does not carry one of its own - a rule that moves a page
    /// should not lose the `?utm_source=` a visitor arrived with.
    pub fn apply(&self, path_segments: &[String], query: &str) -> Option<RedirectAction> {
        for rule in &self.rules {
            let Some(captures) = rule.pattern.match_segments(path_segments) else {
                continue;
            };
            let target = expand(&rule.target, &captures);
            return Some(match rule.status {
                200 | 404 => RedirectAction::Rewrite {
                    status: rule.status,
                    target,
                },
                status => {
                    let location = if query.is_empty() || target.contains('?') {
                        target
                    } else {
                        format!("{target}?{query}")
                    };
                    RedirectAction::Redirect {
                        status,
                        location: percent_encode_uri(&location),
                    }
                }
            });
        }
        None
    }
}

/// The rules that will serve one fixed file out of this bundle.
///
/// A build uses it to check the file is actually there. Only 200 and 404 rules
/// qualify: those two *serve* the target, so a target that is not a file means
/// a rule that matches and then has nothing to send. A 3xx target is a URL the
/// browser will ask for next, and it is perfectly normal for that to be a
/// function route or another rule rather than a file - checking it against the
/// asset list would fail builds that are correct.
///
/// A target built out of captures cannot be checked this way either and is
/// left alone; guessing what it expands to would fail correct builds for the
/// same reason.
pub fn literal_asset_targets(rules: &[RedirectRule]) -> Vec<(usize, String)> {
    rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| matches!(rule.status, 200 | 404))
        .filter(|(_, rule)| !is_absolute(&rule.to))
        .filter_map(
            |(index, rule)| match parse_target(&rule.to).ok()?.as_slice() {
                [TargetPart::Literal(path)] => Some((index, path.clone())),
                _ => None,
            },
        )
        .collect()
}

/// A parsed `_redirects`, with the line each rule came from.
///
/// The lines are kept beside the rules rather than inside them because a
/// [`RedirectRule`] is what travels in the manifest, where a line number from
/// a file nobody has any more would mean nothing. A build still has the file
/// open, and its later checks - does this target name a file that exists? -
/// have to be able to point at the line like the parser does.
#[derive(Debug, Clone, Default)]
pub struct ParsedRedirects {
    pub rules: Vec<RedirectRule>,
    /// 1-based, parallel to `rules`.
    pub lines: Vec<usize>,
}

impl ParsedRedirects {
    pub fn line_of(&self, index: usize) -> usize {
        self.lines.get(index).copied().unwrap_or(0)
    }
}

/// Parse a `_redirects` file.
///
/// Every rejection carries the line number and the line, because the whole
/// point of doing this at build time is that the author is still looking at
/// the file.
pub fn parse_redirects(text: &str) -> Result<ParsedRedirects, RuleError> {
    let mut rules = Vec::new();
    let mut lines = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let content = strip_comment(raw).trim();
        if content.is_empty() {
            continue;
        }

        let fields: Vec<&str> = content.split_whitespace().collect();
        let (from, to, status) = match fields.as_slice() {
            [from, to] => (*from, *to, DEFAULT_STATUS),
            [from, to, status] => {
                let parsed = status.parse::<u16>().map_err(|_| {
                    RuleError::at(
                        REDIRECTS_FILE,
                        line,
                        content,
                        format!(
                            "{status:?} is not a status code; write one of {ALLOWED_STATUSES:?}"
                        ),
                    )
                })?;
                (*from, *to, parsed)
            }
            [only] => {
                return Err(RuleError::at(
                    REDIRECTS_FILE,
                    line,
                    content,
                    format!("{only:?} has no target; a rule is \"from to [status]\""),
                ));
            }
            _ => {
                return Err(RuleError::at(
                    REDIRECTS_FILE,
                    line,
                    content,
                    format!(
                        "a rule is \"from to [status]\", which is two or three fields; this line \
                         has {}",
                        fields.len()
                    ),
                ));
            }
        };

        let rule = RedirectRule {
            from: from.to_string(),
            to: to.to_string(),
            status,
        };
        compile_rule(&rule, line)?;
        rules.push(rule);
        lines.push(line);
    }

    if rules.len() > MAX_RULES {
        return Err(RuleError::at(
            REDIRECTS_FILE,
            0,
            format!("{} rules", rules.len()),
            format!("a bundle may carry at most {MAX_RULES} redirect rules"),
        ));
    }

    Ok(ParsedRedirects { rules, lines })
}

/// Everything a single rule has to satisfy, in one place so the parser and the
/// node check exactly the same things.
fn compile_rule(rule: &RedirectRule, line: usize) -> Result<Compiled, RuleError> {
    let text = format!("{} {} {}", rule.from, rule.to, rule.status);
    let fail = |reason: String| RuleError::at(REDIRECTS_FILE, line, text.clone(), reason);

    if !status_is_known(rule.status) {
        return Err(fail(format!(
            "status {} is not one of {ALLOWED_STATUSES:?}; 200 rewrites, 404 serves the target \
             as a not-found page, and the rest are redirects",
            rule.status
        )));
    }

    if !rule.from.starts_with('/') {
        return Err(fail(format!(
            "the source {:?} has to be an absolute path starting with \"/\"; this table matches \
             paths on the domains the app already answers, not other hosts",
            rule.from
        )));
    }
    if rule.from.contains('?') || rule.from.contains('#') {
        return Err(fail(format!(
            "the source {:?} carries a query string or a fragment; rules match the path only, so \
             write the path and let the query through",
            rule.from
        )));
    }

    let pattern = CompiledPattern::compile(&rule.from).map_err(&fail)?;

    let absolute = is_absolute(&rule.to);
    if absolute && rule.status == 200 {
        return Err(fail(format!(
            "a 200 rule rewrites to a file in this bundle, and {:?} names another origin; the \
             node would have to fetch it, which the asset path does not do - write a function and \
             call fetch() if that is what you want",
            rule.to
        )));
    }
    if absolute && rule.status == 404 {
        return Err(fail(format!(
            "a 404 rule serves a page from this bundle, and {:?} names another origin",
            rule.to
        )));
    }

    let target = if absolute {
        let (origin, rest) = split_origin(&rule.to);
        check_origin(origin).map_err(&fail)?;
        let mut parts = vec![TargetPart::Literal(origin.to_string())];
        // A bare origin is a legal target; only the part after it can carry
        // captures, which is what keeps `:8080` from reading as one.
        if !rest.is_empty() {
            parts.extend(parse_target(rest).map_err(&fail)?);
        }
        parts
    } else {
        // `/\host` is read as protocol-relative by several browsers, so it is
        // the same off-site redirect as `//host` wearing a disguise.
        if rule.to.starts_with("/\\") {
            return Err(fail(format!(
                "the target {:?} starts with a backslash after its slash, which several browsers \
                 read as another host rather than a path on this one",
                rule.to
            )));
        }
        if rule.to.starts_with("//") {
            return Err(fail(format!(
                "the target {:?} starts with \"//\", which a browser reads as another host rather \
                 than a path on this one; write \"https://host/...\" if that is what you meant, or \
                 a single leading slash if it is not",
                rule.to
            )));
        }
        if !rule.to.starts_with('/') {
            return Err(fail(format!(
                "the target {:?} has to be an absolute path starting with \"/\", or a full \
                 http(s) URL",
                rule.to
            )));
        }
        parse_target(&rule.to).map_err(&fail)?
    };

    // A target naming a capture the source never takes would expand to the
    // literal text of the placeholder, which is an author's typo showing up as
    // a broken link rather than a build failure.
    for part in &target {
        if let TargetPart::Capture(name) = part {
            if name == SPLAT_NAME {
                if !pattern.has_catch_all() {
                    return Err(fail(format!(
                        ":{SPLAT_NAME} stands for what a trailing \"*\" matched, and the source \
                         {:?} has no \"*\"",
                        rule.from
                    )));
                }
                continue;
            }
            if !source_names(&rule.from).any(|declared| declared == name) {
                return Err(fail(format!(
                    "the target names :{name}, which the source {:?} does not capture",
                    rule.from
                )));
            }
        }
    }

    Ok(Compiled {
        pattern,
        target,
        status: rule.status,
    })
}

/// The `:name` placeholders a source pattern declares.
fn source_names(from: &str) -> impl Iterator<Item = &str> {
    crate::routing::segments(from)
        .into_iter()
        .filter_map(|segment| segment.strip_prefix(':'))
        .filter(|name| !name.is_empty())
}

/// Split a target into its literal text and the captures it substitutes.
///
/// A `:` that is not followed by a name character is literal text, which is
/// what keeps `https://` and `:8080` from reading as captures.
fn parse_target(text: &str) -> Result<Vec<TargetPart>, String> {
    let mut parts = Vec::new();
    let mut literal = String::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == ':' {
            let start = index + 1;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == '_') {
                end += 1;
            }
            if end > start {
                if !literal.is_empty() {
                    parts.push(TargetPart::Literal(std::mem::take(&mut literal)));
                }
                parts.push(TargetPart::Capture(bytes[start..end].iter().collect()));
                index = end;
                continue;
            }
        }
        literal.push(bytes[index]);
        index += 1;
    }

    if !literal.is_empty() {
        parts.push(TargetPart::Literal(literal));
    }
    if parts.is_empty() {
        return Err("the target is empty".to_string());
    }
    Ok(parts)
}

fn expand(target: &[TargetPart], captures: &Captures) -> String {
    let mut out = String::new();
    for part in target {
        match part {
            TargetPart::Literal(text) => out.push_str(text),
            TargetPart::Capture(name) if name == SPLAT_NAME => {
                out.push_str(captures.splat.as_deref().unwrap_or(""))
            }
            TargetPart::Capture(name) => out.push_str(captures.get(name).unwrap_or("")),
        }
    }
    out
}

/// Whether a target names another origin.
fn is_absolute(target: &str) -> bool {
    let lowered = target.to_ascii_lowercase();
    lowered.starts_with("http://") || lowered.starts_with("https://")
}

/// Split `https://host:8080/a/b` into `("https://host:8080", "/a/b")`.
fn split_origin(target: &str) -> (&str, &str) {
    let after_scheme = target.find("://").map(|at| at + 3).unwrap_or(0);
    match target[after_scheme..].find(['/', '?', '#']) {
        Some(offset) => target.split_at(after_scheme + offset),
        None => (target, ""),
    }
}

/// The rule that keeps an author's redirect from becoming a visitor's.
///
/// Letters, digits, dots and hyphens, plus one optional numeric port. That
/// refuses a capture (`:slug` is a colon followed by a letter), userinfo (`@`),
/// a wildcard, and anything else that would let the destination host depend on
/// the request.
fn check_origin(origin: &str) -> Result<(), String> {
    let authority = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin);
    if authority.is_empty() {
        return Err(format!("{origin:?} names no host"));
    }
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(format!(
            "the host in {origin:?} has to be a constant made of letters, digits, dots and \
             hyphens; a capture or a userinfo section there would let whoever sends the request \
             choose where the redirect goes"
        ));
    }
    if let Some(port) = port
        && (port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()))
    {
        return Err(format!(
            "the port in {origin:?} has to be digits; anything else in the authority would let \
             the request choose the destination"
        ));
    }
    Ok(())
}

/// A line whose first non-blank character is `#` is a comment.
///
/// Only a whole line, never a trailing one: `#` is legal in a redirect target,
/// where it introduces a fragment, and treating `/old /new#section 301` as a
/// rule with a truncated target would break a link the author can see is
/// correct.
fn strip_comment(line: &str) -> &str {
    if line.trim_start().starts_with('#') {
        ""
    } else {
        line
    }
}

#[cfg(test)]
#[path = "redirects_test.rs"]
mod redirects_test;
