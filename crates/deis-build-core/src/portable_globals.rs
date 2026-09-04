//! Runtime normalization before any application module is evaluated.

/// Provider-owned runtime globals that the current Edge Canon intersection
/// deliberately excludes from application code.
pub const PROVIDER_ONLY_GLOBALS: &[&str] = &["WebSocket", "WebSocketPair", "WebSocketServer"];

/// Execute before any application module body.
///
/// Static analysis gives authors precise build diagnostics, but computed or
/// reflective access cannot be exhaustively enumerated. Every generated
/// runtime entry therefore shadows the same provider globals with one sealed,
/// deterministic `undefined` value before evaluating application code. The
/// same boundary also turns the provider's no-argument transform into the
/// standard identity-byte transform and seals that constructor.
pub fn portable_runtime_prelude() -> &'static str {
    r#";(() => {
  const standardError = (message, code) => {
    const error = new TypeError(message);
    error.code = code;
    return error;
  };
  let providerGlobalsAlreadyIsolated = true;
  for (const name of ["WebSocket", "WebSocketPair", "WebSocketServer"]) {
    const current = Object.getOwnPropertyDescriptor(globalThis, name);
    if (current && current.value === undefined && current.writable === false && current.enumerable === false && current.configurable === false) continue;
    providerGlobalsAlreadyIsolated = false;
    try {
      Object.defineProperty(globalThis, name, { value: undefined, writable: false, enumerable: false, configurable: false });
    } catch (_) {
      const error = new Error(`Edge Canon could not isolate provider global ${name}`);
      error.name = "EdgeCanonIsolationError";
      error.code = "EC_STREAM_PROVIDER_GLOBAL_EXPOSED";
      throw error;
    }
  }

  const currentTransform = Object.getOwnPropertyDescriptor(globalThis, "TransformStream");
  const transformAlreadyNormalized = providerGlobalsAlreadyIsolated
    && currentTransform
    && typeof currentTransform.value === "function"
    && currentTransform.writable === false
    && currentTransform.enumerable === false
    && currentTransform.configurable === false;
  if (!transformAlreadyNormalized) {
    try {
      const NativeTransformStream = globalThis.TransformStream;
      if (typeof NativeTransformStream !== "function") throw new Error("TransformStream is unavailable");
      const probe = new NativeTransformStream();
      const writablePrototype = Object.getPrototypeOf(probe.writable);
      const probeWriter = probe.writable.getWriter();
      const writerPrototype = Object.getPrototypeOf(probeWriter);
      probeWriter.releaseLock();
      const getWriterDescriptor = Object.getOwnPropertyDescriptor(writablePrototype, "getWriter");
      const writeDescriptor = Object.getOwnPropertyDescriptor(writerPrototype, "write");
      if (typeof getWriterDescriptor?.value !== "function" || typeof writeDescriptor?.value !== "function") {
        throw new Error("stream writer prototypes are unavailable");
      }
      const nativeGetWriter = getWriterDescriptor.value;
      const nativeWrite = writeDescriptor.value;
      const identityWritables = new WeakSet();
      const identityWriters = new WeakSet();
      Object.defineProperty(writablePrototype, "getWriter", {
        ...getWriterDescriptor,
        value: function getWriter(...args) {
          const writer = Reflect.apply(nativeGetWriter, this, args);
          if (identityWritables.has(this)) identityWriters.add(writer);
          return writer;
        },
      });
      Object.defineProperty(writerPrototype, "write", {
        ...writeDescriptor,
        value: function write(chunk) {
          if (identityWriters.has(this) && !(chunk instanceof Uint8Array)) {
            return Promise.reject(standardError("Edge Canon identity transform accepts only Uint8Array chunks", "EC_STREAM_CHUNK_TYPE"));
          }
          return Reflect.apply(nativeWrite, this, [chunk]);
        },
      });
      const EdgeCanonTransformStream = function TransformStream(...args) {
        if (!new.target) throw new TypeError("TransformStream constructor requires 'new'");
        if (args.length !== 0) {
          throw standardError("Edge Canon v1 TransformStream accepts no arguments", "EC_STREAM_TRANSFORMER_NONPORTABLE");
        }
        const stream = new NativeTransformStream();
        identityWritables.add(stream.writable);
        return stream;
      };
      Object.defineProperty(EdgeCanonTransformStream, "prototype", {
        value: NativeTransformStream.prototype,
        writable: false,
        enumerable: false,
        configurable: false,
      });
      const constructorDescriptor = Object.getOwnPropertyDescriptor(NativeTransformStream.prototype, "constructor");
      Object.defineProperty(NativeTransformStream.prototype, "constructor", {
        ...constructorDescriptor,
        value: EdgeCanonTransformStream,
        writable: false,
        enumerable: false,
        configurable: false,
      });
      Object.defineProperty(globalThis, "TransformStream", {
        value: EdgeCanonTransformStream,
        writable: false,
        enumerable: false,
        configurable: false,
      });
    } catch (cause) {
      if (cause?.code === "EC_STREAM_TRANSFORMER_NONPORTABLE") throw cause;
      const error = new Error("Edge Canon could not install the identity-byte TransformStream boundary");
      error.name = "EdgeCanonIsolationError";
      error.code = "EC_STREAM_RUNTIME_INCOMPATIBLE";
      throw error;
    }
  }
})();
"#
}

/// Backward-compatible name retained for callers that only inspect the
/// provider-global portion of the now broader runtime boundary.
pub fn provider_global_isolation_prelude() -> &'static str {
    portable_runtime_prelude()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prelude_and_static_policy_share_one_exact_name_set() {
        let prelude = portable_runtime_prelude();
        for name in PROVIDER_ONLY_GLOBALS {
            assert_eq!(prelude.matches(&format!("\"{name}\"")).count(), 1);
        }
        assert!(prelude.contains("EC_STREAM_CHUNK_TYPE"));
        assert!(prelude.contains("EC_STREAM_TRANSFORMER_NONPORTABLE"));
        assert!(prelude.contains("EC_STREAM_RUNTIME_INCOMPATIBLE"));
    }
}
