//! Runtime isolation for provider globals outside the unified application API.

/// Provider-owned runtime globals that the current Edge Canon intersection
/// deliberately excludes from application code.
pub const PROVIDER_ONLY_GLOBALS: &[&str] = &["WebSocket", "WebSocketPair", "WebSocketServer"];

/// Execute before any application module body.
///
/// Static analysis gives authors precise build diagnostics, but computed or
/// reflective access cannot be exhaustively enumerated. Every generated
/// runtime entry therefore shadows the same provider globals with one sealed,
/// deterministic `undefined` value before evaluating application code.
pub fn provider_global_isolation_prelude() -> &'static str {
    r#";(() => {
  for (const name of ["WebSocket", "WebSocketPair", "WebSocketServer"]) {
    const current = Object.getOwnPropertyDescriptor(globalThis, name);
    if (current && current.value === undefined && current.writable === false && current.enumerable === false && current.configurable === false) continue;
    try {
      Object.defineProperty(globalThis, name, { value: undefined, writable: false, enumerable: false, configurable: false });
    } catch (_) {
      const error = new Error(`Edge Canon could not isolate provider global ${name}`);
      error.name = "EdgeCanonIsolationError";
      error.code = "EC_STREAM_PROVIDER_GLOBAL_EXPOSED";
      throw error;
    }
  }
})();
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prelude_and_static_policy_share_one_exact_name_set() {
        let prelude = provider_global_isolation_prelude();
        for name in PROVIDER_ONLY_GLOBALS {
            assert_eq!(prelude.matches(&format!("\"{name}\"")).count(), 1);
        }
    }
}
