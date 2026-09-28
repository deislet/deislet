# Deploying to Cloudflare

Cloudflare is one of the two supported deployment targets. `deis deploy
--target cloudflare` builds the application's Cloudflare payload, stores it on
the control plane, and asks the control plane for a provider deployment; the
control plane runs the Cloudflare provider driver
(`deis-provider-driver-cloudflare`), which drives a pinned `wrangler` through
preflight, plan, prepare, upload, verify, activate and observe.

## Status

| Part | State |
|---|---|
| Build: bindings, queues and cron mapped onto `wrangler.toml` with `deis:unresolved/<kind>/<name>` placeholders | implemented, covered by local tests |
| Payload delivery: CLI upload, control payload store, per-operation work directory re-verified against the digest | implemented, covered by local tests |
| Driver: wrangler install, exit codes, namespaced resource creation, placeholder fill, upload/activate, delete/cleanup | implemented, verified against a fake `wrangler` that records its invocations and replays real outputs and exit codes |
| Against a real Cloudflare account | **not yet verified**; run [`scripts/cloudflare-smoke.sh`](../../scripts/cloudflare-smoke.sh) |
| Environment secrets and deployment-time config values handed from the control plane to the driver | not done: the driver accepts them (`EDGE_CANON_PROVIDER_SECRETS` / `EDGE_CANON_PROVIDER_CONFIG`), the control plane does not send them yet |
| D1 migrations before activate (ADR-008 T5-3) | not done |

Until the smoke script has passed against a real account, treat Cloudflare
deployment as implemented but unverified.

## How a payload reaches the driver

The provider deployment names its artifact by three digests only
(`artifactVersionId`, `canonicalArtifactSha256`, `providerPayloadSha256`). The
bytes travel separately:

1. `deis deploy --target cloudflare` compiles once and gets both the Deislet
   bundle and the Cloudflare payload (`wrangler.toml`, the Worker bundle,
   `binding-plan.json`) from the same canonical artifact. It uploads the
   canonical artifact, the Deislet version (the version is what a provider
   deployment names; no self-hosted pointer moves), and the payload envelope
   to `POST /api/v1/apps/{app_id}/artifacts/{artifact_id}/provider-payloads`.
2. The control plane revalidates the derivation against the stored canonical
   parent and the unpacked tree, and stores the tree under its file-set digest
   in `<bundle_dir>/.provider-payloads/<application id>/<digest>/`.
3. For every driver process, the control plane creates
   `<data_dir>/provider-work/operations/<operation>-<uuid>/` (mode 0700),
   copies the payload to `<that>/<providerPayloadSha256>/`, re-hashes it, and
   only then takes a credential lease and starts the driver with that
   directory as its working directory. The paths travel in the process
   envelope's `workspace` member
   (`contracts/v1/provider-driver-process.schema.json`). The directory is
   removed when the process exits and swept when the control plane starts.
   `<data_dir>/provider-work/state/<target>/<environment>/` persists across
   operations for driver journals, and `<data_dir>/provider-work/home/` is the
   driver's `HOME` (toolchain cache).

The driver process gets a cleared environment: the operation-scoped
credential, `PATH=/usr/local/bin:/usr/bin:/bin`, `TMPDIR` and `HOME`. Node.js
22 or newer must therefore be installed as `/usr/local/bin/node` or
`/usr/bin/node` on the control plane host; the driver installs `wrangler`
itself from its locked package graph and verifies every package before use.

## Setting it up

All of these are control plane API calls made by an admin credential.

1. **Enable provider execution.** Configure `provider_driver_installation_root`
   and `provider_driver_trust_root` in the control plane config; without both,
   provider deployments are refused.
2. **Install the driver release.** `POST /api/v1/providers/driver-releases`
   with the signed release, manifest, authorization, transparency proof and
   archive of `deis-provider-driver-cloudflare`.
3. **Store the Cloudflare credential.** `POST /api/v1/providers/credentials`
   with `backendId: "cloudflare-workers-pages"`, the API token as
   `credential` and the account id as `accountId`. The token needs Workers
   Scripts, Workers KV Storage, D1, R2 and Queues edit permissions on that
   account.
4. **Register the installation and target.** `POST
   /api/v1/providers/installations` with that credential binding, then `POST
   /api/v1/providers/installations/{installation_id}/targets`.
5. **Grant the target to an environment.** `POST
   /api/v1/apps/{app}/environments/{env}/provider-target-grants` with the
   target id.
6. **Deploy.** From the project directory: `deis deploy --target cloudflare
   -e <env>`. When the environment holds grants on more than one Cloudflare
   target, add `--provider-target <name or id>`. `deis provider-deployments
   list|get|cancel|rollback` follow and intervene.

## What the driver creates

Every Cloudflare resource is namespaced by application and environment id, so
two applications that both declare a KV binding `HOT` never share a namespace.
`delete` removes the Worker; data resources (KV, D1, R2, Queues) are only
destroyed by `cleanup` when the operator explicitly consents for that
environment. The exact naming and the consent mechanism are documented in the
driver crate, `crates/deis-provider-driver-cloudflare`.

## Verifying against a real account

[`scripts/cloudflare-smoke.sh`](../../scripts/cloudflare-smoke.sh) needs
`CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`. It builds a sample
application that uses a KV namespace, a D1 database, an R2 bucket, a queue, a
secret and a cron trigger; runs the driver's deploy steps against a throwaway
environment whose name is unique to the run; requests the deployed Worker and
checks that every binding answers; then deletes the Worker and purges that
environment's resources, and checks nothing named for the run is left. It
exits non-zero on any failure and always attempts the cleanup.

The same script runs as the manually dispatched workflow
`.github/workflows/cloudflare-smoke.yml` on the self-hosted runner, with the
token and account id taken from the repository secrets
`CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`.
