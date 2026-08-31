# Contributing

This is the only contributing document in the repository. It sits next to
`deis` because that is where it grew, but it applies to the whole workspace.

It replaces a document that described a pnpm monorepo with `packages/types`,
`packages/cli` and Vitest. None of that has ever existed here: this is a Cargo
workspace, and the only npm project in it is the control plane's web console.

## What you need

- Rust 1.85 or later. Crates in this workspace are a mix of edition 2021 and
  edition 2024, and 1.85 is the first release that understands the latter.
- Node.js 18 or later — the compiler shells out to it for bundling, and the
  control plane's console is built with it.

## First build

```bash
# The console is embedded in deis-control at compile time and its output is
# gitignored, so this comes first or `cargo build` fails.
cd crates/deis-control/web && npm ci && npm run build && cd -

cargo build --workspace
```

## The loop

Work one package at a time. `--workspace` builds a V8 engine you probably did
not touch.

```bash
cargo build   -p <package>
cargo test    -p <package>
cargo clippy  -p <package> --all-targets -- -D warnings
cargo fmt     -p <package>
```

Zero clippy warnings in what you touched, and `cargo fmt` before you commit.

For anything that crosses component boundaries, run the real thing:

```bash
scripts/demo.sh start
scripts/demo.sh verify
scripts/demo.sh stop
```

## Rules that are not negotiable

**Every behavioural change gets a test.** A change nothing can fail on is a
change nobody will notice breaking.

**Code is English.** Comments, identifiers, log messages, commit messages —
all English, in every `.rs`, `.js`, `.ts`, `.toml` and `.proto`. Markdown may
be Chinese; name those files `*.zh.md`.

**A feature is not finished until a request can reach it.** Written, tested and
registered is not the same as reachable, and this repository has produced six
of those at once: site rules only tests called, an `env.ASSETS` with a Rust
half and no op, a counter nothing incremented — so an operator read "no isolate
ever ran out of heap" from a number no code ever wrote — a manifest `limits`
block the node dropped on the floor, traces with no reader, and a CLI calling
a control-plane route that did not exist. The check that closes a piece of work
is to walk outward from every new symbol looking for a caller, and to drive the
thing the way a user drives it.

**Documents must be true or gone.** If you change behaviour, change the
document that describes it in the same commit. If a document is a snapshot of
a moment rather than a statement of a contract, delete it — a stale status
document is worse than no document, because it is read as current. Two waves
of this project have been lost to exactly that.

**Never copy a shared constant into prose or into a second crate.** Header
names, route paths, tenant separators, message ceilings all live in
`deis_wire::conventions`. Nothing checks that two crates spell an HTTP path the
same way: a node once asked for `/register` while the control plane served
`/registration`, both compiled, and the symptom was a fleet with no nodes in
it.

**Names mean one thing each.**

| Word | Means | Never means |
|---|---|---|
| worker | a Cloudflare Worker | a node, a thread, a process |
| node | a registered `deis-runtime` process | a Worker |
| control | the control plane, `deis-control` | supervisor, master |
| env | an application's variables (`context.env`) | environment; that flag is always `-e, --environment` |

## Adding a build target

The adapters live in `crates/deis-build-adapter-<vendor>`, and the crate name
is the `--vendor` value verbatim — `--vendor cloudflare` is served by
`deis-build-adapter-cloudflare`. A `VENDOR` constant in each one, and a test
that compares it with `CARGO_PKG_NAME`, is what keeps that from drifting.
`deis-build`'s dispatch matches on those constants rather than on literals.

To add one:

1. Create `crates/deis-build-adapter-<vendor>` with `pub const VENDOR` and the
   naming test.
2. Implement `generate(routes, config) -> AdapterArtifacts`.
3. Add the variant to `Platform` in `deis-build-core`, along with its honest
   capability set — a feature the platform cannot do must be `false`, so the
   build fails rather than the request.
4. Add the arm to `deis-build`'s dispatch and the re-export in its `lib.rs`.
5. Write a `spec_compliance` test against the vendor's real documented shape.

## Commits

[Conventional Commits](https://www.conventionalcommits.org/), in English,
short but specific:

```
feat(control): deliver decrypted secrets with the assignment list
fix(runtime): stop a queue poller when its application is unloaded
docs: make the written record match the code
```

Do not reference tooling, assistants or generators anywhere — commit messages,
comments, or documents.

## Pull requests

A clear description, the linked issue, tests for the behaviour changed, and a
green `cargo clippy -D warnings` and `cargo test` for every package touched.

## Reporting a problem

What you did, what you expected, what happened, and the versions of Rust and
Node. If it involves the platform running, `.demo/logs/` from
`scripts/demo.sh` is usually the fastest evidence.

## License

Contributions are released under Apache-2.0, the licence of this repository.
