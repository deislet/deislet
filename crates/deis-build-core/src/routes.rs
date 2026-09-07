//! Turning the function file tree into route patterns.
//!
//! edge-canon says the file tree *is* the route table: a handler's path under
//! the function root decides which requests reach it. Every target has to
//! derive the same pattern from the same file, otherwise an app answers on one
//! platform and 404s on another, so the derivation lives here rather than once
//! per adapter.
//!
//! ```text
//! functions/index.ts            ->  /
//! functions/users/[id].ts       ->  /users/:id
//! functions/users/[id]/index.ts ->  /users/:id
//! functions/api/[[catch]].ts    ->  /api/*
//! ```
//!
//! `:name` and `*` are also exactly how a pattern is spelled in
//! `deis_bundle::Route::pattern` and in `RouteRule.pattern` on the wire, so a
//! pattern derived here can go straight into a bundle manifest.
//!
//! Which is why a file name that *already* reads as that syntax has to be
//! escaped rather than passed through: `[name]` and `[[name]]` are the file
//! tree's spelling of a placeholder and a catch-all, so a raw `:` or `*` in a
//! name is a literal, and a literal has to be written into the pattern in a
//! form that can only mean itself. See `escape_literal`.

/// Where the compiler stages a handler, as the generated entry module reaches
/// it.
///
/// Every handler under the function root gets a route proxy in `src/` of the
/// build directory and the generated entry module is written into
/// `_deis_internal/`, so the entry imports it as
/// `../src/<path under the function root>`. The proxy re-exports the source
/// from the project-shaped module mirror; keeping this route-facing prefix
/// stable lets route planning stay independent of source staging.
/// The constant lives here rather than in either half because both halves read
/// it: the compiler builds the specifier, and the deislet route planner takes
/// it apart again to find out whether a file sits *at* the function root.
pub const STAGED_HANDLER_PREFIX: &str = "../src/";

/// The invocation surface selected by a handler file's position and name.
/// Queue and scheduled handlers are distinct standard event families; their
/// context must never be validated as an EC-WEB request context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerKind {
    Http,
    Queue,
    Scheduled,
    Reserved,
}

pub const QUEUE_HANDLER_STEM: &str = "_queue";
pub const SCHEDULED_HANDLER_STEM: &str = "_scheduled";

/// Classify a handler path relative to the configured function root.
///
/// Only a root file is special. `admin/_queue.ts` remains the ordinary HTTP
/// route `/admin/_queue`; repeatedly stripping a staging prefix would silently
/// turn such a route into the application's queue consumer.
pub fn handler_kind(relative_path: &str) -> HandlerKind {
    let relative = staged_handler_path(relative_path).replace('\\', "/");
    if relative.contains('/') {
        return HandlerKind::Http;
    }
    let stem = strip_handler_extension(&relative);
    match stem {
        QUEUE_HANDLER_STEM => HandlerKind::Queue,
        SCHEDULED_HANDLER_STEM => HandlerKind::Scheduled,
        value if value.starts_with('_') => HandlerKind::Reserved,
        _ => HandlerKind::Http,
    }
}

/// Encode an application-controlled value as a JavaScript string literal.
///
/// Route names and import specifiers come from the project file tree. They
/// must never be interpolated between quotes by a provider adapter: a legal
/// apostrophe, line separator or backslash in a file name would otherwise
/// change the generated module. JSON strings are valid JavaScript string
/// literals and give every adapter one escaping rule.
pub fn javascript_string(value: &str) -> String {
    serde_json::to_string(value).expect("a Rust string always serialises as JSON")
}

/// Provider-neutral route matching code embedded by JavaScript/TypeScript
/// adapters.
///
/// Keep this code untyped: it is valid in both `.js` and `.ts` outputs. A
/// provider adapter may choose its native entrypoint shape, but it may not
/// choose different path semantics.
pub const JAVASCRIPT_ROUTE_MATCHER: &str = r#"export function matchRoute(pathname, route) {
  const routeParts = route.split('/').filter(Boolean);
  const pathParts = pathname.split('/').filter(Boolean);
  for (let i = 0; i < routeParts.length; i++) {
    const part = routeParts[i];
    if (part === '*') return i === routeParts.length - 1;
    if (i >= pathParts.length) return false;
    if (part.startsWith(':')) continue;
    if (decodeRouteSegment(part) !== decodeRouteSegment(pathParts[i])) return false;
  }
  return routeParts.length === pathParts.length;
}"#;

/// Provider-neutral parameter extraction code embedded in generated context
/// builders. `wildcardName` preserves the name from `[[catch]]` while `*`
/// remains the stable cross-artifact spelling of a catch-all.
pub const JAVASCRIPT_ROUTE_PARAM_EXTRACTOR: &str = r#"export function extractParams(pathname, route, wildcardName) {
  const params = {};
  const routeParts = route.split('/').filter(Boolean);
  const pathParts = pathname.split('/').filter(Boolean);

  routeParts.forEach((part, i) => {
    if (part === '*') {
      const rest = pathParts.slice(i).map(decodeRouteSegment).join('/');
      params['*'] = rest;
      if (wildcardName !== null) params[wildcardName] = rest;
    } else if (part.startsWith(':')) {
      params[part.slice(1)] = decodeRouteSegment(pathParts[i] || '');
    }
  });

  return params;
}"#;

const JAVASCRIPT_ROUTE_DECODER: &str = r#"function decodeRouteSegment(segment) {
  try { return decodeURIComponent(segment); }
  catch (_) { return segment; }
}"#;

/// The common Edge Canon context, handler and response-lifecycle boundary used
/// by provider entry modules.
///
/// It deliberately records only stable codes and bounded task numbers.
/// Application error messages can contain secrets and must not become response
/// data (or an adapter-authored log field). A provider evidence collector can
/// associate these structured records with an invocation without parsing an
/// error page.
pub const JAVASCRIPT_HANDLER_BOUNDARY: &str = r#"const EdgeCanonObject = globalThis.Object;
const EdgeCanonPromise = globalThis.Promise;
const EdgeCanonReadableStream = globalThis.ReadableStream;
const EdgeCanonResponse = globalThis.Response;
const EdgeCanonTypeError = globalThis.TypeError;
const edgeCanonObjectAssign = EdgeCanonObject.assign.bind(EdgeCanonObject);
const edgeCanonObjectCreate = EdgeCanonObject.create.bind(EdgeCanonObject);
const edgeCanonObjectDefineProperty = EdgeCanonObject.defineProperty.bind(EdgeCanonObject);
const edgeCanonObjectFreeze = EdgeCanonObject.freeze.bind(EdgeCanonObject);
const edgeCanonObjectGetPrototypeOf = EdgeCanonObject.getPrototypeOf.bind(EdgeCanonObject);
const edgeCanonObjectHasOwn = EdgeCanonObject.hasOwn.bind(EdgeCanonObject);
const edgeCanonObjectIsFrozen = EdgeCanonObject.isFrozen.bind(EdgeCanonObject);
const edgeCanonObjectKeys = EdgeCanonObject.keys.bind(EdgeCanonObject);
const edgeCanonArrayIsArray = globalThis.Array.isArray.bind(globalThis.Array);
const edgeCanonNumberIsFinite = globalThis.Number.isFinite.bind(globalThis.Number);
const edgeCanonJsonParse = globalThis.JSON.parse.bind(globalThis.JSON);
const EdgeCanonWeakSet = globalThis.WeakSet;
const edgeCanonWeakSetAdd = globalThis.Function.prototype.call.bind(EdgeCanonWeakSet.prototype.add);
const edgeCanonWeakSetDelete = globalThis.Function.prototype.call.bind(EdgeCanonWeakSet.prototype.delete);
const edgeCanonWeakSetHas = globalThis.Function.prototype.call.bind(EdgeCanonWeakSet.prototype.has);
const edgeCanonPromiseCatch = globalThis.Function.prototype.call.bind(EdgeCanonPromise.prototype.catch);
const edgeCanonPromiseResolve = EdgeCanonPromise.resolve.bind(EdgeCanonPromise);
const edgeCanonConsoleError = globalThis.console && typeof globalThis.console.error === 'function'
  ? globalThis.console.error.bind(globalThis.console)
  : () => {};

function edgeCanonRecordFailure(failureCode, fields = undefined) {
  const record = fields === undefined
    ? { failureCode }
    : edgeCanonObjectAssign({ failureCode }, fields);
  edgeCanonConsoleError('Edge Canon invocation failure', record);
}

function edgeCanonTypeError(failureCode, message) {
  const error = new EdgeCanonTypeError(message);
  edgeCanonObjectDefineProperty(error, 'code', {
    value: failureCode,
    enumerable: false,
    configurable: false,
    writable: false
  });
  return error;
}

function copiedRecord(value) {
  return edgeCanonObjectFreeze(edgeCanonObjectAssign({}, value || {}));
}

function edgeCanonEnvironmentError(code, message) {
  throw edgeCanonTypeError(code, message);
}

function copiedEnvironmentJson(value, seen) {
  if (value === null || typeof value === 'boolean' || typeof value === 'string') return value;
  if (typeof value === 'number') {
    if (!edgeCanonNumberIsFinite(value)) {
      edgeCanonEnvironmentError('EC_ENV_VALUE_INVALID', 'environment JSON contains a non-finite number');
    }
    return value;
  }
  if (typeof value !== 'object' || edgeCanonWeakSetHas(seen, value)) {
    edgeCanonEnvironmentError('EC_ENV_VALUE_INVALID', 'environment JSON is not a finite JSON value');
  }
  edgeCanonWeakSetAdd(seen, value);
  let copy;
  if (edgeCanonArrayIsArray(value)) {
    copy = [];
    for (let index = 0; index < value.length; index += 1) {
      copy[index] = copiedEnvironmentJson(value[index], seen);
    }
  } else {
    const prototype = edgeCanonObjectGetPrototypeOf(value);
    if (prototype !== EdgeCanonObject.prototype && prototype !== null) {
      edgeCanonEnvironmentError('EC_ENV_VALUE_INVALID', 'environment JSON contains a non-JSON object');
    }
    copy = edgeCanonObjectCreate(null);
    const keys = edgeCanonObjectKeys(value);
    for (let index = 0; index < keys.length; index += 1) {
      const key = keys[index];
      copy[key] = copiedEnvironmentJson(value[key], seen);
    }
  }
  edgeCanonWeakSetDelete(seen, value);
  return edgeCanonObjectFreeze(copy);
}

/**
 * Project a provider binding container onto the exact EC-ENV declaration.
 * Extra provider/account/host fields are never copied. A new null-prototype,
 * recursively frozen value graph is produced on every call.
 */
export function materializeEdgeCanonEnvironment(providerEnvironment, declarations) {
  const source = providerEnvironment && typeof providerEnvironment === 'object'
    ? providerEnvironment
    : edgeCanonObjectCreate(null);
  const environment = edgeCanonObjectCreate(null);
  const declared = declarations || [];
  for (let index = 0; index < declared.length; index += 1) {
    const declaration = declared[index];
    const present = edgeCanonObjectHasOwn(source, declaration.name);
    if (!present) {
      if (declaration.required) {
        edgeCanonEnvironmentError('EC_ENV_REQUIRED_MISSING', 'required environment binding is unavailable');
      }
      continue;
    }
    let value = source[declaration.name];
    if (declaration.valueType === 'string') {
      if (typeof value !== 'string') {
        edgeCanonEnvironmentError('EC_ENV_VALUE_INVALID', 'string environment binding has a non-string value');
      }
    } else if (declaration.kind === 'config' && declaration.valueType === 'json') {
      if (typeof value === 'string') {
        try {
          value = edgeCanonJsonParse(value);
        } catch (_) {
          edgeCanonEnvironmentError('EC_ENV_VALUE_INVALID', 'JSON environment binding does not parse');
        }
      }
      value = copiedEnvironmentJson(value, new EdgeCanonWeakSet());
    } else {
      edgeCanonEnvironmentError('EC_ENV_DECLARATION_INVALID', 'environment declaration kind or value type is unsupported');
    }
    environment[declaration.name] = value;
  }
  return edgeCanonObjectFreeze(environment);
}

/** Writable Node compatibility projection: config/string only. */
export function edgeCanonProcessEnvironment(environment, declarations) {
  const projection = edgeCanonObjectCreate(null);
  const declared = declarations || [];
  for (let index = 0; index < declared.length; index += 1) {
    const declaration = declared[index];
    if (declaration.kind === 'config' && declaration.valueType === 'string'
        && edgeCanonObjectHasOwn(environment, declaration.name)) {
      projection[declaration.name] = environment[declaration.name];
    }
  }
  return projection;
}

export function createEdgeCanonContext(request, environment, params, nativeWaitUntil) {
  if (typeof nativeWaitUntil !== 'function') {
    throw edgeCanonTypeError(
      'EC_PROVIDER_CONTEXT_INVALID',
      'the provider context has no waitUntil implementation'
    );
  }

  let closed = false;
  let registeredBackgroundTaskCount = 0;
  const contextEnvironment = environment
    && edgeCanonObjectGetPrototypeOf(environment) === null
    && edgeCanonObjectIsFrozen(environment)
      ? environment
      : edgeCanonObjectFreeze(edgeCanonObjectAssign(edgeCanonObjectCreate(null), environment || {}));
  const context = {
    request,
    env: contextEnvironment,
    params: copiedRecord(params),
    waitUntil(promise) {
      if (closed) {
        edgeCanonRecordFailure('EC_WAIT_UNTIL_CLOSED');
        throw edgeCanonTypeError(
          'EC_WAIT_UNTIL_CLOSED',
          'waitUntil cannot register work after the response lifecycle closed'
        );
      }

      let then;
      try {
        then = promise == null ? undefined : promise.then;
      } catch (_) {
        then = undefined;
      }
      if (typeof then !== 'function') {
        edgeCanonRecordFailure('EC_WAIT_UNTIL_PROMISE_REQUIRED');
        throw edgeCanonTypeError(
          'EC_WAIT_UNTIL_PROMISE_REQUIRED',
          'waitUntil requires a Promise or thenable'
        );
      }

      registeredBackgroundTaskCount += 1;
      const taskNumber = registeredBackgroundTaskCount;
      const tracked = edgeCanonPromiseCatch(edgeCanonPromiseResolve(promise), () => {
        edgeCanonRecordFailure('EC_BACKGROUND_REJECTED', { taskNumber });
      });
      nativeWaitUntil(tracked);
    }
  };

  return {
    context,
    close() {
      closed = true;
    },
    failure(failureCode) {
      edgeCanonRecordFailure(failureCode);
    }
  };
}

// Whether the provider has already settled this response's body as whole bytes.
//
// The lifecycle has to close when the response is finished, and for a body the
// application is still producing there is no way to learn that moment except by
// reading the body to its end, which is what the wrapper below exists to do. A
// body that is already whole has no such moment: it is finished the instant the
// handler hands it over, so re-reading it through a stream of ours buys nothing
// and costs a great deal. It costs the framing: a response the provider would
// have sent with a Content-Length leaves as a chunked one instead, for every
// answer of every application built here, which is visible to every client,
// disables length-based proxies and progress reporting, and erases the one
// difference an operator has between an answer that was ready and an answer
// that is still being produced.
//
// So a provider that can tell the two apart is asked, and only its own word is
// taken: a missing hook, a hook that throws, or any answer other than exactly
// `true` leaves the response on the wrapper's road. That road is correct for
// every body and merely more expensive for some, which is the right way round
// for a fallback.
function edgeCanonProviderSettledBody(responseBodyIsWhole, response) {
  if (typeof responseBodyIsWhole !== 'function') return false;
  try {
    return responseBodyIsWhole(response) === true;
  } catch (_) {
    return false;
  }
}

function edgeCanonLifecycleResponse(response, lifecycle, responseBodyIsWhole) {
  if (response.body === null) {
    lifecycle.close();
    return response;
  }
  if (edgeCanonProviderSettledBody(responseBodyIsWhole, response)) {
    // Nothing is left to observe, so the response goes to the provider exactly
    // as the handler built it and the lifecycle closes here. A `waitUntil`
    // captured out of this invocation and called afterwards therefore still
    // fails with EC_WAIT_UNTIL_CLOSED, which is the rule the wrapper enforces
    // for the bodies that do have a later ending.
    lifecycle.close();
    return response;
  }
  if (typeof EdgeCanonReadableStream !== 'function') {
    lifecycle.close();
    throw edgeCanonTypeError(
      'EC_STREAM_RUNTIME_INCOMPATIBLE',
      'the provider runtime has no ReadableStream implementation'
    );
  }

  let reader;
  try {
    reader = response.body.getReader();
  } catch (_) {
    lifecycle.close();
    throw edgeCanonTypeError(
      'EC_HANDLER_RESULT_INVALID',
      'the handler response body cannot be consumed'
    );
  }

  const body = new EdgeCanonReadableStream({
    async pull(controller) {
      try {
        const result = await reader.read();
        if (result.done) {
          lifecycle.close();
          controller.close();
        } else {
          controller.enqueue(result.value);
        }
      } catch (error) {
        lifecycle.close();
        controller.error(error);
      }
    },
    async cancel(reason) {
      try {
        await reader.cancel(reason);
      } finally {
        lifecycle.close();
      }
    }
  });

  return new EdgeCanonResponse(body, {
    status: response.status,
    statusText: response.statusText,
    headers: response.headers
  });
}

export async function invokeEdgeCanonHandler(lifecycle, invoke, responseBodyIsWhole) {
  let result;
  try {
    result = await invoke();
  } catch (_) {
    lifecycle.failure('EC_HANDLER_THROWN');
    result = edgeCanonFailureResponse();
  }
  if (!(result instanceof EdgeCanonResponse)) {
    lifecycle.failure('EC_HANDLER_RESULT_INVALID');
    result = edgeCanonFailureResponse();
  }
  try {
    return edgeCanonLifecycleResponse(result, lifecycle, responseBodyIsWhole);
  } catch (error) {
    const failureCode = error && error.code === 'EC_STREAM_RUNTIME_INCOMPATIBLE'
      ? 'EC_STREAM_RUNTIME_INCOMPATIBLE'
      : 'EC_HANDLER_RESULT_INVALID';
    lifecycle.failure(failureCode);
    lifecycle.close();
    return edgeCanonFailureResponse();
  }
}

export function edgeCanonFailureResponse(failureCode = undefined) {
  if (failureCode !== undefined) edgeCanonRecordFailure(failureCode);
  return new EdgeCanonResponse('Internal Server Error\n', {
    status: 500,
    headers: {
      'content-type': 'text/plain; charset=utf-8',
      'cache-control': 'no-store'
    }
  });
}"#;

/// One module containing the application-independent JavaScript semantics all
/// provider adapters import. Deno's unbundled Web output also executes the
/// runtime isolation prelude from this first dependency; bundled targets get
/// the same prelude from the bundler banner and avoid carrying it twice.
pub fn javascript_runtime_module(include_portable_boundary: bool) -> String {
    let mut module = String::new();
    if include_portable_boundary {
        module.push_str(crate::portable_runtime_prelude());
        module.push('\n');
    }
    module.push_str(JAVASCRIPT_HANDLER_BOUNDARY);
    module.push_str("\n\n");
    module.push_str(JAVASCRIPT_ROUTE_DECODER);
    module.push_str("\n\n");
    module.push_str(JAVASCRIPT_ROUTE_MATCHER);
    module.push_str("\n\n");
    module.push_str(JAVASCRIPT_ROUTE_PARAM_EXTRACTOR);
    module.push('\n');
    module
}

/// The handler's path under the function root, recovered from the specifier the
/// compiler built with [`STAGED_HANDLER_PREFIX`].
///
/// `strip_prefix`, once, and deliberately not `trim_start_matches`: that strips
/// the prefix as many times as it appears, so `../src/src/_queue.ts` - an
/// ordinary file in an ordinary directory called `src` - came back as
/// `_queue.ts` and was read as a file at the function root. The platform
/// handlers are named by position, so that quietly turned an author's page into
/// the whole application's queue consumer and dropped its route.
///
/// A specifier that does not carry the prefix is returned unchanged: it did not
/// come from the staging convention, so there is nothing to take off it.
pub fn staged_handler_path(import_path: &str) -> &str {
    import_path
        .strip_prefix(STAGED_HANDLER_PREFIX)
        .unwrap_or(import_path)
}

/// Extensions a handler file may carry. Anything else keeps its suffix, since
/// stripping an unknown one would silently route `report.tar.gz` as `report`.
const HANDLER_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"];

/// Derive the route pattern for a handler file.
///
/// `relative_path` is the handler's path under the function root, with either
/// separator (a Windows build produces `users\[id].ts`).
pub fn route_pattern(relative_path: &str) -> String {
    let normalised = relative_path.replace('\\', "/");
    let without_ext = strip_handler_extension(&normalised);

    let mut segments: Vec<String> = without_ext
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(pattern_segment)
        .collect();

    // `index` names the directory it sits in, not a child of it.
    if segments.last().map(|s| s == "index").unwrap_or(false) {
        segments.pop();
    }

    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

/// Whether a pattern can ever match a request.
///
/// A `*` stands for "the rest of the path", so it only means anything as the
/// final segment. `functions/api/[[catch]]/edit.ts` reads like a route but
/// matches nothing, and the only symptom would be a handler that never runs -
/// worth a build warning rather than silence.
pub fn wildcard_is_reachable(pattern: &str) -> bool {
    let segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    match segments.iter().position(|s| *s == "*") {
        Some(index) => index + 1 == segments.len(),
        None => true,
    }
}

/// The placeholder names a pattern binds, in order.
///
/// A `*` binds under the name `*` as well as under the name it was written
/// with in the file tree, which this function cannot recover - callers that
/// need the written name work from the file path instead.
pub fn placeholder_names(pattern: &str) -> Vec<String> {
    pattern
        .split('/')
        .filter(|s| !s.is_empty())
        .filter_map(|segment| {
            segment
                .strip_prefix(':')
                .filter(|name| !name.is_empty())
                .map(|name| name.to_string())
                .or_else(|| (segment == "*").then(|| "*".to_string()))
        })
        .collect()
}

/// The name a `[[catch]]` segment was written with, so a handler can read the
/// captured tail as `context.params.catch` and not only as `params['*']`.
pub fn wildcard_name(relative_path: &str) -> Option<String> {
    let normalised = relative_path.replace('\\', "/");
    strip_handler_extension(&normalised)
        .split('/')
        .filter_map(|segment| {
            segment
                .strip_prefix("[[")
                .and_then(|s| s.strip_suffix("]]"))
                .filter(|name| !name.is_empty())
                .map(|name| name.to_string())
        })
        .next_back()
}

fn strip_handler_extension(path: &str) -> &str {
    for ext in HANDLER_EXTENSIONS {
        if let Some(stripped) = path.strip_suffix(ext) {
            return stripped;
        }
    }
    path
}

/// The segments of `relative_path` whose own text had to be escaped, as
/// `(what the file is called, what it became in the pattern)`.
///
/// Empty for every ordinary file name, which is what makes it usable as a
/// build warning: a name only appears here when the author wrote pattern
/// syntax the file-tree convention does not use, and would otherwise have got
/// a route that means something other than the file's own name.
pub fn escaped_segments(relative_path: &str) -> Vec<(String, String)> {
    let normalised = relative_path.replace('\\', "/");
    strip_handler_extension(&normalised)
        .split('/')
        .filter(|segment| !segment.is_empty())
        .filter_map(|segment| {
            let pattern = pattern_segment(segment);
            (pattern != segment && !is_placeholder(segment)).then(|| (segment.to_string(), pattern))
        })
        .collect()
}

/// Whether this segment is the file tree's own spelling of a placeholder,
/// `[name]` or `[[name]]`, rather than a literal that had to be escaped.
fn is_placeholder(segment: &str) -> bool {
    segment
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .is_some_and(|inner| !inner.trim_matches(['[', ']']).is_empty())
}

fn pattern_segment(segment: &str) -> String {
    if let Some(inner) = segment
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
    {
        if !inner.is_empty() {
            return "*".to_string();
        }
    }
    if let Some(inner) = segment.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        if !inner.is_empty() {
            return format!(":{inner}");
        }
    }
    escape_literal(segment)
}

/// A literal segment, written so that it can only ever mean itself.
///
/// The pattern language has exactly three pieces of syntax inside a segment: a
/// segment that *is* `*` is the catch-all, a segment that *starts with* `:` is
/// a placeholder, and a `%XX` escape is decoded before the comparison
/// (`deis_bundle::routing::CompiledPattern::compile`, and the same rule in the
/// generated entry module's matcher). A file whose name contains one of the
/// three used to be handed straight through, so the file tree stopped being a
/// route table and started being a route *language* that anyone could write
/// into a file name:
///
/// * `functions/api/*.ts` became `/api/*` - a catch-all that swallowed every
///   path under `/api`, shadowing its siblings, that no author had asked for
///   with `[[name]]`;
/// * `functions/users/:id.ts` became `/users/:id` - a placeholder binding
///   `context.params.id`, indistinguishable from `[id].ts`, and colliding with
///   it if both existed, with which of the two answered decided by a
///   tie-break;
/// * `functions/caf%C3%A9.ts` became `/caf%C3%A9`, which decodes to `/café` -
///   the same route as an ordinary file called `café.ts`.
///
/// Escaped rather than refused. The file tree is the route table, so every
/// file has to have a route, and a legal file name is not a build error - it
/// is a name that has to be spelled in the pattern language rather than
/// interpreted by it. `%2A` and `%3A` decode back to `*` and `:` in both
/// matchers, so the file answers the URL that spells its own name and nothing
/// else. `%` itself is escaped first, which is what makes the mapping from
/// file name to pattern one-to-one: without it `*.ts` and a file actually
/// called `%2A.ts` would land on one route.
///
/// The build says so, once per file, because an author who typed `:id.ts`
/// almost certainly meant `[id].ts` - see
/// `Compiler::report_escaped_route_segments`.
fn escape_literal(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for (index, character) in segment.char_indices() {
        match character {
            '%' => out.push_str("%25"),
            '*' => out.push_str("%2A"),
            ':' if index == 0 => out.push_str("%3A"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn javascript_literals_cannot_be_broken_by_project_paths() {
        assert_eq!(
            javascript_string("../src/a'b\\c.ts"),
            r#""../src/a'b\\c.ts""#
        );
        assert_eq!(javascript_string("/line\nnext"), "\"/line\\nnext\"");
    }

    #[test]
    fn shared_javascript_runtime_carries_the_canonical_route_and_error_rules() {
        assert!(JAVASCRIPT_ROUTE_MATCHER.contains("part === '*'"));
        assert!(JAVASCRIPT_ROUTE_MATCHER.contains("decodeRouteSegment(pathParts[i])"));
        assert!(JAVASCRIPT_ROUTE_PARAM_EXTRACTOR.contains("params['*'] = rest"));
        assert!(JAVASCRIPT_ROUTE_PARAM_EXTRACTOR.contains("params[wildcardName] = rest"));
        assert!(JAVASCRIPT_HANDLER_BOUNDARY.contains("await invoke()"));
        assert!(JAVASCRIPT_HANDLER_BOUNDARY.contains("createEdgeCanonContext"));
        assert!(JAVASCRIPT_HANDLER_BOUNDARY.contains("new EdgeCanonReadableStream"));
        assert!(JAVASCRIPT_HANDLER_BOUNDARY.contains("EC_HANDLER_THROWN"));
        assert!(JAVASCRIPT_HANDLER_BOUNDARY.contains("EC_HANDLER_RESULT_INVALID"));
        assert!(JAVASCRIPT_HANDLER_BOUNDARY.contains("EC_BACKGROUND_REJECTED"));
        assert!(JAVASCRIPT_HANDLER_BOUNDARY.contains("EC_WAIT_UNTIL_CLOSED"));
        assert!(JAVASCRIPT_HANDLER_BOUNDARY.contains("EC_WAIT_UNTIL_PROMISE_REQUIRED"));
        assert!(JAVASCRIPT_HANDLER_BOUNDARY.contains("Internal Server Error\\n"));
        assert!(JAVASCRIPT_HANDLER_BOUNDARY.contains("'cache-control': 'no-store'"));
    }

    #[test]
    fn the_root_index_is_the_root_route() {
        assert_eq!(route_pattern("index.ts"), "/");
        assert_eq!(route_pattern("index.js"), "/");
    }

    #[test]
    fn a_plain_file_is_its_own_path() {
        assert_eq!(route_pattern("api/hello.ts"), "/api/hello");
    }

    #[test]
    fn an_index_names_its_directory() {
        assert_eq!(route_pattern("users/index.ts"), "/users");
        assert_eq!(route_pattern("users/[id]/index.ts"), "/users/:id");
    }

    #[test]
    fn a_bracket_segment_becomes_a_placeholder() {
        assert_eq!(route_pattern("users/[id].ts"), "/users/:id");
        assert_eq!(route_pattern("[org]/[repo]/tree.ts"), "/:org/:repo/tree");
    }

    #[test]
    fn a_double_bracket_segment_becomes_a_wildcard() {
        assert_eq!(route_pattern("api/[[catch]].ts"), "/api/*");
        assert_eq!(route_pattern("[[all]].ts"), "/*");
    }

    #[test]
    fn windows_separators_derive_the_same_pattern() {
        assert_eq!(route_pattern("users\\[id].ts"), "/users/:id");
    }

    #[test]
    fn an_unknown_extension_stays_part_of_the_path() {
        // Stripping it would route two different files to one pattern.
        assert_eq!(route_pattern("data/report.tar.gz"), "/data/report.tar.gz");
    }

    #[test]
    fn empty_brackets_are_a_literal_segment() {
        // `[].ts` names no placeholder, so it can only be a literal.
        assert_eq!(route_pattern("[].ts"), "/[]");
    }

    /// A file name that is itself a route pattern used to become one. The
    /// placeholder and the catch-all are spelled `[name]` and `[[name]]` in a
    /// file tree, so a raw `:` or `*` in a name is a literal - and reading it
    /// as syntax gave the author a route they never wrote.
    #[test]
    fn a_file_named_like_a_pattern_answers_its_own_name_and_nothing_else() {
        // This one is the dangerous half: unescaped it is `/api/*`, a
        // catch-all that swallows every path under `/api` and shadows every
        // sibling route, from a file the author called `*.ts`.
        assert_eq!(route_pattern("api/*.ts"), "/api/%2A");
        // And this one is indistinguishable from `[id].ts` unescaped, so the
        // two files collide on one route and a tie-break picks the winner.
        assert_eq!(route_pattern("users/:id.ts"), "/users/%3Aid");
        assert_ne!(
            route_pattern("users/:id.ts"),
            route_pattern("users/[id].ts")
        );
    }

    #[test]
    fn one_file_name_is_one_route_and_no_two_share_it() {
        // `%` is escaped first, and that is what makes the mapping one-to-one.
        // Without it a file called `%2A.ts` and a file called `*.ts` both
        // reached `/%2A`, and `caf%C3%A9.ts` reached the same route as
        // `café.ts` - because the matcher decodes a pattern's literal
        // segments before comparing them.
        let names = [
            "*.ts",
            "%2A.ts",
            ":id.ts",
            "%3Aid.ts",
            "café.ts",
            "caf%C3%A9.ts",
            "[id].ts",
            "[[rest]].ts",
        ];
        let mut patterns: Vec<String> = names.iter().map(|name| route_pattern(name)).collect();
        patterns.sort();
        let distinct = patterns.len();
        patterns.dedup();
        assert_eq!(
            patterns.len(),
            distinct,
            "two file names produced one route: {patterns:?}"
        );
    }

    #[test]
    fn an_ordinary_name_is_left_exactly_as_it_is() {
        // The escape must be invisible to every project that does not write
        // pattern syntax into a file name, or it changes the route table - and
        // the manifest, and therefore the version id - of applications that
        // were never affected.
        for name in [
            "index.ts",
            "api/hello.ts",
            "users/[id]/index.ts",
            "api/[[catch]].ts",
            "data/report.tar.gz",
            "my file.ts",
            "a+b.ts",
            "a:b.ts",
        ] {
            assert!(
                escaped_segments(name).is_empty(),
                "{name} was escaped: {:?}",
                escaped_segments(name)
            );
        }
        assert_eq!(route_pattern("my file.ts"), "/my file");
        // Only a *leading* colon is a placeholder, so this one means itself
        // already.
        assert_eq!(route_pattern("a:b.ts"), "/a:b");
    }

    #[test]
    fn what_was_escaped_is_reportable_so_the_build_can_say_so() {
        assert_eq!(
            escaped_segments("api/*.ts"),
            vec![("*".to_string(), "%2A".to_string())]
        );
        assert_eq!(
            escaped_segments("users/:id.ts"),
            vec![(":id".to_string(), "%3Aid".to_string())]
        );
        assert!(escaped_segments("users/[id].ts").is_empty());
    }

    #[test]
    fn a_wildcard_is_only_reachable_as_the_last_segment() {
        assert!(wildcard_is_reachable("/api/*"));
        assert!(wildcard_is_reachable("/users/:id"));
        assert!(!wildcard_is_reachable("/api/*/edit"));
    }

    #[test]
    fn placeholders_come_back_in_order() {
        assert_eq!(
            placeholder_names("/:org/:repo/*"),
            vec!["org".to_string(), "repo".to_string(), "*".to_string()]
        );
        assert!(placeholder_names("/api/hello").is_empty());
    }

    #[test]
    fn a_directory_called_src_is_an_ordinary_directory() {
        // The staging prefix is taken off once. Stripped repeatedly,
        // `functions/src/_queue.ts` came back as `_queue.ts`, which is the
        // spelling reserved for the application's own queue consumer.
        assert_eq!(staged_handler_path("../src/_queue.ts"), "_queue.ts");
        assert_eq!(staged_handler_path("../src/src/_queue.ts"), "src/_queue.ts");
        assert_eq!(
            staged_handler_path("../src/src/src/index.ts"),
            "src/src/index.ts"
        );
    }

    #[test]
    fn a_specifier_without_the_staging_prefix_is_left_alone() {
        assert_eq!(staged_handler_path("./index.ts"), "./index.ts");
    }

    #[test]
    fn only_root_event_files_leave_the_http_surface() {
        assert_eq!(handler_kind("_queue.ts"), HandlerKind::Queue);
        assert_eq!(handler_kind("../src/_scheduled.js"), HandlerKind::Scheduled);
        assert_eq!(handler_kind("_unused.ts"), HandlerKind::Reserved);
        assert_eq!(handler_kind("admin/_queue.ts"), HandlerKind::Http);
        assert_eq!(handler_kind("../src/src/_queue.ts"), HandlerKind::Http);
        assert_eq!(handler_kind("admin\\_scheduled.ts"), HandlerKind::Http);
    }

    #[test]
    fn the_written_wildcard_name_survives() {
        assert_eq!(wildcard_name("api/[[catch]].ts").as_deref(), Some("catch"));
        assert_eq!(wildcard_name("users/[id].ts"), None);
    }
}
