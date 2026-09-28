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
| Driver: wrangler install, exit codes, namespaced resource creation, placeholder fill, first-deploy Worker creation, upload/activate, cron schedules and queue consumers (`triggers deploy`), rollback, delete/cleanup | implemented, tested against a fake `wrangler` that records its invocations and models the wrangler 4.129.0 behaviour the driver relies on (see below); the fake is written from wrangler's source, not recorded from a live account |
| Control plane end to end: product API → provider deployment → signed driver release launched as a process → `wrangler` | covered by `crates/deis-control/src/api/catalog/cloudflare_e2e.rs` against the fake `wrangler`: the first deploy creates the Worker before uploading a version; resolved, namespaced `wrangler.toml`; cron schedule, queue consumer and workers.dev applied after activation; a second deploy reuses every resource; delete removes the Worker and keeps the data |
| Against a real Cloudflare account | **not yet verified**; run [`scripts/cloudflare-smoke.sh`](../../scripts/cloudflare-smoke.sh) |
| Environment secrets and deployment-time config values handed from the control plane to the driver | implemented, covered by `cloudflare_e2e.rs` against the fake `wrangler`: the config value fills its `[vars]` placeholder, the bound secret reaches `wrangler versions upload` on stdin and never argv, a rotated secret reaches the next deployment, and a delivery the audit log cannot record does not happen (see [Secrets and config](#secrets-and-config)) |
| Undeploying through the control plane | not done: the driver implements `delete` and `cleanup`, but the control plane has no product API that issues them, and it never passes the purge consent (`DEIS_PROVIDER_PURGE_DATA`) |
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

One driver process may run for at most 270 seconds (the credential lease it
holds is five minutes); the Cloudflare driver stops itself at 240 seconds and
reports a retryable timeout. The first operation on a fresh control plane
also downloads the toolchain into `<data_dir>/provider-work/home/.deis/tools`;
on a slow link that can use up the budget. The operation is then retried, and
because the install is atomic the retry starts it again cleanly; running any
driver operation once before the first real deploy warms the cache.

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

## Secrets and config

`deis deploy --target cloudflare` sends the deployment-time config values
from `.config.json` with the provider deployment request (`configValues`).
Secret values never travel with a request; they are set on the environment
(`deis secret set`) exactly as for a Deislet deployment.

When the deployment is begun, the control plane records an environment
snapshot, in the same table and under the same identity a Deislet
deployment uses: the version's `environment` declarations, the config values
given, and the secret revision at each declared secret's head. A required
value that is missing refuses the deployment there. The snapshot's digest is
written into the desired state as `plan.bindingSnapshotDigest`, so every
receipt of the deployment is bound to exactly those revisions and values; a
secret rotated afterwards reaches the next deployment, not this one.

When the dispatcher launches the driver for a step, it reads the snapshot
back and hands the driver what that step needs, in the process environment
and never on argv or in a file: every step gets the config values
(`EDGE_CANON_PROVIDER_CONFIG`; `plan`, `upload` and `activate` render them
into `[vars]`), and `upload`, the one step that sends
secrets to Cloudflare (`wrangler versions upload --secrets-file
/dev/stdin`), gets the secret values (`EDGE_CANON_PROVIDER_SECRETS`). Only
the revisions the snapshot binds are decrypted. As with a node's
`FetchSecretValues`, the values leave the control plane only once an audit
row saying so (`secret.provider-delivery`, naming the revisions and a count,
never a value) is durable: if the audit log cannot be written, the step is
not started and waits to be retried.

## How a deployment reaches Cloudflare

Each lifecycle step is one wrangler 4.129.0 command (the full table is at the
top of `crates/deis-provider-driver-cloudflare/src/commands.rs`):

* **upload** first asks whether the Worker exists. wrangler refuses
  `versions upload` for a Worker that does not exist yet, so the first upload
  creates it with `wrangler deploy` of a placeholder script that answers 503
  and is deployed with `workers_dev = false` and `preview_urls = false`, so
  nothing is reachable. Then `versions upload` uploads the real version
  without deploying it. The rendered `wrangler.toml` also sets `preview_urls
  = false`, so an uploaded version has no workers.dev preview URL before it
  is activated. If the placeholder deploy fails before the Worker exists, a
  reconcile of the upload reports it as not applied and the upload is
  retried.
* **activate** runs `versions deploy <version>@100%` and then `triggers
  deploy` with the rendered `wrangler.toml`. `versions upload` and `versions
  deploy` never change what Cloudflare keeps per Worker rather than per
  version; `triggers deploy` applies it: the cron schedule, each queue
  consumer, and the workers.dev route. The rendered file always carries
  `[triggers] crons`, empty when the application has no cron, so removing the
  last cron clears the schedule. `triggers deploy` runs in a directory that
  holds only the rendered file plus, for an application with static assets,
  an empty `[assets] directory`: wrangler checks that the directory exists
  even though this command uploads nothing from it.
* **rollback** redeploys the target version, then re-applies the
  configuration that version was activated with (the driver keeps it in its
  state directory). If that state was lost, the version is served and the
  triggers are left as they are; the receipt's diagnostic says so.

Known limits: `triggers deploy` adds and updates queue consumers but never
detaches one the application no longer lists; detach it in the dashboard or
with `wrangler queues consumer remove`. Secrets are additive for the same
reason: `--secrets-file` never deletes a secret an application stopped
declaring. The account must have a workers.dev subdomain registered
(Workers & Pages → your subdomain): `triggers deploy` enables the workers.dev
route and fails without one, and it runs after `versions deploy` has already
moved traffic, so activate then reports a possible side effect and reconcile
keeps failing until the subdomain exists. `versions list` returns only the
10 most recent versions, so a version followed by more than 10 further
uploads before its verify or upload reconcile is reported as missing.

The fake `wrangler` the tests use
(`crates/deis-provider-driver-cloudflare/tests/fixtures/fake-wrangler.sh`)
models exactly these behaviours: `versions upload` fails for an unknown
Worker with wrangler's code-less error, only `deploy` and `triggers deploy`
change schedules, consumers and workers.dev, version ids are UUIDs, a
Worker that was never deployed has no deployment status, `deploy`, `versions
upload` and `triggers deploy` refuse a missing `[assets] directory`, and
versions get preview URLs unless `preview_urls = false`.

## What the driver creates

Every Cloudflare resource is namespaced by application and environment id, so
two applications that both declare a KV binding `HOT` never share a namespace.
`delete` removes the Worker. Data resources (KV, D1, R2, Queues) are
destroyed only when the operator explicitly consents for that environment by
setting `DEIS_PROVIDER_PURGE_DATA=<environment id>` for the driver process:
with that consent, both `delete` (after removing the Worker) and `cleanup`
(which refuses while the Worker still exists) purge the environment's
resources. Without it, neither does. The exact naming and the consent
mechanism are documented in the driver crate,
`crates/deis-provider-driver-cloudflare`.

## Verifying against a real account

[`scripts/cloudflare-smoke.sh`](../../scripts/cloudflare-smoke.sh) needs
`CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`. It builds a sample
application that uses a KV namespace, a D1 database, an R2 bucket, a queue, a
secret and a cron trigger; runs the driver's deploy steps against a throwaway
environment whose name is unique to the run; checks through the Cloudflare
API that the upload left the new Worker unpublished and that activate applied
the cron schedule, the queue consumer and workers.dev; requests the deployed
Worker and checks that every binding, the queue consumer and the cron answer;
then deletes the Worker and purges that
environment's resources, and checks nothing named for the run is left. It
exits non-zero on any failure and always attempts the cleanup.

The same script runs as the manually dispatched workflow
`.github/workflows/cloudflare-smoke.yml` on the self-hosted runner, with the
token and account id taken from the repository secrets
`CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`.
