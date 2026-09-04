//! The Node surface promised by Edge Canon.
//!
//! Build-time validation and the self-hosted runtime both consume this crate.
//! Keeping the inventory here prevents a source graph from being accepted by
//! the compiler and then interpreted against a different runtime allow-list.

pub const NODE_SEMANTIC_BASELINE: &str = "24.20.0";

/// Conditions Edge Canon adds to the resolver's syntax-specific `import` or
/// `require` condition and its standard `browser`/`default` conditions.
///
/// Package condition objects retain Node's declaration-order semantics; this
/// list is the exact additional active set, not a provider-specific priority
/// override.
pub const NODE_CUSTOM_CONDITIONS: [&str; 2] = ["edge-canon", "worker"];
pub const NODE_IMPORT_CONDITIONS: [&str; 5] =
    ["edge-canon", "worker", "browser", "import", "default"];
pub const NODE_REQUIRE_CONDITIONS: [&str; 5] =
    ["edge-canon", "worker", "browser", "require", "default"];

pub const NODE_BUILTIN_MODULES: [&str; 18] = [
    "node:assert",
    "node:assert/strict",
    "node:async_hooks",
    "node:buffer",
    "node:crypto",
    "node:diagnostics_channel",
    "node:events",
    "node:path",
    "node:process",
    "node:querystring",
    "node:stream",
    "node:stream/promises",
    "node:string_decoder",
    "node:timers",
    "node:timers/promises",
    "node:url",
    "node:util",
    "node:zlib",
];

/// Return the canonical `node:` spelling for a selected builtin.
pub fn canonical_builtin(specifier: &str) -> Option<&'static str> {
    let name = specifier.strip_prefix("node:").unwrap_or(specifier);
    Some(match name {
        "assert" => "node:assert",
        "assert/strict" => "node:assert/strict",
        "async_hooks" => "node:async_hooks",
        "buffer" => "node:buffer",
        "crypto" => "node:crypto",
        "diagnostics_channel" => "node:diagnostics_channel",
        "events" => "node:events",
        "path" => "node:path",
        "process" => "node:process",
        "querystring" => "node:querystring",
        "stream" => "node:stream",
        "stream/promises" => "node:stream/promises",
        "string_decoder" => "node:string_decoder",
        "timers" => "node:timers",
        "timers/promises" => "node:timers/promises",
        "url" => "node:url",
        "util" => "node:util",
        "zlib" => "node:zlib",
        _ => return None,
    })
}

/// The exact named exports an application may import from a selected builtin.
pub fn builtin_exports(specifier: &str) -> Option<&'static [&'static str]> {
    Some(match specifier {
        "node:assert" | "node:assert/strict" => &[
            "AssertionError",
            "deepEqual",
            "deepStrictEqual",
            "doesNotMatch",
            "doesNotReject",
            "doesNotThrow",
            "equal",
            "fail",
            "ifError",
            "match",
            "notDeepEqual",
            "notDeepStrictEqual",
            "notEqual",
            "notStrictEqual",
            "ok",
            "rejects",
            "strictEqual",
            "throws",
        ],
        "node:async_hooks" => &["AsyncLocalStorage"],
        "node:buffer" => &["Buffer"],
        "node:crypto" => &[
            "createHash",
            "createHmac",
            "randomBytes",
            "randomUUID",
            "timingSafeEqual",
            "webcrypto",
        ],
        "node:diagnostics_channel" => &["channel", "hasSubscribers"],
        "node:events" => &["EventEmitter", "once"],
        "node:path" => &[
            "basename",
            "delimiter",
            "dirname",
            "extname",
            "format",
            "isAbsolute",
            "join",
            "normalize",
            "parse",
            "posix",
            "relative",
            "resolve",
            "sep",
            "toNamespacedPath",
            "win32",
        ],
        "node:process" => &[
            "env",
            "getBuiltinModule",
            "nextTick",
            "platform",
            "version",
            "versions",
        ],
        "node:querystring" => &[
            "decode",
            "encode",
            "escape",
            "parse",
            "stringify",
            "unescape",
        ],
        "node:stream" => &[
            "Duplex",
            "PassThrough",
            "Readable",
            "Transform",
            "Writable",
            "finished",
            "pipeline",
        ],
        "node:stream/promises" => &["finished", "pipeline"],
        "node:string_decoder" => &["StringDecoder"],
        "node:timers" => &[
            "clearImmediate",
            "clearInterval",
            "clearTimeout",
            "setImmediate",
            "setInterval",
            "setTimeout",
        ],
        "node:timers/promises" => &["setTimeout"],
        "node:url" => &[
            "URL",
            "URLSearchParams",
            "domainToASCII",
            "domainToUnicode",
            "fileURLToPath",
            "pathToFileURL",
            "urlToHttpOptions",
        ],
        "node:util" => &[
            "callbackify",
            "format",
            "formatWithOptions",
            "inherits",
            "inspect",
            "promisify",
            "stripVTControlCharacters",
        ],
        "node:zlib" => &[
            "brotliCompressSync",
            "brotliDecompressSync",
            "constants",
            "deflateSync",
            "gzipSync",
            "gunzipSync",
            "inflateSync",
        ],
        _ => return None,
    })
}

/// Whether a spelling is a Node 24 builtin, including the selected subset.
/// This keeps an excluded bare builtin from being mistaken for an npm package
/// by the compiler or for a relative module by the runtime backstop.
pub fn is_known_node_builtin(specifier: &str) -> bool {
    let bare = specifier.strip_prefix("node:").unwrap_or(specifier);
    matches!(
        bare,
        "_http_agent"
            | "_http_client"
            | "_http_common"
            | "_http_incoming"
            | "_http_outgoing"
            | "_http_server"
            | "_stream_duplex"
            | "_stream_passthrough"
            | "_stream_readable"
            | "_stream_transform"
            | "_stream_wrap"
            | "_stream_writable"
            | "_tls_common"
            | "_tls_wrap"
            | "child_process"
            | "cluster"
            | "console"
            | "constants"
            | "dgram"
            | "dns"
            | "dns/promises"
            | "domain"
            | "fs"
            | "fs/promises"
            | "http"
            | "http2"
            | "https"
            | "inspector"
            | "inspector/promises"
            | "module"
            | "net"
            | "os"
            | "path/posix"
            | "path/win32"
            | "perf_hooks"
            | "punycode"
            | "readline"
            | "readline/promises"
            | "repl"
            | "sea"
            | "sqlite"
            | "stream/consumers"
            | "stream/web"
            | "sys"
            | "test"
            | "test/reporters"
            | "tls"
            | "trace_events"
            | "tty"
            | "util/types"
            | "v8"
            | "vm"
            | "wasi"
            | "worker_threads"
    ) || canonical_builtin(specifier).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_has_exports_and_bare_alias() {
        for builtin in NODE_BUILTIN_MODULES {
            assert!(!builtin_exports(builtin).unwrap().is_empty());
            assert_eq!(canonical_builtin(builtin), Some(builtin));
            assert_eq!(
                canonical_builtin(builtin.strip_prefix("node:").unwrap()),
                Some(builtin)
            );
        }
    }

    #[test]
    fn resolver_conditions_are_exact_and_syntax_specific() {
        assert_eq!(NODE_CUSTOM_CONDITIONS, ["edge-canon", "worker"]);
        assert_eq!(
            NODE_IMPORT_CONDITIONS,
            ["edge-canon", "worker", "browser", "import", "default"]
        );
        assert_eq!(
            NODE_REQUIRE_CONDITIONS,
            ["edge-canon", "worker", "browser", "require", "default"]
        );
    }
}
