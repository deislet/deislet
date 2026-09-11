# Trust boundaries and the fleet credential

This is the one place that states what the node credential opens, who holds
it, which ports have to be kept off the public internet, and how to rotate the
value. Everything else that mentions `node_preshared_keys` or `[control]
secret` points here rather than restating it, because the danger in this
credential is not in any one of its uses - it is in the join, and a reader who
meets the uses one at a time never sees it.

It describes the platform as it is in this release. Where something is not
implemented it says so; a plan is not a boundary.

**The shared value described below is now optional.** Every machine may
instead hold its own certificate, whose one URI SAN is a SPIFFE name -
`spiffe://<trust domain>/deislet/control`, `.../deislet/node/<id>` or
`.../deislet/proxy/<id>` - and be admitted by the name it proves rather than
by anything copied between files. A control plane with a `[machine]` channel
configured and an empty `node_preshared_keys` list mints nothing and holds no
fleet-wide value at all; a node or a proxy with a `[machine_identity]` table
leaves `[control] secret` out of its file entirely, and an absent line there
is a configuration, not an omission. Where a certificate now stands in a
value's place, this document says so. Most of what the shared value used to
open is certificate-only now; what is still true of it, and for whom, is
exactly what follows.

## One credential, whole fleet

`node_preshared_keys` in the control plane's configuration file, where a file
still names one, is a **list of equivalent values**, not a set of per-node
identities. Every value in the list is equally valid wherever this document
says a value is still accepted, and no surface asks which machine is calling
by anything other than the value itself:

- `crates/deis-control/src/api/middleware.rs`, `NodeAuthConfig` - the code
  that used to guard the HTTP endpoints a node called (registration, the
  assignment list, the bundle download) still exists, but nothing in the
  running server mounts it any more. Those routes are not reached with this
  value, or with anything else, over HTTP - see "What one value opens" below.
- `crates/deis-control/src/handler.rs` and
  `crates/deis-control/src/api/grpc.rs` - the control plane's original node
  channel, a long-lived gRPC stream (`ControlService::establish_channel`),
  still checks the same list for a caller that proves no certificate. It is
  mounted on the human listener unconditionally, and it is the channel
  deis-proxy's own control connection still falls back to when it has no
  `[machine_identity]` table.
- `crates/deis-control/src/runtime_client/mod.rs`, `NodeProof::PresharedKey`
  and `NodeKeyStatus` - when this control plane dials a node's own management
  plane and has no certificate to dial it with, it presents **the value that
  node proved when it last registered this way**, never another one; a node
  whose value has since left the list is not called at all. A node from
  `deis_runtime_nodes` that has never registered - by certificate or
  otherwise - has proved nothing and is driven with the list's first value,
  and so, for now, is a node that proved a certificate but reported no
  separate machine management address of its own: the certificate has so far
  only secured the direction where the node asks and the control plane
  answers, not the direction where the control plane calls in. Which state a
  node is in is shown by `key_status` and `key_fingerprint` on
  `GET /api/runtime` - a SHA-256 prefix, never the value; a node admitted by
  certificate instead reports `key_status: "proved"`, no fingerprint at all,
  and the SPIFFE name it proved in a separate `identity` field.

So wherever a file still names one, there is no such thing as "one node's
key". There is the fleet credential, and a holder of any one of its values -
but its reach is far narrower than it was: see the table below for what a
bare value like that actually opens today.

## What one value opens

Three of the surfaces this credential used to open over HTTP are gone in this
release. `PUT /api/nodes/{id}/registration`, the assignment-list route and the
bundle download are not mounted on the control plane's human listener at all
any more - `crates/deis-control/src/api/registration.rs` and
`crates/deis-control/src/api/catalog/mod.rs` still define the handlers and
the credential check that used to guard them, but nothing outside the crate's
own tests wires either to a route. A request to one of the old paths gets a
**404**, never a 401: this listener does not serve them, and a 401 would
suggest a credential could change that. The same is true of
`/api/routing-table`.

| Surface | What the holder can do today |
|---|---|
| `deis_wire::conventions::NODE_ASSIGNMENTS_ROUTE`, `NODE_REGISTRATION_ROUTE`, `/api/routing-table`, the bundle download - all on the control plane's human listener | Nothing. Every one of the four answers 404 to every caller, with or without a credential. A node or a proxy reaches the equivalent of the first three - `FetchAssignments`, `Register`/`Deregister`, `FetchRoutingTable` - only as `deis.machine.MachineChannel` RPCs on the control plane's separate *machine* listener, which admits nobody who has not first completed a mutual-TLS handshake proving a certificate this fleet signed; the bundle download is still plain HTTP, but served on that same certificate-only listener (`crates/deis-control/src/machine/listen.rs`). |
| The control plane's original node channel - `ControlService::establish_channel`, still mounted on the human listener (`crates/deis-control/src/api/grpc.rs`) | Attach a long-lived gRPC stream and be admitted by a value in `node_preshared_keys`, exactly as before. What that buys is a place in the dashboard's session list, counted heartbeats, and whatever operator-issued command (`shutdown`, `restart`, `reload`, a status or metrics fetch) is later addressed to the id the caller announced - **not** a place in the fleet's delivery-target registry (`crate::runtime_client::RuntimeNodeManager`), and not the assignment list or a bundle. |
| `deis.management.RuntimeManagement` on a node this control plane still dials with a bearer value, because that node has not registered a certificate or has registered one without a machine management address of its own | Unchanged from before this release: `ApplyDeployment` (run a bundle of the caller's choosing under any tenant's identity, **with that environment's decrypted secrets carried in the same request**), `UnloadApp`, `UpdateApp`, `PushConfig`, `ExecuteCommand`, `GetStatus` (every tenant's applications and metrics on that node), and `StartInspect`. (`crates/deis-runtime/src/dispatcher/auth.rs`) A node whose `[machine_identity] management_listen` this control plane dials instead admits none of this to any caller but the one certificate naming its own control plane. |
| `deis.trigger.Trigger` on the same node | Fire queue and cron handlers. Same condition as the row above. |
| The inspector | `StartInspect` mints a session token; that token opens a Chrome DevTools Protocol WebSocket **on the node's data port** against a running isolate. Full debugger access to a tenant's running code. (`crates/deis-runtime/src/dispatcher/handlers.rs`) |

Read as one sentence: **the control plane's own copy of every tenant's
plaintext is no longer reachable with a bare shared value** - the assignment
list, the bundle store and the routing table are certificate-only now, and a
certificate is scoped to the one machine it names, not to "any node". What a
shared value still opens, unchanged, is administrative control - secrets
included, through `ApplyDeployment` - of any *node* that has not itself
finished moving its management plane to `[machine_identity]`. And a
certificate is not, on its own, the fix for that last row: `FetchAssignments`
answers with **every** environment's decrypted secrets regardless of which
node's name asked for it (`crates/deis-control/src/api/catalog/assignments.rs`,
`assignments_for`) - a compromised certificate for any one node still reads
every tenant's plaintext, it is simply no longer a value anyone could copy
out of a configuration file to get there.

## Who holds it

Three kinds of file carry the value, where one is still configured:

1. **The control plane's configuration** - `node_preshared_keys`, now
   `#[serde(default)]` and legitimately absent on an installation whose
   `[machine]` channel has replaced it.
2. **Every node's configuration** - `[control] secret`, now `Option<String>`.
   Leaving the line out says this node authenticates with a certificate
   instead; a line present but blank is still refused, because that is an
   operator who forgot to fill it in, not one who chose a certificate.
   `[management] keys` still holds whatever is added there during a rotation.
3. **deis-proxy's configuration** - `[control] secret`, also now
   `Option<String>`. deis-proxy is not a runtime node; where it still carries
   this value, it is because its control connection falls back to it when it
   has no `[machine_identity]` table (`crates/deis-proxy/src/control/client.rs`).
   Its catalog poll - the routing-table fetch - no longer accepts this value
   at all: without an identity, that request now lands on the dead HTTP path
   above and gets a 404. A proxy configured with only `[control] secret` and
   no `[machine_identity]` can still open its control connection but **cannot
   fetch a routing table**, which is most of what a proxy is for; deis-proxy
   itself now refuses to start when `[control]` is present and both `secret`
   and `[machine_identity]` are absent, for exactly this reason
   (`crates/deis-proxy/src/config/system/mod.rs`).

The third one used to be the uncomfortable one, because the one credential a
proxy carried opened the assignment list, the bundle download and every
node's management plane besides the routing table it was described by. That
is no longer what a bare `[control] secret` buys a proxy: read the table
above. What still deserves the same suspicion in this release is a
**certificate** - whichever machine's `[machine_identity]` private key an
attacker can read opens that one machine's control connection, and if it
names a node, `FetchAssignments` on it still returns every tenant's
plaintext, not only that node's own. Treat every host holding a
`[machine_identity]` private key, proxy included, as inside the control
plane's trust boundary, not in a DMZ in front of it - the DMZ argument that
used to apply to a shared bearer value never applied to a certificate that
opens the same door.

## The node's data port is a trust boundary

The node's data port binds `0.0.0.0:http_port`
(`crates/deis-runtime/src/dispatcher/mod.rs`). Anything that can reach it can:

- Address **any application in any environment** on that node by sending
  `x-deis-app-id` and `x-deis-environment` headers. That includes preview
  environments and applications with no route in the catalog at all: the node
  routes on the headers, and having no public address is not a defence.
- Reach the management and trigger gRPC services, because by default they
  share this listener. They are authenticated - a caller with no credential is
  refused - but the door is on the public port.
- Reach the inspector WebSocket, with a token from `StartInspect`.

None of this is reachable *through* deis-proxy: it resolves an inbound
`x-deis-app-id` against the applications it knows and refuses an id that names
none of them - an `_inspect:` target names none - and it stamps the routing
headers itself on everything it forwards. That is exactly why the node's own
port has to be unreachable as well. The proxy is the only thing enforcing
rules the node does not, so a request that arrives at the node directly is a
request that skipped all of them.

**What must sit in front of it.** The node's data port must be reachable only
from the hosts that are supposed to talk to it, enforced by a firewall or by a
private network - not by the absence of a published address:

| Port | Who may reach it |
|---|---|
| Node data port (`http_port`) | deis-proxy, and the control plane. Nothing else, and never the internet. |
| Node management port, if `[management] listen` moved it | The control plane only, admitted by `[control] secret` or `[management] keys`. |
| `[machine_identity] management_listen`, if configured | The one machine this node's `[machine_identity]` names as its control plane, and nobody else this fleet has ever signed a certificate for - every other proved name is refused. |
| Control plane's human HTTP/gRPC port | Nodes and proxies still using the legacy channel, and the operators' network. Never the internet. It no longer serves the assignment list - see "What one value opens" - but it still carries `api_keys`-guarded dashboard routes and the legacy `ControlService` stream. |
| Control plane's machine listener, if `[machine]` is configured | Every machine holding a certificate this fleet signed. This is where the assignment list actually lives now, so it must never be the internet either. |
| deis-store/deis-queue gRPC ports | Runtime nodes, the control plane and the operators' network only. The optional migration service shares this listener but uses its own Bearer credential; neither backend belongs on the internet. |
| deis-proxy's listeners | The internet. This is the only one that belongs there. |

Setting `[management] listen` to a separate address takes the management and
trigger services off the data port entirely - their paths then fall through to
the application router and answer as any request naming no application does.
That is worth doing, and it is not a substitute for the firewall: the data
port still addresses every application and every environment on the node, and
it still carries the inspector.

`[management] listen` on a loopback address with no key configured is the
single-box case, and it stays open on purpose - the address is the
containment. Any other address with no key is shut rather than open. A node
with `[machine_identity] management_listen` configured instead is never in
this "shut or loopback" situation on that third socket: it admits exactly one
proved name and refuses every other certificate this fleet has signed, key or
no key.

## File permissions

Nothing in the platform checks the mode of a configuration file. The only mode
that is checked is the control plane's `secret_key_file`, which warns when it
is group- or world-readable
(`crates/deis-control/src/catalog/secrets.rs`). Everything below is therefore
the operator's to enforce:

| File | Required |
|---|---|
| The control plane's configuration (`-c`) | `0600`, owned by the service user. Where it is configured, it holds `node_preshared_keys`, `api_keys`, `storage_auth_secret`, `store_migration_admin_token`, `queue_migration_admin_token` and `telemetry_migration_admin_token`. The three migration values must be different from each other and every other credential. |
| The control plane's `secret_key_file` | `0600`. This is the master key every secret in the database is encrypted with; the encrypted rows are worth nothing without it, and everything with it. |
| Every node's configuration | `0600`, owned by the node's service user. Where it is configured, it holds `[control] secret`. |
| deis-proxy's configuration | `0600`, owned by the proxy's service user. Same value, same consequences, where it is still configured. |
| `[machine]` / `[machine_identity]` certificate, private key and trust root files, on every machine that has them | `0600` on the private key at least, owned by that process's service user. This file is not shared - it is the whole reason a leaked copy of it only ever names the one machine it belongs to - but a node's private key still opens that node's certificate identity, and a compromised node's `FetchAssignments` still answers with every tenant's plaintext (see "Who holds it"). Issuance, renewal and revocation of these files are outside this platform; only the mode and the ownership are this document's business. |
| deis-store and deis-queue configurations | `0600`, owned by the service user. They may hold both the application-data `auth_secret` and the independent `migration_admin_token`; possession of the latter can freeze and inventory any tenant in that backend. Never reuse one value for both domains. |
| `.demo/*.toml` written by `scripts/demo.sh` | The demo writes every file it generates as `0600`: `state.env`, `secret.key`, the six service configurations (five of them carry a credential; deis-telemetry's carries none and gets the mode for uniformity) and the ACME-restart copy of the proxy configuration. It is still a workstation tool: it puts every process under one user, so the mode separates the demo from other accounts on the host and nothing else. Do not copy a `.demo/` configuration into a real deployment. |

## Rotating the credential

**This procedure covers the legacy shared value only, and only what is still
load-bearing about it: deis-proxy's control-channel fallback and a node's own
management-plane check.** A node's *pull* side - fetching its assignments and
its bundles - is certificate-only now (see "What one value opens"), so
nothing below moves that direction. And the mechanism this procedure used to
lean on - a node dynamically re-registering itself with the control plane
over HTTP so that the control plane's own calls pick up the value it just
proved - no longer runs: that HTTP route is not mounted (see above), so a
node that has not moved to `[machine_identity]` neither registers nor
refreshes there any more. What follows for such a node is therefore a
**restart-driven** rotation, not the zero-touch one this section used to
describe for the pull side; only a node still being dialled by this control
plane with a bearer value, or a proxy falling back to one on its control
connection, is affected at all.

A node accepts `[control] secret` plus everything in `[management] keys`
(`crates/deis-runtime/src/config.rs`, `management_keys`) for calls made *into*
it, and that part is unchanged and still a file edit plus a restart, not a
network exchange:

1. **Teach every node the new value.** Add it to `[management] keys` on each
   node and restart the nodes one at a time. Each node now answers to both the
   old and the new value on its management plane.
2. **Add the new value to the control plane's `node_preshared_keys`, keeping
   the old one, with the new value first**, and restart the control plane.
   With no live re-registration left on this path, every node this control
   plane still dials with a bearer value - `NodeProof::PresharedKey` is not
   created by anything running today - is dialled with the **first** entry in
   the list, so this one restart switches every such node from the old value
   to the new one at once; step 1 is what keeps that from being a moment of
   failed calls, because each node still answers to both.
3. **Move each node's `[control] secret` to the new value** and drop the old
   one from that node's `[management] keys`. Restart the nodes one at a time;
   until a node is restarted it still accepts the old value too, from step 1.
4. **Move deis-proxy's `[control] secret`** to the new value, where it still
   has one, and run `deis-proxy -s reload` (or the control plane's `restart`
   command, which answers that the upgrade was *queued*; the replacement's own
   registration at `GET /api/sessions` is what says it came up). The
   replacement process reads the file and takes the listening sockets over, so
   this step drops no connection. A proxy with a `[machine_identity]` table
   needs none of this: it has nothing here to rotate.
5. **Remove the old value from `node_preshared_keys`** and restart the control
   plane. The old value now opens nothing on the control plane, and (once
   every node has been through step 3) nothing on any node either.

Restarting nodes one at a time is what keeps traffic up: deis-proxy
health-checks its upstreams and takes a node out of rotation while it is down.
Restarting the control plane does not interrupt a node's own serving at all -
a node serves what it already has when the control plane is unreachable, and,
on a certificate-based node, keeps pulling its own assignments and bundles
straight through the control plane's restart.

**If a value may have leaked, this rotation is not enough on its own for a
node still driven by it.** Whoever held it could reach that node's whole
management plane, `ApplyDeployment` included, which carries decrypted
secrets in the same request. Rotate that node's own application secrets too,
and assume any bundle that node ran was readable by whoever could reach it.
The leaked value opens nothing on the control plane itself in this release -
no assignment list, no bundle store, no routing table - so the exposure is
scoped to whichever nodes were still dialled with it, not to the whole fleet.

## What is not implemented

**Per-tenant key separation (envelope encryption) is not implemented in this
release.** Every environment's secrets are encrypted in the control plane's
database with one master key from `secret_key_file`
(`crates/deis-control/src/catalog/secrets.rs`), and `FetchAssignments` hands
them out decrypted to whichever node's certificate a caller proves - the
answer is not scoped to what that one node actually runs, so proving any
single node's name in this fleet is enough to read every environment's
secrets, not only that node's own. There is no per-tenant data key and no
per-node scoping of the assignment list. A
certificate does carry an identity a shared value never did - `NodeKeyStatus`
distinguishes `proved` from a fingerprinted preshared value precisely because
one names a machine and the other does not - but that identity is not yet
used to narrow what the identified machine is handed back.

The consequence is close to the sentence this document opened with, adjusted
for what changed: one master key, and every tenant behind it, reachable now
by a certificate instead of a copied string. The credential got harder to
steal; the blast radius of stealing one did not shrink.

## Related

- [crates/deis-control/DEPLOYMENT.md](../crates/deis-control/DEPLOYMENT.md) -
  deploying the control plane, including the firewall rules this document sets
  the requirements for.
- [crates/deis-control/API_DOCUMENTATION.md](../crates/deis-control/API_DOCUMENTATION.md) -
  the node endpoints in detail.
- [docs/architecture.zh.md](architecture.zh.md) - the design these boundaries
  fall out of.
