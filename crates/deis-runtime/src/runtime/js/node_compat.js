// Edge Canon's selected Node surface. This module is evaluated after all lazy
// extension arguments and application permissions are installed, but before
// any application module is loaded. `process.getBuiltinModule()` is
// synchronous, and deno_core's lazy loader is the synchronous module loader
// intended for exactly that API; selected modules are then preloaded before
// the privileged Deno bootstrap object is removed.

(() => {

const core = globalThis.Deno.core;
const ops = core.ops;
const defineProperty = Object.defineProperty;
const freeze = Object.freeze;
const objectCreate = Object.create;
const objectEntries = Object.entries;
const objectGetOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
const objectKeys = Object.keys;
const objectValues = Object.values;
const hasOwn = Object.prototype.hasOwnProperty;
const reflectApply = Reflect.apply;
const reflectDeleteProperty = Reflect.deleteProperty;
const regExpTest = RegExp.prototype.test;
const stringIncludes = String.prototype.includes;
const stringEndsWith = String.prototype.endsWith;
const stringSlice = String.prototype.slice;
const stringSplit = String.prototype.split;
const stringStartsWith = String.prototype.startsWith;
const arrayJoin = Array.prototype.join;
const encodeUrlComponent = encodeURIComponent;
const toString = String;
const PlatformURL = URL;
const NativePromise = Promise;
const NativeDOMException = DOMException;
const platformSetTimeout = globalThis.setTimeout;
const platformClearTimeout = globalThis.clearTimeout;
const platformDeno = globalThis.Deno;
const bufferModule = core.createLazyLoader("node:buffer")();

// A selected Node polyfill may consult Deno while its own module body runs.
// The application never receives this object: it exists on the global only
// for the synchronous extent of the internal lazy load. cwd is virtualized
// because the host working directory is explicitly outside the product
// contract.
const internalDeno = objectCreate(platformDeno);
defineProperty(internalDeno, "cwd", {
  value: () => "/",
  enumerable: false,
  writable: false,
  configurable: false,
});

function withInternalDeno(callback) {
  const previous = objectGetOwnPropertyDescriptor(globalThis, "Deno");
  defineProperty(globalThis, "Deno", {
    value: internalDeno,
    enumerable: false,
    writable: false,
    configurable: true,
  });
  try {
    return callback();
  } finally {
    if (previous !== undefined) {
      defineProperty(globalThis, "Deno", previous);
    } else if (!reflectDeleteProperty(globalThis, "Deno")) {
      throw new Error("EC_NODE_ARTIFACT_INCOMPLETE: internal Deno facade escaped");
    }
  }
}

let processEnvContext;
let processEnvGetStore;
let processEnvRun;

function activeEnv() {
  return processEnvGetStore === undefined ? undefined : processEnvGetStore();
}

const env = new Proxy(Object.create(null), {
  get(_target, key) {
    if (typeof key === "symbol") return undefined;
    const current = activeEnv();
    if (current !== undefined) return current[key];
    return ops.op_deis_node_env_get(key);
  },
  set(_target, key, value) {
    if (typeof key === "symbol") throw new TypeError("process.env keys must be strings");
    if (
      reflectApply(stringIncludes, key, ["="]) ||
      reflectApply(stringIncludes, key, ["\0"])
    ) {
      throw new TypeError("process.env keys may not contain '=' or NUL");
    }
    const current = activeEnv();
    if (current !== undefined) {
      current[key] = toString(value);
      return true;
    }
    return ops.op_deis_node_env_set(key, toString(value));
  },
  deleteProperty(_target, key) {
    if (typeof key === "symbol") return true;
    const current = activeEnv();
    if (current !== undefined) {
      delete current[key];
      return true;
    }
    ops.op_deis_node_env_delete(key);
    return true;
  },
  ownKeys() {
    const current = activeEnv();
    if (current !== undefined) return objectKeys(current);
    return ops.op_deis_node_env_keys();
  },
  has(_target, key) {
    if (typeof key === "symbol") return false;
    const current = activeEnv();
    return current !== undefined
      ? reflectApply(hasOwn, current, [key])
      : ops.op_deis_node_env_get(key) !== undefined;
  },
  getOwnPropertyDescriptor(_target, key) {
    if (typeof key === "symbol") return undefined;
    const current = activeEnv();
    const value = current !== undefined ? current[key] : ops.op_deis_node_env_get(key);
    return value === undefined
      ? undefined
      : { value, enumerable: true, writable: true, configurable: true };
  },
});

function setImmediate(callback, ...args) {
  if (typeof callback !== "function") {
    throw new TypeError('The "callback" argument must be of type function');
  }
  return platformSetTimeout(callback, 0, ...args);
}

function clearImmediate(handle) {
  platformClearTimeout(handle);
}

function nextTick(callback, ...args) {
  if (typeof callback !== "function") {
    throw new TypeError('The "callback" argument must be of type function');
  }
  if (!core.hasTickScheduled()) core.setHasTickScheduled(true);
  core.queueNextTick({
    callback,
    args: args.length === 0 ? undefined : args,
    snapshot: core.getAsyncContext(),
  });
}

function timerPromiseSetTimeout(delay = 1, value, options = undefined) {
  const signal = options && options.signal;
  if (signal && signal.aborted) {
    return NativePromise.reject(
      signal.reason ?? new NativeDOMException("The operation was aborted", "AbortError"),
    );
  }
  return new NativePromise((resolve, reject) => {
    let abort;
    const handle = platformSetTimeout(() => {
      if (signal) signal.removeEventListener("abort", abort);
      resolve(value);
    }, delay);
    if (signal) {
      abort = () => {
        platformClearTimeout(handle);
        reject(
          signal.reason ?? new NativeDOMException("The operation was aborted", "AbortError"),
        );
      };
      signal.addEventListener("abort", abort, { once: true });
    }
  });
}

function resolveWithVirtualRoot(value, win32, receiver, args) {
  if (!win32) return reflectApply(value, receiver, ["/", ...args]);

  let explicitDrive = false;
  for (const part of args) {
    if (
      typeof part === "string" &&
      reflectApply(regExpTest, /^[A-Za-z]:/, [part])
    ) {
      explicitDrive = true;
      break;
    }
  }
  const resolved = reflectApply(value, receiver, ["C:\\", ...args]);
  return !explicitDrive &&
      reflectApply(stringStartsWith, resolved, ["C:\\"])
    ? reflectApply(stringSlice, resolved, [2])
    : resolved;
}

function virtualResolve(value, win32) {
  return function (...args) {
    return resolveWithVirtualRoot(value, win32, this, args);
  };
}

function virtualNamespacedPath(value, resolve) {
  return function (path) {
    const absolute = resolveWithVirtualRoot(resolve, true, this, [path]);
    return reflectApply(value, this, [absolute]);
  };
}

function pathFlavor(value, win32) {
  const selected = objectCreate(null);
  for (const [name, member] of objectEntries(value)) {
    let protectedMember = member;
    if (typeof member === "function" && name === "resolve") {
      protectedMember = virtualResolve(member, win32);
    } else if (win32 && typeof member === "function" && name === "toNamespacedPath") {
      protectedMember = virtualNamespacedPath(member, value.resolve);
    }
    defineProperty(selected, name, {
      value: protectedMember,
      enumerable: true,
      writable: false,
      configurable: false,
    });
  }
  return freeze(selected);
}

function protectSelectedValue(canonical, name, value) {
  if (canonical === "node:path" && name === "resolve") {
    return virtualResolve(value, false);
  }
  if (canonical === "node:path" && (name === "posix" || name === "win32")) {
    return pathFlavor(value, name === "win32");
  }
  if (canonical === "node:url" && name === "pathToFileURL") {
    const pathModule = withInternalDeno(() => core.createLazyLoader("node:path")());
    return function (path, options) {
      if (options?.windows === true) {
        let absolute = resolveWithVirtualRoot(
          pathModule.win32.resolve,
          true,
          pathModule.win32,
          [path],
        );
        if (reflectApply(stringStartsWith, absolute, ["\\"])) {
          absolute = `C:${absolute}`;
        }
        return reflectApply(value, this, [absolute, options]);
      }

      let absolute = reflectApply(pathModule.posix.resolve, pathModule.posix, [
        "/",
        path,
      ]);
      if (
        reflectApply(stringEndsWith, path, ["/"]) &&
        !reflectApply(stringEndsWith, absolute, ["/"])
      ) {
        absolute += "/";
      }
      const parts = reflectApply(stringSplit, absolute, ["/"]);
      for (let index = 0; index < parts.length; index += 1) {
        parts[index] = encodeUrlComponent(parts[index]);
      }
      return new PlatformURL(`file://${reflectApply(arrayJoin, parts, ["/"])}`);
    };
  }
  return value;
}

function pick(canonical, module, names) {
  const selected = Object.create(null);
  for (const name of names) {
    if (!reflectApply(hasOwn, module, [name])) {
      throw new Error(`EC_NODE_ARTIFACT_INCOMPLETE: runtime module export is missing: ${name}`);
    }
    defineProperty(selected, name, {
      value: protectSelectedValue(canonical, name, module[name]),
      enumerable: true,
      writable: false,
      configurable: false,
    });
  }
  return freeze(selected);
}

const selectedExports = ops.op_deis_node_contract();
for (const exports of objectValues(selectedExports)) freeze(exports);
freeze(selectedExports);
const builtinCache = Object.create(null);
const timersModule = freeze({
  clearImmediate,
  clearInterval: globalThis.clearInterval,
  clearTimeout: globalThis.clearTimeout,
  setImmediate,
  setInterval: globalThis.setInterval,
  setTimeout: globalThis.setTimeout,
});
const timersPromisesModule = freeze({ setTimeout: timerPromiseSetTimeout });

const nodeVersion = ops.op_deis_node_version();
const processVersions = freeze({ node: nodeVersion });
let processFacade;

function getBuiltinModule(specifier) {
  if (typeof specifier !== "string") return undefined;
  const canonical = reflectApply(stringStartsWith, specifier, ["node:"])
    ? specifier
    : `node:${specifier}`;
  if (!reflectApply(hasOwn, selectedExports, [canonical])) return undefined;
  if (canonical === "node:process") return processFacade;
  if (canonical === "node:timers") return timersModule;
  if (canonical === "node:timers/promises") return timersPromisesModule;
  if (reflectApply(hasOwn, builtinCache, [canonical])) return builtinCache[canonical];
  const selected = pick(
    canonical,
    withInternalDeno(() => core.createLazyLoader(canonical)()),
    selectedExports[canonical],
  );
  defineProperty(builtinCache, canonical, {
    value: selected, enumerable: true, writable: false, configurable: false,
  });
  return selected;
}

processFacade = freeze({
  env,
  getBuiltinModule,
  nextTick,
  platform: "linux",
  version: `v${nodeVersion}`,
  versions: processVersions,
});

defineProperty(builtinCache, "node:buffer", {
  value: pick("node:buffer", bufferModule, selectedExports["node:buffer"]),
  enumerable: true,
  writable: false,
  configurable: false,
});

// All selected polyfills are initialized once while producing the shared
// startup snapshot. Every application isolate restores this cache rather than
// repeating the work, and no later lazy load needs the privileged Deno global.
for (const canonical of objectKeys(selectedExports)) {
  // Reading node:crypto's selected webcrypto export instantiates cppgc-backed
  // objects. Snapshot isolates intentionally have no CppHeap, so this one
  // remains lazy and is initialized in the restored application isolate.
  if (canonical !== "node:crypto") getBuiltinModule(canonical);
}

processEnvContext = new (getBuiltinModule("node:async_hooks").AsyncLocalStorage)();
processEnvGetStore = processEnvContext.getStore.bind(processEnvContext);
processEnvRun = processEnvContext.run.bind(processEnvContext);

function runWithProcessEnv(callback) {
  const current = objectCreate(null);
  for (const [key, value] of objectEntries(ops.op_deis_node_env_snapshot())) {
    current[key] = value;
  }
  return processEnvRun(current, callback);
}

// Captured and removed by runtime.js before an application module exists.
// Configurable is intentional: leaving an internal runner on the global
// surface would turn implementation structure into application API.
defineProperty(globalThis, "__deis_node_run_with_env", {
  value: runWithProcessEnv, enumerable: false, writable: false, configurable: true,
});

defineProperty(globalThis, "Buffer", {
  value: bufferModule.Buffer, enumerable: false, writable: false, configurable: false,
});
defineProperty(globalThis, "process", {
  value: processFacade, enumerable: false, writable: false, configurable: false,
});
defineProperty(globalThis, "setImmediate", {
  value: setImmediate, enumerable: false, writable: false, configurable: false,
});
defineProperty(globalThis, "clearImmediate", {
  value: clearImmediate, enumerable: false, writable: false, configurable: false,
});

})();
