# Provider driver release governance: frozen

The control plane only runs a provider driver it has installed from a signed
release. The machinery behind that — release authorization with threshold
Ed25519 signatures over DSSE envelopes, trust-root rotation, transparency log
inclusion proofs with witness cosignatures, and the installer that checks all
of it before writing a content-addressed, read-only driver tree — lives in
`deis-provider-driver` (`release_authorization`, `release_transparency`,
`installer`) and in the control plane's driver-release API
(`/api/v1/providers/driver-releases`).

**This machinery is frozen.** It keeps working, it stays under test, and it
gets fixes; it gets no new features. The reason is proportion: Deislet
positions itself for trusted tenants on self-hosted Linux, the one driver
that exists is Cloudflare's, and it is built and installed by the same
operator who runs the control plane. A signing and transparency chain built
for third-party drivers from independent publishers has no user until such a
driver exists. When one does, this page is where the freeze is lifted.

What the freeze means in practice:

- No new release formats, signature schemes, log or witness features, and no
  new host platforms.
- New provider work (a driver's own lifecycle, secret and config delivery,
  undeploy) goes into the driver and the provider deployment workflow, not
  into release governance.
- The shared contracts under `contracts/v1/provider-driver-release*.json`,
  `provider-driver-transparency-proof.schema.json` and
  `provider-driver-trust-root*.json` do not change.

## Unix only

Deislet builds its control plane for Unix hosts only, so `deis-provider-driver`
does too: it refuses to compile anywhere else. The Windows code it used to
carry — DACL validation of the installation root and trust files, Job Object
process groups, reparse-point checks — has been removed, since there was no
Windows build to run it. The `windows-amd64` value remains in the release
contract's host vocabulary as data; nothing installs or runs it.

## Where to start

Installing the Cloudflare driver release and the rest of the setup is in
[cloudflare.md](cloudflare.md). The terms used here are defined in the
[glossary](../glossary.md).
