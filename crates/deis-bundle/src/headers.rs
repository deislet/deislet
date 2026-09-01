//! `_headers`: a path pattern on its own line, its headers indented under it.
//!
//! ```text
//! # every response
//! /*
//!   X-Frame-Options: DENY
//!   Referrer-Policy: strict-origin-when-cross-origin
//!
//! /assets/*
//!   Cache-Control: public, max-age=31536000, immutable
//!
//! /downloads/:file
//!   Content-Disposition: attachment
//!   ! X-Robots-Tag
//! ```
//!
//! A line that begins with whitespace is a header; anything else begins a new
//! block. `Name: value` sets a header, `! Name` removes one - which is how an
//! author narrows something a broader block set.
//!
//! `!` reaches only what these blocks set. The headers the node computes for
//! an asset - `Content-Type`, `Content-Length`, `ETag`, and the default
//! `Cache-Control` - are not removable, because two of them are the response's
//! own framing and the other two are what makes a cache correct. Where an
//! author wants a different content type or cache policy, they set one.
//!
//! **All matching blocks apply, and a later one wins a conflict.** That is the
//! opposite of `_redirects`, where the first match wins, and it is deliberate:
//! headers compose (a site-wide security block plus a per-directory caching
//! block), while a redirect is a single decision about where a request goes.
//!
//! **A name written twice in one block is sent twice.** A response's header
//! list is not a map, and the two names authors repeat are exactly the two
//! that may not be combined into one comma-joined value:
//!
//! ```text
//! /*
//!   Link: </app.css>; rel=preload; as=style
//!   Link: </app.js>; rel=preload; as=script
//! ```
//!
//! sends both `Link` headers. "A later block wins a conflict" is about blocks:
//! a block that names `Link` again replaces the whole group an earlier block
//! wrote, at the position the name first took. Within one block the author is
//! listing values, not correcting themselves, so nothing is dropped. `! Name`
//! still takes every value of that name off - a name is all it can name.
//!
//! ## Headers an application may not set
//!
//! These are refused at build time, by name, with the line number:
//!
//!   * `content-length`, `transfer-encoding`, `connection`, `keep-alive`,
//!     `upgrade`, `te`, `trailer`, `proxy-authenticate`, `proxy-authorization`,
//!     `host` - message framing and hop-by-hop control. They describe the
//!     connection between the node and whatever is in front of it, not the
//!     asset. A `Content-Length` that disagrees with the body is the primitive
//!     every request-smuggling attack is built from.
//!   * `content-encoding` - the node and the proxy decide whether a response
//!     is compressed. Declaring an encoding the bytes are not in produces a
//!     response no client can read, and the bytes are not the author's to
//!     describe here.
//!   * `etag` - computed from the file's own digest, and it is what makes a
//!     cache correct. A constant tag pinned by an author would have caches
//!     keep the old file forever after a deploy.
//!   * anything beginning `x-deis-` - the platform's own headers. deis-proxy
//!     and the node route and authorise on them, so an application that could
//!     write them on a response could describe itself as another application.
//!
//! Everything else is allowed, `Content-Type` and `Cache-Control` included:
//! those two are most of the reason this file exists.
//!
//! ## The pattern language
//!
//! The same one a function route and a `_redirects` source are written in
//! ([`crate::routing`]): literal segments, `:name` for exactly one segment,
//! and `*` as the whole last segment for the rest of the path. A wildcard
//! *inside* a segment - `/*.jpg` - is refused at build time rather than half
//! understood, because a second dialect that only worked here would be a
//! second thing to keep in agreement forever. Per-extension headers are
//! written as a directory: put the files under `/img/` and match `/img/*`.

use serde::{Deserialize, Serialize};

use crate::routing::CompiledPattern;
use crate::site::{HEADERS_FILE, RuleError};

/// How many blocks one bundle may carry.
///
/// Every block is tested against every request that reaches the asset layer,
/// so the count is a per-request cost. Five hundred is well past a generated
/// `_headers` and still bounded.
pub const MAX_RULES: usize = 500;

/// The prefix the platform reserves for its own headers.
pub const PLATFORM_PREFIX: &str = "x-deis-";

/// Header names a bundle may not set, and why. Lower case, because that is how
/// a name is compared.
pub const REFUSED_HEADERS: [(&str, &str); 12] = [
    (
        "content-length",
        "it describes the body the node is sending, and a value that disagrees with it is a request-smuggling primitive",
    ),
    (
        "transfer-encoding",
        "it is message framing between the node and the proxy in front of it",
    ),
    (
        "connection",
        "it is hop-by-hop and belongs to the connection, not the response",
    ),
    (
        "keep-alive",
        "it is hop-by-hop and belongs to the connection, not the response",
    ),
    (
        "upgrade",
        "it is hop-by-hop; a static file cannot switch protocols",
    ),
    (
        "te",
        "it is hop-by-hop and belongs to the connection, not the response",
    ),
    (
        "trailer",
        "it is hop-by-hop and belongs to the connection, not the response",
    ),
    (
        "proxy-authenticate",
        "it is the proxy's to send, not the application's",
    ),
    (
        "proxy-authorization",
        "it is the proxy's to read, not the application's",
    ),
    (
        "host",
        "it is a request header; on a response it means nothing and confuses caches",
    ),
    (
        "content-encoding",
        "the node decides whether a response is compressed, and a declared encoding the bytes are not in is unreadable",
    ),
    (
        "etag",
        "it is the file's own digest, and pinning it would make caches keep the old file after a deploy",
    ),
];

/// One `Name: value` a block sets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderField {
    /// Lower case, because that is the only spelling two components can agree
    /// on without one of them normalising the other's.
    pub name: String,
    pub value: String,
}

/// One block of `_headers`, as it travels in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderRule {
    pub pattern: String,
    #[serde(default)]
    pub set: Vec<HeaderField>,
    /// Names this block takes off a response an earlier block set.
    #[serde(default)]
    pub remove: Vec<String>,
}

#[derive(Debug, Clone)]
struct Compiled {
    pattern: CompiledPattern,
    set: Vec<HeaderField>,
    remove: Vec<String>,
}

/// Every block, compiled once.
#[derive(Debug, Clone, Default)]
pub struct HeaderTable {
    rules: Vec<Compiled>,
}

impl HeaderTable {
    /// Compile the table, or say which block is wrong and why.
    ///
    /// Run by the build on what the author wrote and again by a node when a
    /// version loads: the manifest arrives from outside the process, and the
    /// refused-header list is a platform rule, not a lint.
    pub fn compile(rules: &[HeaderRule]) -> Result<Self, RuleError> {
        if rules.len() > MAX_RULES {
            return Err(RuleError::at(
                HEADERS_FILE,
                0,
                format!("{} blocks", rules.len()),
                format!("a bundle may carry at most {MAX_RULES} header blocks"),
            ));
        }

        let mut compiled = Vec::with_capacity(rules.len());
        for rule in rules {
            let fail =
                |reason: String| RuleError::at(HEADERS_FILE, 0, rule.pattern.clone(), reason);
            if !rule.pattern.starts_with('/') {
                return Err(fail(format!(
                    "the pattern {:?} has to be an absolute path starting with \"/\"",
                    rule.pattern
                )));
            }
            let pattern = CompiledPattern::compile(&rule.pattern).map_err(&fail)?;

            let mut set = Vec::with_capacity(rule.set.len());
            for field in &rule.set {
                let name = check_name(&field.name).map_err(&fail)?;
                let value = check_value(&field.name, &field.value).map_err(&fail)?;
                set.push(HeaderField { name, value });
            }
            let mut remove = Vec::with_capacity(rule.remove.len());
            for name in &rule.remove {
                remove.push(check_name(name).map_err(&fail)?);
            }

            compiled.push(Compiled {
                pattern,
                set,
                remove,
            });
        }
        Ok(Self { rules: compiled })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// The headers to put on a response for this path.
    ///
    /// The result is a list, not a map, and the difference is the whole of
    /// this function. A response's header list may carry the same name twice,
    /// and for the two names authors repeat most - `Link` with several
    /// `rel=preload` targets, and `Set-Cookie` with a session and a CSRF
    /// cookie - it *has* to, because neither may be folded into one
    /// comma-joined value. Keeping one value per name sent the last one and
    /// said nothing about the rest, which is a preload that never happens or a
    /// login that stops working.
    ///
    /// So the two directions are separated:
    ///
    ///   * Two lines with the same name *inside one block* both survive, in
    ///     the order written. That is an author listing two values.
    ///   * A block that sets a name replaces every value an *earlier* block
    ///     gave it, at the position that name first took. That is the
    ///     documented "a later block wins a conflict", and it has to stay:
    ///     a per-directory `Cache-Control` overrules the site-wide one rather
    ///     than adding a second, contradictory value next to it.
    ///
    /// `!` still takes every value of a name off, because a name is the only
    /// thing an author can write there.
    ///
    /// Order is fixed rather than incidental: it is what makes two responses
    /// to the same path byte-identical, so a diff between two nodes means
    /// something. Names were lower-cased by [`check_name`] on the way in, so
    /// comparing them exactly here is comparing them the way they were
    /// normalised - unlike the isolate's response path, which sees whatever
    /// spelling the handler wrote and has to compare without regard to case.
    pub fn apply(&self, path_segments: &[String]) -> Vec<HeaderField> {
        let mut out: Vec<HeaderField> = Vec::new();
        for rule in &self.rules {
            if rule.pattern.match_segments(path_segments).is_none() {
                continue;
            }
            for name in &rule.remove {
                out.retain(|field| &field.name != name);
            }
            let mut written = HeaderNamesWritten::for_fields(&rule.set);
            for field in &rule.set {
                if !written.replaces(&field.name) {
                    // The block has already taken this name over from the
                    // earlier blocks, so its second `Link:` line joins its
                    // first instead of replacing the group it just wrote.
                    let after = out
                        .iter()
                        .rposition(|existing| existing.name == field.name)
                        .map_or(out.len(), |index| index + 1);
                    out.insert(after, field.clone());
                    continue;
                }
                match out.iter().position(|existing| existing.name == field.name) {
                    Some(first) => {
                        // Everything dropped sat at or after `first`, so the
                        // list never shrinks past it and the name keeps the
                        // place it first took.
                        out.retain(|existing| existing.name != field.name);
                        out.insert(first, field.clone());
                    }
                    None => out.push(field.clone()),
                }
            }
        }
        out
    }
}

/// The header names a writer has put down so far, so that it can tell a value
/// that *replaces* from a value that *joins*.
///
/// That one bit is the whole of the rule every layer that writes these headers
/// needs, and it is why it lives here rather than as three loops that agree
/// today. The first value of a name replaces whatever the layer underneath
/// already had under it - the content type the index guessed, the node's own
/// cache policy - and every value after it joins that name's group. A writer
/// that replaced on every value kept the last one and dropped the rest, which
/// is one `Link` of two preloads and one `Set-Cookie` of a session and a CSRF
/// cookie: a preload that never happens, and a login that stops working. That
/// defect has been found three times on this platform - on the isolate's
/// response path, in the cache, and here - each time in a place that had
/// written its own copy of this loop.
///
/// The callers write onto three different things: [`HeaderTable::apply`] onto
/// its own `Vec<HeaderField>`, the node onto the `http::HeaderMap` of a
/// finished response, and `env.ASSETS.fetch()` onto the name/value pairs an op
/// hands to JavaScript. None of those types is this crate's to know about, so
/// what they share is this decision and not the write itself.
///
/// It is driven by the caller, one call per value it is actually going to
/// write, rather than handed out as an iterator over the whole list. That is
/// deliberate: the node drops a value that turns out not to be a legal HTTP
/// header, and if a dropped value had already claimed its name, the next value
/// of that name would join a group that was never written and sit beside the
/// node's own value instead of replacing it.
///
/// Names arrive lower-cased from [`check_name`], so they are compared exactly.
/// A caller holding names in whatever spelling a handler wrote - the isolate's
/// response path is the one - has to compare without regard to case instead,
/// which is why that path does not use this.
#[derive(Debug, Default)]
pub struct HeaderNamesWritten<'a> {
    // A vector rather than a set: a block holds a handful of names, and a
    // linear scan over that beats hashing every one of them.
    seen: Vec<&'a str>,
}

impl<'a> HeaderNamesWritten<'a> {
    /// Start on a list of this many fields.
    pub fn for_fields(fields: &[HeaderField]) -> Self {
        Self {
            seen: Vec::with_capacity(fields.len()),
        }
    }

    /// Record that `name` is being written, and say whether this write is the
    /// first of that name - the one that replaces, rather than joins.
    pub fn replaces(&mut self, name: &'a str) -> bool {
        if self.seen.contains(&name) {
            return false;
        }
        self.seen.push(name);
        true
    }
}

/// Parse a `_headers` file.
pub fn parse_headers(text: &str) -> Result<Vec<HeaderRule>, RuleError> {
    let mut rules: Vec<HeaderRule> = Vec::new();
    // Where the block being filled in started, so an empty one can name its
    // own line rather than the end of the file.
    let mut open_at: Option<usize> = None;

    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        if raw.trim().is_empty() || raw.trim_start().starts_with('#') {
            continue;
        }

        let indented = raw.starts_with([' ', '\t']);
        let content = raw.trim();

        if !indented {
            if let Some(at) = open_at
                && rules
                    .last()
                    .map(|r| r.set.is_empty() && r.remove.is_empty())
                    == Some(true)
            {
                return Err(empty_block(at, &rules));
            }
            let fail = |reason: String| RuleError::at(HEADERS_FILE, line, content, reason);
            if !content.starts_with('/') {
                return Err(fail(format!(
                    "{content:?} is neither an indented header nor a path pattern; a block starts \
                     with a path beginning \"/\" and its headers are indented under it"
                )));
            }
            CompiledPattern::compile(content).map_err(&fail)?;
            rules.push(HeaderRule {
                pattern: content.to_string(),
                set: Vec::new(),
                remove: Vec::new(),
            });
            open_at = Some(line);
            continue;
        }

        let fail = |reason: String| RuleError::at(HEADERS_FILE, line, content, reason);
        let Some(rule) = rules.last_mut() else {
            return Err(fail(
                "this header has no path pattern above it; a block is a path on its own line with \
                 its headers indented under it"
                    .to_string(),
            ));
        };

        if let Some(name) = content.strip_prefix('!') {
            rule.remove.push(check_name(name.trim()).map_err(&fail)?);
            continue;
        }

        let Some((name, value)) = content.split_once(':') else {
            return Err(fail(format!(
                "{content:?} is not \"Name: value\"; a header line needs a colon, and a line that \
                 was meant to start a new block must not be indented"
            )));
        };
        rule.set.push(HeaderField {
            name: check_name(name.trim()).map_err(&fail)?,
            value: check_value(name.trim(), value.trim()).map_err(&fail)?,
        });
    }

    if let Some(at) = open_at
        && rules
            .last()
            .map(|r| r.set.is_empty() && r.remove.is_empty())
            == Some(true)
    {
        return Err(empty_block(at, &rules));
    }

    if rules.len() > MAX_RULES {
        return Err(RuleError::at(
            HEADERS_FILE,
            0,
            format!("{} blocks", rules.len()),
            format!("a bundle may carry at most {MAX_RULES} header blocks"),
        ));
    }

    Ok(rules)
}

fn empty_block(line: usize, rules: &[HeaderRule]) -> RuleError {
    let pattern = rules.last().map(|r| r.pattern.clone()).unwrap_or_default();
    RuleError::at(
        HEADERS_FILE,
        line,
        pattern,
        "this block has no headers under it; header lines have to be indented, and a block with \
         none does nothing"
            .to_string(),
    )
}

/// Lower-case the name and refuse the ones an application may not set.
fn check_name(name: &str) -> Result<String, String> {
    if name.is_empty() {
        return Err("a header line has no name before its colon".to_string());
    }
    if !name.bytes().all(is_token_byte) {
        return Err(format!(
            "{name:?} is not a header name; a name is letters, digits and the characters \
             !#$%&'*+-.^_`|~"
        ));
    }
    let lowered = name.to_ascii_lowercase();
    if let Some((_, why)) = REFUSED_HEADERS.iter().find(|(known, _)| *known == lowered) {
        return Err(format!("a bundle may not set {lowered:?}: {why}"));
    }
    if lowered.starts_with(PLATFORM_PREFIX) {
        return Err(format!(
            "{lowered:?} is in the {PLATFORM_PREFIX}* range the platform reserves for itself; the \
             proxy and the node route and authorise on those headers"
        ));
    }
    Ok(lowered)
}

/// Refuse a value that could not be sent, or that would end the header block
/// early and start writing a response of its own.
fn check_value(name: &str, value: &str) -> Result<String, String> {
    for byte in value.bytes() {
        if byte == b'\r' || byte == b'\n' {
            return Err(format!(
                "the value of {name:?} contains a carriage return or newline; that would end the \
                 response headers early and let the rest be read as a response of its own"
            ));
        }
        if byte < 0x20 || byte == 0x7f {
            return Err(format!(
                "the value of {name:?} contains a control character (byte {byte:#04x})"
            ));
        }
        if byte >= 0x80 {
            return Err(format!(
                "the value of {name:?} is not ASCII; a header value has no agreed encoding, so \
                 write the escaped form the header's own syntax defines"
            ));
        }
    }
    Ok(value.to_string())
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

#[cfg(test)]
#[path = "headers_test.rs"]
mod headers_test;
