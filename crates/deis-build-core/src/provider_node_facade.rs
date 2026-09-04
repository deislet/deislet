//! Provider-neutral Node façades for bundled vendor targets.
//!
//! Cloudflare, EdgeOne and Deno expose different host `process` objects and
//! may add Node exports as their runtimes evolve.  Applications must not see
//! those differences.  This Rolldown plugin routes every selected builtin
//! through a virtual module with the exact Edge Canon export set, while those
//! virtual modules alone may reach the provider's native implementation.

use std::borrow::Cow;

use rolldown::plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookUsage, Plugin, PluginContext,
};

pub(crate) const NODE_RUNTIME_SPECIFIER: &str = "edge-canon:node-runtime";
const FACADE_PREFIX: &str = "edge-canon-facade:";
const NATIVE_PREFIX: &str = "edge-canon-native:";
const VIRTUAL_PREFIX: &str = "\0edge-canon-node:";
const VIRTUAL_RUNTIME: &str = "\0edge-canon-node:runtime";

#[derive(Debug, Default)]
pub(crate) struct ProviderNodeFacadePlugin;

impl Plugin for ProviderNodeFacadePlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("edge-canon-provider-node-facade")
    }

    async fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        if args.specifier == NODE_RUNTIME_SPECIFIER {
            return Ok(Some(HookResolveIdOutput {
                id: VIRTUAL_RUNTIME.into(),
                ..Default::default()
            }));
        }

        if let Some(specifier) = args.specifier.strip_prefix(NATIVE_PREFIX) {
            let Some(canonical) = deis_node_contract::canonical_builtin(specifier) else {
                anyhow::bail!(
                    "internal Node facade requested an unknown native module {specifier:?}"
                );
            };
            return Ok(Some(HookResolveIdOutput {
                id: canonical.into(),
                external: Some(true.into()),
                ..Default::default()
            }));
        }

        let selected = args
            .specifier
            .strip_prefix(FACADE_PREFIX)
            .or(Some(args.specifier));
        if let Some(canonical) = selected.and_then(deis_node_contract::canonical_builtin) {
            return Ok(Some(HookResolveIdOutput {
                id: format!("{VIRTUAL_PREFIX}{canonical}").into(),
                ..Default::default()
            }));
        }

        Ok(None)
    }

    async fn load(&self, _ctx: &PluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        if args.id == VIRTUAL_RUNTIME {
            return Ok(Some(HookLoadOutput {
                code: runtime_source().into(),
                ..Default::default()
            }));
        }

        let Some(canonical) = args.id.strip_prefix(VIRTUAL_PREFIX) else {
            return Ok(None);
        };
        if deis_node_contract::canonical_builtin(canonical) != Some(canonical) {
            anyhow::bail!("invalid Edge Canon Node facade module id {canonical:?}");
        }
        Ok(Some(HookLoadOutput {
            code: builtin_source(canonical).into(),
            ..Default::default()
        }))
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load
    }
}

fn facade_specifier(canonical: &str) -> String {
    format!("{FACADE_PREFIX}{canonical}")
}

fn native_specifier(canonical: &str) -> String {
    format!("{NATIVE_PREFIX}{canonical}")
}

fn builtin_source(canonical: &str) -> String {
    match canonical {
        "node:async_hooks" => async_hooks_source(),
        "node:process" => process_source(),
        "node:path" => path_source(),
        "node:url" => url_source(),
        _ => passthrough_source(canonical),
    }
}

/// Expose the exact `AsyncLocalStorage` method intersection selected by Edge
/// Canon. Cloudflare deliberately omits `enterWith()` and `disable()`, so
/// those methods cannot be part of one portable application contract. The
/// provider's native engine still supplies the selected propagation behavior.
fn async_hooks_source() -> String {
    format!(
        r#"import {{ AsyncLocalStorage as NativeAsyncLocalStorage }} from {native};

const states = new WeakMap();
function state(instance) {{
  const value = states.get(instance);
  if (value === undefined) throw new TypeError("invalid AsyncLocalStorage receiver");
  return value;
}}

class AsyncLocalStorage {{
  constructor(options) {{
    states.set(this, new NativeAsyncLocalStorage(options));
  }}

  run(store, callback, ...args) {{
    if (typeof callback !== "function") throw new TypeError("AsyncLocalStorage callback must be a function");
    return state(this).run(store, callback, ...args);
  }}

  getStore() {{
    return state(this).getStore();
  }}

  exit(callback, ...args) {{
    if (typeof callback !== "function") throw new TypeError("AsyncLocalStorage callback must be a function");
    return state(this).run(undefined, callback, ...args);
  }}
}}

export {{ AsyncLocalStorage }};
"#,
        native = serde_json::to_string(&native_specifier("node:async_hooks"))
            .expect("specifier is JSON text"),
    )
}

fn passthrough_source(canonical: &str) -> String {
    let exports = deis_node_contract::builtin_exports(canonical)
        .expect("every selected builtin has an export list");
    let imports = exports
        .iter()
        .enumerate()
        .map(|(index, name)| format!("{name} as __ec_{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let reexports = exports
        .iter()
        .enumerate()
        .map(|(index, name)| format!("__ec_{index} as {name}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "import {{ {imports} }} from {};\nexport {{ {reexports} }};\n",
        serde_json::to_string(&native_specifier(canonical)).expect("specifier is JSON text")
    )
}

/// `node:path` follows the host platform and resolves relative paths from the
/// host cwd.  Edge Canon fixes the visible platform to Linux and gives code a
/// virtual `/` root, so the top-level flavour is always POSIX and both nested
/// flavours receive a deterministic root.
fn path_source() -> String {
    format!(
        r#"import {{ posix as nativePosix, win32 as nativeWin32 }} from {native};

const hasDrive = (parts) => parts.some((part) => typeof part === "string" && /^[A-Za-z]:/.test(part));
const posixResolve = (...parts) => nativePosix.resolve("/", ...parts);
const win32Resolve = (...parts) => {{
  const explicitDrive = hasDrive(parts);
  const resolved = nativeWin32.resolve("C:\\", ...parts);
  return !explicitDrive && resolved.startsWith("C:\\") ? resolved.slice(2) : resolved;
}};
const posix = Object.freeze(Object.assign(Object.create(null), nativePosix, {{ resolve: posixResolve }}));
const win32 = Object.freeze(Object.assign(Object.create(null), nativeWin32, {{ resolve: win32Resolve }}));

const basename = nativePosix.basename;
const delimiter = nativePosix.delimiter;
const dirname = nativePosix.dirname;
const extname = nativePosix.extname;
const format = nativePosix.format;
const isAbsolute = nativePosix.isAbsolute;
const join = nativePosix.join;
const normalize = nativePosix.normalize;
const parse = nativePosix.parse;
const relative = nativePosix.relative;
const resolve = posixResolve;
const sep = nativePosix.sep;
const toNamespacedPath = nativePosix.toNamespacedPath;

export {{ basename, delimiter, dirname, extname, format, isAbsolute, join, normalize, parse, posix, relative, resolve, sep, toNamespacedPath, win32 }};
"#,
        native =
            serde_json::to_string(&native_specifier("node:path")).expect("specifier is JSON text")
    )
}

/// File URL conversion is another route by which a host cwd and path flavour
/// can leak.  Feed native Node only an absolute path under the standard's
/// virtual roots, and make POSIX the default even when a bundle is exercised
/// on a non-Linux development host.
fn url_source() -> String {
    format!(
        r#"import {{ URL, URLSearchParams, domainToASCII, domainToUnicode, fileURLToPath as nativeFileURLToPath, pathToFileURL as nativePathToFileURL, urlToHttpOptions }} from {native};
import {{ posix, win32 }} from {path};

const pathToFileURL = (value, options) => {{
  const windows = options?.windows === true;
  const flavor = windows ? win32 : posix;
  const root = windows ? "C:\\" : "/";
  let absolute = flavor.resolve(root, value);
  if (value.endsWith(flavor.sep) && !absolute.endsWith(flavor.sep)) absolute += flavor.sep;
  if (windows) {{
    const parts = absolute.split("\\");
    if (absolute.startsWith("\\\\")) {{
      const host = parts[2];
      return new URL(`file://${{host}}/${{parts.slice(3).map(encodeURIComponent).join("/")}}`);
    }}
    const drive = parts.shift();
    return new URL(`file:///${{drive}}/${{parts.map(encodeURIComponent).join("/")}}`);
  }}
  return nativePathToFileURL(absolute, {{ windows }});
}};
const fileURLToPath = (value, options) => {{
  if (options?.windows !== true) return nativeFileURLToPath(value, {{ windows: false }});
  const url = value instanceof URL ? value : new URL(value);
  if (url.protocol !== "file:") return nativeFileURLToPath(value, {{ windows: true }});
  if (/%2f|%5c/i.test(url.pathname)) return nativeFileURLToPath(value, {{ windows: true }});
  const pathname = decodeURIComponent(url.pathname);
  if (url.hostname && url.hostname !== "localhost") {{
    return `\\\\${{url.hostname}}${{pathname.replaceAll("/", "\\")}}`;
  }}
  if (!/^\/[A-Za-z]:/.test(pathname)) return nativeFileURLToPath(value, {{ windows: true }});
  return pathname.slice(1).replaceAll("/", "\\");
}};

export {{ URL, URLSearchParams, domainToASCII, domainToUnicode, fileURLToPath, pathToFileURL, urlToHttpOptions }};
"#,
        native =
            serde_json::to_string(&native_specifier("node:url")).expect("specifier is JSON text"),
        path =
            serde_json::to_string(&facade_specifier("node:path")).expect("specifier is JSON text"),
    )
}

fn process_source() -> String {
    let mut module_imports = String::new();
    let mut module_entries = String::new();
    for (index, canonical) in deis_node_contract::NODE_BUILTIN_MODULES
        .iter()
        .filter(|canonical| **canonical != "node:process")
        .enumerate()
    {
        module_imports.push_str(&format!(
            "import * as __ec_module_{index} from {};\n",
            serde_json::to_string(&facade_specifier(canonical)).expect("specifier is JSON text")
        ));
        module_entries.push_str(&format!(
            "Object.defineProperty(moduleTable, {}, {{ value: __ec_module_{index}, enumerable: true }});\n",
            serde_json::to_string(canonical).expect("module name is JSON text")
        ));
    }

    format!(
        r#"import {{ AsyncLocalStorage as NativeAsyncLocalStorage }} from {async_hooks};
import {{ nextTick as nativeNextTick }} from {native_process};
{module_imports}
const own = Function.call.bind(Object.prototype.hasOwnProperty);
const storage = new NativeAsyncLocalStorage();
const bootstrapEnvironment = Object.create(null);

function selectedEnvironment() {{
  return storage.getStore() ?? bootstrapEnvironment;
}}

function validKey(key) {{
  if (typeof key !== "string") throw new TypeError("process.env keys must be strings");
  if (key.includes("=") || key.includes("\0")) throw new TypeError("process.env keys may not contain '=' or NUL");
  return key;
}}

const env = new Proxy(Object.create(null), {{
  get(_target, key) {{
    return typeof key === "string" ? selectedEnvironment()[key] : undefined;
  }},
  set(_target, key, value) {{
    selectedEnvironment()[validKey(key)] = String(value);
    return true;
  }},
  defineProperty(_target, key, descriptor) {{
    if (!("value" in descriptor) || "get" in descriptor || "set" in descriptor) {{
      throw new TypeError("process.env properties must have values");
    }}
    selectedEnvironment()[validKey(key)] = String(descriptor.value);
    return true;
  }},
  deleteProperty(_target, key) {{
    if (typeof key === "string") delete selectedEnvironment()[key];
    return true;
  }},
  ownKeys() {{ return Object.keys(selectedEnvironment()); }},
  has(_target, key) {{ return typeof key === "string" && own(selectedEnvironment(), key); }},
  getOwnPropertyDescriptor(_target, key) {{
    if (typeof key !== "string" || !own(selectedEnvironment(), key)) return undefined;
    return {{ value: selectedEnvironment()[key], enumerable: true, writable: true, configurable: true }};
  }},
}});

const moduleTable = Object.create(null);
{module_entries}
let processFacade;

function getBuiltinModule(name) {{
  if (typeof name !== "string") return undefined;
  const canonical = name.startsWith("node:") ? name : `node:${{name}}`;
  if (canonical === "node:process") return processFacade;
  return own(moduleTable, canonical) ? moduleTable[canonical] : undefined;
}}

const versions = Object.freeze({{ node: {version:?} }});
processFacade = Object.freeze({{
  env,
  getBuiltinModule,
  nextTick: nativeNextTick,
  platform: "linux",
  version: {version_with_v:?},
  versions,
}});
Object.defineProperty(moduleTable, "node:process", {{ value: processFacade, enumerable: true }});
Object.freeze(moduleTable);

function snapshotEnvironment(input) {{
  const snapshot = Object.create(null);
  if (input === null || (typeof input !== "object" && typeof input !== "function")) return snapshot;
  for (const [key, value] of Object.entries(input)) {{
    if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") {{
      snapshot[key] = String(value);
    }}
  }}
  return snapshot;
}}

function runWithEdgeCanonNodeEnv(environment, callback) {{
  if (typeof callback !== "function") throw new TypeError("Edge Canon invocation callback must be a function");
  return storage.run(snapshotEnvironment(environment), callback);
}}

const nextTick = nativeNextTick;
const platform = "linux";
const version = {version_with_v:?};
export {{ env, getBuiltinModule, nextTick, platform, version, versions, processFacade as process, runWithEdgeCanonNodeEnv }};
"#,
        async_hooks = serde_json::to_string(&facade_specifier("node:async_hooks"))
            .expect("specifier is JSON text"),
        native_process = serde_json::to_string(&native_specifier("node:process"))
            .expect("specifier is JSON text"),
        version = deis_node_contract::NODE_SEMANTIC_BASELINE,
        version_with_v = format!("v{}", deis_node_contract::NODE_SEMANTIC_BASELINE),
    )
}

fn runtime_source() -> String {
    format!(
        r#"import {{ process, runWithEdgeCanonNodeEnv }} from {process_module};
import {{ Buffer }} from {buffer_module};
import {{ setImmediate, clearImmediate }} from {timers_module};

function installGlobal(name, value) {{
  const current = Object.getOwnPropertyDescriptor(globalThis, name);
  if (current !== undefined && current.configurable === false) {{
    if (current.value === value) return;
    throw new Error(`EC_NODE_ARTIFACT_INCOMPLETE: provider global ${{name}} cannot be normalized`);
  }}
  Object.defineProperty(globalThis, name, {{
    value,
    enumerable: false,
    writable: false,
    configurable: false,
  }});
}}

installGlobal("process", process);
installGlobal("Buffer", Buffer);
installGlobal("setImmediate", setImmediate);
installGlobal("clearImmediate", clearImmediate);

export {{ process, Buffer, setImmediate, clearImmediate, runWithEdgeCanonNodeEnv }};
"#,
        process_module = serde_json::to_string(&facade_specifier("node:process"))
            .expect("specifier is JSON text"),
        buffer_module = serde_json::to_string(&facade_specifier("node:buffer"))
            .expect("specifier is JSON text"),
        timers_module = serde_json::to_string(&facade_specifier("node:timers"))
            .expect("specifier is JSON text"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_virtual_builtin_uses_only_selected_native_exports() {
        for canonical in deis_node_contract::NODE_BUILTIN_MODULES {
            let source = builtin_source(canonical);
            assert!(!source.contains("import * from"), "{canonical}: {source}");
            assert!(
                source.contains(NATIVE_PREFIX) || canonical == "node:process",
                "{canonical}: {source}"
            );
        }
    }

    #[test]
    fn process_identity_is_generated_from_the_shared_contract() {
        let source = process_source();
        assert!(source.contains("version: \"v24.20.0\""), "{source}");
        assert!(source.contains("node: \"24.20.0\""), "{source}");
        for canonical in deis_node_contract::NODE_BUILTIN_MODULES {
            assert!(source.contains(canonical), "missing {canonical}: {source}");
        }
    }

    #[test]
    fn async_local_storage_exposes_only_the_portable_method_intersection() {
        let source = async_hooks_source();
        for method in ["run(store", "getStore()", "exit(callback"] {
            assert!(source.contains(method), "missing {method}: {source}");
        }
        assert!(!source.contains("  enterWith("), "{source}");
        assert!(!source.contains("  disable()"), "{source}");
    }
}
