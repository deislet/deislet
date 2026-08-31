// A telemetry module that measures nothing.
//
// deno_fetch loads `ext:deno_telemetry/telemetry.ts` on first use and reads
// its tracer, its context manager and two flags off `internals.__telemetry`;
// without the module the isolate fails to build. The real one is roughly two
// thousand lines of OpenTelemetry that this platform does not use - a node's
// own traces come from `deis-telemetry`, on the Rust side, where they can be
// attributed to a tenant - so what is here is the shape deno_fetch reads and
// nothing behind it.
//
// A script rather than an ES module, and registered with `lazy_loaded_js`,
// because that is what `core.loadExtScript` can fetch; the real crate declares
// its two files the same way.
(function () {
const { internals } = __bootstrap;

const TRACING_ENABLED = false;
const METRICS_ENABLED = false;
const PROPAGATORS = [];

class ContextManager {
  run(fn) {
    return fn();
  }
}

// Every method a caller might reach for, so a span that is handed around
// still behaves like a span; `isRecording()` answering false is what tells a
// caller not to bother building attributes for it.
const builtinTracer = {
  startSpan() {
    return {
      end() {},
      setAttribute() {},
      setAttributes() {},
      updateName() {},
      setStatus() {},
      addEvent() {},
      recordException() {},
      isRecording() {
        return false;
      },
      context() {
        return undefined;
      },
    };
  },
};

function enterSpan(_span, fn) {
  return fn();
}

function restoreSnapshot() {}

internals.__telemetry = {
  TRACING_ENABLED,
  METRICS_ENABLED,
  PROPAGATORS,
  ContextManager,
  builtinTracer,
  enterSpan,
  restoreSnapshot,
};

return internals.__telemetry;
})();
