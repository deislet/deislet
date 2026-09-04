//! The provider artifacts must expose one Node identity, not the identity of
//! whichever host happens to execute them.  This runs the real bundled entry
//! modules as ESM and interleaves two invocations so the test covers both the
//! module façade and the `AsyncLocalStorage` environment boundary.

use std::fs;
use std::path::Path;
use std::process::Command;

use deis_build::{Compiler, CompilerOptions, OutputMode};

const PROVIDERS: [&str; 3] = ["cloudflare", "tencent", "deno"];

fn stage(project: &Path, vendor: &str) {
    fs::create_dir_all(project.join("functions")).expect("function directory");
    fs::write(
        project.join("functions/index.ts"),
        r#"
          import { deepStrictEqual, rejects } from "node:assert/strict";
          import { AsyncLocalStorage } from "node:async_hooks";
          import { Buffer as ImportedBuffer } from "node:buffer";
          import { createHash, createHmac, timingSafeEqual } from "node:crypto";
          import { channel, hasSubscribers } from "node:diagnostics_channel";
          import { EventEmitter, once } from "node:events";
          import { posix, win32, resolve } from "node:path";
          import { env, platform, version, versions } from "node:process";
          import { stringify as stringifyQuery, parse as parseQuery } from "node:querystring";
          import { Readable, Transform, Writable } from "node:stream";
          import { pipeline } from "node:stream/promises";
          import { StringDecoder } from "node:string_decoder";
          import { setImmediate as nodeSetImmediate } from "node:timers";
          import { setTimeout as sleep } from "node:timers/promises";
          import { domainToASCII, domainToUnicode, fileURLToPath, pathToFileURL } from "node:url";
          import { format as formatValue, inspect, stripVTControlCharacters } from "node:util";
          import { brotliCompressSync, brotliDecompressSync, deflateSync, gzipSync, gunzipSync, inflateSync } from "node:zlib";

          async function selectedSemantics(tenant) {
            const original = ImportedBuffer.from("edge-canon", "utf8");
            const view = original.subarray(5);
            view[0] = 0x43;
            let assertionError;
            try { deepStrictEqual({ value: 1 }, { value: 2 }); }
            catch (error) {
              assertionError = {
                name: error.name,
                code: error.code,
                actual: error.actual.value,
                expected: error.expected.value,
                operator: error.operator,
              };
            }
            await rejects(
              Promise.reject(Object.assign(new Error("expected"), { code: "E_FIXTURE" })),
              { code: "E_FIXTURE" },
            );

            const input = ImportedBuffer.from("edge-canon-node-24", "utf8");
            const digest = createHash("sha256").update(input).digest("hex");
            const hmac = createHmac("sha256", "fixed-key").update(input).digest("hex");
            const unequal = digest.replace(/^./, digest[0] === "0" ? "1" : "0");
            const compression = {};
            for (const [name, compress, decompress] of [
              ["gzip", gzipSync, gunzipSync],
              ["deflate", deflateSync, inflateSync],
              ["brotli", brotliCompressSync, brotliDecompressSync],
            ]) {
              const compressed = compress(input);
              compression[name] = {
                compressedOctets: compressed.byteLength,
                roundtrip: decompress(compressed).toString("utf8"),
              };
            }

            const emitter = new EventEmitter();
            const eventTrace = [];
            emitter.on("value", value => eventTrace.push(`first-${value}`));
            emitter.on("value", value => eventTrace.push(`second-${value}`));
            const oncePromise = once(emitter, "once");
            emitter.emit("value", 7);
            emitter.emit("once", "only");
            const onceValue = await oncePromise;
            const channelName = `edge-canon.provider.${tenant}`;
            const diagnosticChannel = channel(channelName);
            const diagnosticTrace = [];
            const subscriber = (message, name) => diagnosticTrace.push({ message, name });
            diagnosticChannel.subscribe(subscriber);
            diagnosticChannel.publish({ value: 1 });
            diagnosticChannel.unsubscribe(subscriber);
            diagnosticChannel.publish({ value: 2 });

            const fixedUrl = new URL("https://例え.テスト/a?x=1&x=2");
            const decoder = new StringDecoder("utf8");
            const emoji = ImportedBuffer.from("A😀B", "utf8");
            const decoded = decoder.write(emoji.subarray(0, 3))
              + decoder.write(emoji.subarray(3, 5))
              + decoder.end(emoji.subarray(5));
            const streamed = [];
            const streamTrace = [];
            await pipeline(
              Readable.from([ImportedBuffer.from("edge"), ImportedBuffer.from("canon")]),
              new Transform({ transform(chunk, _encoding, callback) {
                streamTrace.push(`transform-${chunk.toString()}`);
                callback(null, ImportedBuffer.from(chunk.toString().toUpperCase()));
              } }),
              new Writable({ write(chunk, _encoding, callback) {
                streamed.push(chunk.toString());
                streamTrace.push(`write-${chunk.toString()}`);
                callback();
              } }),
            );
            let streamError;
            try {
              await pipeline(
                Readable.from([ImportedBuffer.from("x")]),
                new Transform({ transform(_chunk, _encoding, callback) {
                  callback(Object.assign(new Error("stream-sentinel"), { code: "E_STREAM_FIXTURE" }));
                } }),
                new Writable({ write(_chunk, _encoding, callback) { callback(); } }),
              );
            } catch (error) {
              streamError = { settlement: "reject", name: error.name, code: error.code };
            }

            const scheduleOrder = await new Promise(resolveOrder => {
              nodeSetImmediate(() => {
                const order = ["callback"];
                process.nextTick(() => order.push("nextTick"));
                Promise.resolve().then(() => order.push("promise"));
                nodeSetImmediate(() => { order.push("immediate"); resolveOrder(order); });
              });
            });
            const localStorage = new AsyncLocalStorage();
            async function contextProbe(label, wait) {
              return localStorage.run({ label }, async () => {
                const values = [localStorage.getStore().label];
                await Promise.resolve(); values.push(localStorage.getStore().label);
                await new Promise(done => process.nextTick(() => {
                  values.push(localStorage.getStore().label);
                  done();
                }));
                await sleep(wait); values.push(localStorage.getStore().label);
                const localEmitter = new EventEmitter();
                localEmitter.on("check", () => values.push(localStorage.getStore().label));
                localEmitter.emit("check");
                return values;
              });
            }
            const [contextA, contextB] = await Promise.all([
              contextProbe("A", 2),
              contextProbe("B", 1),
            ]);
            localStorage.disable();

            return {
              buffer: {
                utf8: original.toString("utf8"),
                hex: original.toString("hex"),
                viewAliases: original[5] === 0x43,
                assertionError,
              },
              crypto: {
                digest,
                hmac,
                equalTiming: timingSafeEqual(ImportedBuffer.from(digest), ImportedBuffer.from(digest)),
                unequalTiming: timingSafeEqual(ImportedBuffer.from(digest), ImportedBuffer.from(unequal)),
                compression,
              },
              events: {
                eventTrace,
                onceValue,
                diagnosticTrace,
                hasSubscribersAfter: hasSubscribers(channelName),
              },
              paths: {
                pathValues: {
                  posix: posix.normalize("/a//b/../c"),
                  win32: win32.normalize("C:\\a\\b\\..\\c"),
                  relative: posix.relative("/a/b", "/a/c/d"),
                  parsed: posix.parse("/srv/app/index.mjs"),
                  topLevelResolve: resolve("edge", "canon"),
                  posixResolve: posix.resolve("edge", "canon"),
                  win32Resolve: win32.resolve("edge", "canon"),
                },
                urlValues: {
                  ascii: domainToASCII("例え.テスト"),
                  unicode: domainToUnicode(fixedUrl.hostname),
                  params: fixedUrl.searchParams.getAll("x"),
                  relativeFile: pathToFileURL("asset #%.txt").href,
                  posixFile: fileURLToPath("file:///asset%20space.txt"),
                  windowsFile: fileURLToPath("file:///C:/asset%20space.txt", { windows: true }),
                },
                queryValues: {
                  stringified: stringifyQuery({ a: "x y", b: ["1", "2"] }),
                  parsed: parseQuery("a=x%20y&b=1&b=2"),
                },
                utilValues: {
                  formatted: formatValue("%s:%d:%j", "value", 7, { ok: true }),
                  stripped: stripVTControlCharacters("\u001b[31mred\u001b[0m"),
                  inspected: inspect({ b: 2, a: 1 }, { sorted: true }),
                },
              },
              streams: { decoded, streamed, streamTrace, streamError },
              scheduling: {
                scheduleOrder,
                contexts: { A: contextA, B: contextB },
                storeAfterDisable: localStorage.getStore() ?? null,
              },
            };
          }

          export default async function handler() {
            const before = env.SHARED;
            const tenant = process.env.TENANT;
            process.env.SHARED = `changed-${tenant}`;
            await sleep(tenant === "A" ? 25 : 5);
            return new Response(JSON.stringify({
              tenant,
              before,
              after: process.env.SHARED,
              importedAndGlobalEnvAreSame: env === process.env,
              imported: { platform, version, node: versions.node },
              global: {
                platform: globalThis.process.platform,
                version: globalThis.process.version,
                node: globalThis.process.versions.node,
                fields: Object.keys(globalThis.process).sort(),
                versionFields: Object.keys(globalThis.process.versions).sort(),
              },
              hostFieldsHidden: [process.pid, process.argv, process.cwd].every(value => value === undefined),
              virtualRoot: resolve("workspace"),
              bufferHex: ImportedBuffer.from("edge-canon").toString("hex"),
              globals: [typeof Buffer, typeof setImmediate, typeof clearImmediate],
              builtinPath: process.getBuiltinModule("path").join("edge", "canon"),
              builtinUrl: {
                relativeFile: process.getBuiltinModule("url").pathToFileURL("builtin asset.txt").href,
                fields: Object.keys(process.getBuiltinModule("node:url")).sort(),
              },
              unsupportedBuiltin: process.getBuiltinModule("fs") === undefined,
              semantics: await selectedSemantics(tenant),
            }));
          }
        "#,
    )
    .expect("handler");
    fs::write(
        project.join(".config.json"),
        format!(
            r#"{{
              "version": "1.0.0",
              "name": "provider-node-facade",
              "runtime": "standard-v1",
              "language": "typescript",
              "functionRoot": "./functions",
              "compatibilityDate": "2026-09-01",
              "vendors": {{ "{vendor}": {{ "enabled": true }} }},
              "build": {{ "outDir": "./dist", "minify": false, "sourcemap": false }}
            }}"#
        ),
    )
    .expect("config");
}

const NODE_DRIVER: &str = r#"
import { pathToFileURL } from "node:url";
const hostProcess = globalThis.process;
const hostDeno = globalThis.Deno;
const args = hostDeno?.args ?? hostProcess.argv.slice(1);
const entry = pathToFileURL(args[0]).href;
const vendor = args[1];
const environments = [
  { TENANT: "A", SHARED: "initial" },
  { TENANT: "B", SHARED: "initial" },
];
let denoHandler;
if (vendor === "deno") {
  Object.defineProperty(globalThis, "Deno", {
    value: {
      env: { toObject: () => environments.shift() },
      serve: callback => { denoHandler = callback; },
      readFile: async () => { throw new Error("no fixture asset"); },
    },
    configurable: true,
  });
}
const module = await import(entry);
const requests = [
  new Request("https://fixture.invalid/?tenant=A"),
  new Request("https://fixture.invalid/?tenant=B"),
];
const executionContext = { waitUntil() {}, passThroughOnException() {} };
let pending;
if (vendor === "cloudflare") {
  pending = [
    module.default.fetch(requests[0], environments[0], executionContext),
    module.default.fetch(requests[1], environments[1], executionContext),
  ];
} else if (vendor === "tencent") {
  pending = [
    module.onRequest({ request: requests[0], env: environments[0], waitUntil() {} }),
    module.onRequest({ request: requests[1], env: environments[1], waitUntil() {} }),
  ];
} else {
  pending = [denoHandler(requests[0]), denoHandler(requests[1])];
}
const responses = await Promise.all(pending);
const observations = await Promise.all(responses.map(response => response.json()));
hostProcess.stdout.write(`EC_RESULT=${JSON.stringify(observations)}\n`);
"#;

fn run_entry(runtime: &str, vendor: &str, entry: &Path) -> Vec<serde_json::Value> {
    let mut command = Command::new(runtime);
    if std::env::var("DEIS_NODE_TEST_RUNTIME").as_deref() == Ok("deno") {
        command.args(["eval", "--allow-read", NODE_DRIVER, "--"]);
    } else {
        command.args(["--input-type=module", "--eval", NODE_DRIVER]);
    }
    let output = command
        .arg(entry)
        .arg(vendor)
        .output()
        .unwrap_or_else(|error| panic!("start {runtime} for {vendor}: {error}"));
    assert!(
        output.status.success(),
        "{vendor} provider entry failed under {runtime}:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("Node output is UTF-8");
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("EC_RESULT="))
        .unwrap_or_else(|| panic!("{vendor} emitted no observation: {stdout}"));
    serde_json::from_str(line).expect("provider observation JSON")
}

#[tokio::test]
async fn every_provider_node_bundle_normalizes_identity_globals_and_invocation_env() {
    let node = std::env::var("DEIS_NODE_TEST_BIN").unwrap_or_else(|_| "node".to_string());

    for vendor in PROVIDERS {
        let project = tempfile::tempdir().expect("temporary project");
        stage(project.path(), vendor);
        let compiler = Compiler::with_options(CompilerOptions {
            project_dir: project.path().to_string_lossy().to_string(),
            output_mode: OutputMode::Both,
            ..Default::default()
        });
        let result = compiler
            .compile(project.path(), vec![vendor.to_string()], true)
            .await
            .unwrap_or_else(|error| panic!("build {vendor} provider probe: {error}"));
        let code = &result.builds[vendor]
            .artifacts
            .as_ref()
            .expect("memory artifacts")
            .entry_code;
        assert!(!code.contains("edge-canon-facade:"), "{vendor}");
        assert!(!code.contains("edge-canon-native:"), "{vendor}");
        assert!(!code.contains("edge-canon:node-runtime"), "{vendor}");

        let entry = project.path().join(format!("{vendor}.mjs"));
        fs::write(&entry, code).expect("provider entry module");
        let observations = run_entry(&node, vendor, &entry);
        assert_eq!(observations.len(), 2, "{vendor}");
        for (index, value) in observations.iter().enumerate() {
            let tenant = if index == 0 { "A" } else { "B" };
            assert_eq!(value["tenant"], tenant, "{vendor}: {value}");
            assert_eq!(value["before"], "initial", "{vendor}: {value}");
            assert_eq!(
                value["after"],
                format!("changed-{tenant}"),
                "{vendor}: {value}"
            );
            assert_eq!(value["importedAndGlobalEnvAreSame"], true, "{vendor}");
            for identity in ["imported", "global"] {
                assert_eq!(value[identity]["platform"], "linux", "{vendor}: {value}");
                assert_eq!(value[identity]["version"], "v24.20.0", "{vendor}: {value}");
                assert_eq!(value[identity]["node"], "24.20.0", "{vendor}: {value}");
            }
            assert_eq!(
                value["global"]["fields"],
                serde_json::json!([
                    "env",
                    "getBuiltinModule",
                    "nextTick",
                    "platform",
                    "version",
                    "versions"
                ]),
                "{vendor}: {value}"
            );
            assert_eq!(
                value["global"]["versionFields"],
                serde_json::json!(["node"])
            );
            assert_eq!(value["hostFieldsHidden"], true, "{vendor}: {value}");
            assert_eq!(value["virtualRoot"], "/workspace", "{vendor}: {value}");
            assert_eq!(
                value["bufferHex"], "656467652d63616e6f6e",
                "{vendor}: {value}"
            );
            assert_eq!(
                value["globals"],
                serde_json::json!(["function", "function", "function"]),
                "{vendor}: {value}"
            );
            assert_eq!(value["builtinPath"], "edge/canon", "{vendor}: {value}");
            assert_eq!(
                value["builtinUrl"],
                serde_json::json!({
                    "relativeFile": "file:///builtin%20asset.txt",
                    "fields": [
                        "URL",
                        "URLSearchParams",
                        "domainToASCII",
                        "domainToUnicode",
                        "fileURLToPath",
                        "pathToFileURL",
                        "urlToHttpOptions"
                    ]
                }),
                "{vendor}: {value}"
            );
            assert_eq!(value["unsupportedBuiltin"], true, "{vendor}: {value}");

            let semantics = &value["semantics"];
            assert_eq!(semantics["buffer"]["utf8"], "edge-Canon", "{vendor}");
            assert_eq!(
                semantics["buffer"]["hex"], "656467652d43616e6f6e",
                "{vendor}"
            );
            assert_eq!(semantics["buffer"]["viewAliases"], true, "{vendor}");
            assert_eq!(
                semantics["buffer"]["assertionError"],
                serde_json::json!({
                    "name": "AssertionError",
                    "code": "ERR_ASSERTION",
                    "actual": 1,
                    "expected": 2,
                    "operator": "deepStrictEqual"
                }),
                "{vendor}"
            );
            assert_eq!(
                semantics["crypto"]["digest"],
                "cfa7d671d5f577ea4f8847ef508612bd3af792071334dcf00ffb1dfe57d2489c",
                "{vendor}"
            );
            assert_eq!(
                semantics["crypto"]["hmac"],
                "511dc83d6a37511e6a7da6a06e644dd1cfcf9392da2100b137d73fc0f114405b",
                "{vendor}"
            );
            assert_eq!(semantics["crypto"]["equalTiming"], true, "{vendor}");
            assert_eq!(semantics["crypto"]["unequalTiming"], false, "{vendor}");
            for algorithm in ["gzip", "deflate", "brotli"] {
                assert!(
                    semantics["crypto"]["compression"][algorithm]["compressedOctets"]
                        .as_u64()
                        .is_some_and(|octets| octets > 0),
                    "{vendor}: {semantics}"
                );
                assert_eq!(
                    semantics["crypto"]["compression"][algorithm]["roundtrip"],
                    "edge-canon-node-24",
                    "{vendor}"
                );
            }
            assert_eq!(
                semantics["events"]["eventTrace"],
                serde_json::json!(["first-7", "second-7"]),
                "{vendor}"
            );
            assert_eq!(
                semantics["events"]["onceValue"],
                serde_json::json!(["only"]),
                "{vendor}"
            );
            assert_eq!(
                semantics["events"]["diagnosticTrace"]
                    .as_array()
                    .map(Vec::len),
                Some(1),
                "{vendor}"
            );
            assert_eq!(
                semantics["events"]["diagnosticTrace"][0]["message"]["value"], 1,
                "{vendor}"
            );
            assert_eq!(
                semantics["events"]["hasSubscribersAfter"], false,
                "{vendor}"
            );
            assert_eq!(
                semantics["paths"]["pathValues"],
                serde_json::json!({
                    "posix": "/a/c",
                    "win32": "C:\\a\\c",
                    "relative": "../c/d",
                    "parsed": {
                        "root": "/",
                        "dir": "/srv/app",
                        "base": "index.mjs",
                        "ext": ".mjs",
                        "name": "index"
                    },
                    "topLevelResolve": "/edge/canon",
                    "posixResolve": "/edge/canon",
                    "win32Resolve": "\\edge\\canon"
                }),
                "{vendor}"
            );
            assert_eq!(
                semantics["paths"]["urlValues"],
                serde_json::json!({
                    "ascii": "xn--r8jz45g.xn--zckzah",
                    "unicode": "例え.テスト",
                    "params": ["1", "2"],
                    "relativeFile": "file:///asset%20%23%25.txt",
                    "posixFile": "/asset space.txt",
                    "windowsFile": "C:\\asset space.txt"
                }),
                "{vendor}"
            );
            assert_eq!(
                semantics["paths"]["queryValues"],
                serde_json::json!({
                    "stringified": "a=x%20y&b=1&b=2",
                    "parsed": {"a": "x y", "b": ["1", "2"]}
                }),
                "{vendor}"
            );
            assert_eq!(
                semantics["paths"]["utilValues"],
                serde_json::json!({
                    "formatted": "value:7:{\"ok\":true}",
                    "stripped": "red",
                    "inspected": "{ a: 1, b: 2 }"
                }),
                "{vendor}"
            );
            assert_eq!(semantics["streams"]["decoded"], "A😀B", "{vendor}");
            assert_eq!(
                semantics["streams"]["streamed"],
                serde_json::json!(["EDGE", "CANON"]),
                "{vendor}"
            );
            assert_eq!(
                semantics["streams"]["streamTrace"],
                serde_json::json!([
                    "transform-edge",
                    "write-EDGE",
                    "transform-canon",
                    "write-CANON"
                ]),
                "{vendor}"
            );
            assert_eq!(
                semantics["streams"]["streamError"],
                serde_json::json!({
                    "settlement": "reject",
                    "name": "Error",
                    "code": "E_STREAM_FIXTURE"
                }),
                "{vendor}"
            );
            assert_eq!(
                semantics["scheduling"]["scheduleOrder"],
                serde_json::json!(["callback", "nextTick", "promise", "immediate"]),
                "{vendor}"
            );
            for context in ["A", "B"] {
                assert_eq!(
                    semantics["scheduling"]["contexts"][context],
                    serde_json::json!([context, context, context, context, context]),
                    "{vendor}"
                );
            }
            assert_eq!(
                semantics["scheduling"]["storeAfterDisable"],
                serde_json::Value::Null,
                "{vendor}"
            );
        }
    }
}
