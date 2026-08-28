export const TRACING_ENABLED = false;
export const METRICS_ENABLED = false;
export const PROPAGATORS = [];

export class ContextManager {
  run(fn) {
    return fn();
  }
}

export const builtinTracer = {
  startSpan() {
    return {
      end() {},
      setAttribute() {},
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

export function enterSpan(_span, fn) {
  return fn();
}

export function restoreSnapshot() {}
