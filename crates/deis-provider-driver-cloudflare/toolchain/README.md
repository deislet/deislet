# Pinned wrangler toolchain

wrangler is a Node.js package, not a native binary: its entry point is
`bin/wrangler.js`, and it needs its npm dependencies at run time (`esbuild` to
bundle the Worker, `miniflare`/`workerd`, `unenv`, ...). The driver therefore
installs the whole dependency tree, and does it without npm:

1. `package-lock.json` in this directory is embedded into the driver binary.
   Every package in it carries an exact `resolved` URL and a `sha512`
   `integrity`.
2. The driver refuses the lock unless its `node_modules/wrangler` entry equals
   the pin in `provider-drivers/official-tool-lock.json` (version, tarball URL,
   `npmIntegrity`). A unit test keeps the three in step at build time.
3. On first use the driver downloads each package the host platform needs
   (platform-specific optional packages for other `os`/`cpu`/`libc` values are
   skipped), verifies each tarball's integrity against the lock before
   extracting anything, and additionally checks the wrangler tarball's
   SHA-256 against `distributionSha256`. No install scripts run.
4. The finished tree is renamed into place atomically and a manifest of the
   SHA-256 of every file is written next to it. Every later run re-verifies the
   tree against that manifest and refuses to run a modified toolchain.
5. wrangler runs as `<node> <tree>/node_modules/wrangler/bin/wrangler.js`. The
   driver locates `node` through `DEIS_PROVIDER_NODE` (an absolute path) or
   `PATH`, and refuses a Node.js older than wrangler's `engines.node`
   (`>=22.0.0`).

## Regenerating after a wrangler bump

Update the pin in `provider-drivers/official-tool-lock.json` and the constants
in `src/lib.rs` first, then:

```sh
cd crates/deis-provider-driver-cloudflare/toolchain
# edit package.json: "wrangler": "<new version>"
rm package-lock.json
npm install --package-lock-only --ignore-scripts --no-audit --no-fund \
  --registry https://registry.npmjs.org
cargo nextest run -p deis-provider-driver-cloudflare
```

The lock-consistency test fails until all three places agree.
