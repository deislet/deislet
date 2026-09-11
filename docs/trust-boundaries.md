# Trust boundaries

This is the one place that states how Deislet's own processes prove who they
are to each other, what each proof opens, which ports have to be kept off the
public internet, and which credentials are still shared by more than one
machine. Everything else that touches a piece of this points here rather than
restating it, because the danger is not in any one surface - it is in the
join, and a reader who meets the surfaces one at a time never sees it.

It describes the platform as it is in this release. Where something is not
implemented it says so; a plan is not a boundary.

**What changed, and what did not.** The platform's own processes -
the control plane, every runtime node, deis-proxy - now recognise each other
by a certificate held by one machine, proved during a TLS handshake, and the
fleet-wide value they all used to hold does not exist any more. What did not
change: `storage_auth_secret` and the matching `auth_secret` on deis-store,
deis-queue and deis-telemetry, and `api_keys` for the human API, are still
values copied between files, and whoever holds one holds everything it
opens. A reader who concludes that everything moved to certificates will
protect the wrong files, which is why this document says which is which before
it says anything else.

## How a process proves who it is

Each machine holds three PEM files - its own certificate chain, the matching
private key, and the trust root every peer must chain to - and a certificate
carries exactly one URI subject alternative name, a SPIFFE name
(`crates/deis-identity/src/lib.rs`). This fleet spells three of them
(`crates/deis-identity/src/names.rs`):

```text
spiffe://<trust domain>/deislet/control
spiffe://<trust domain>/deislet/node/<node id>
spiffe://<trust domain>/deislet/proxy/<proxy id>
```

The control plane has no id of its own, because a trust domain has exactly one
control plane. A node and a proxy carry the id they are known by everywhere
else, so "which machine is this certificate for" needs no lookup table.

The name is established once per connection, at the handshake, before any
request exists. On the control plane's side an acceptor
(`crates/deis-control/src/machine/accept.rs`) completes the handshake, reads
the name off the peer's certificate and attaches it to every request that
arrives on that connection as `PeerIdentity`; a connection whose certificate
names nobody is closed before a request is read from it. On a node's side the
same shape holds the connection to one expected name
(`crates/deis-runtime/src/dispatcher/machine.rs`).

Nothing here issues, renews or revokes a certificate. That is the
installation's own certificate authority, and `deis-identity` deliberately
knows nothing about where the files came from. What the platform insists on is
the shape of the name, not its issuer.

### Where each process is configured

| Process | Section | Refuses to start when |
|---|---|---|
| deis-control | `[machine]`: `bind_addr`, `port`, `certificate`, `private_key`, `trust_root`, `identity_reload_seconds` (default 60), `public_base_url` (`crates/deis-control/src/config.rs`) | the section is absent. Everything a node or a proxy calls is served there, so a control plane without it would answer a dashboard and be dead to its fleet (`crates/deis-control/src/main.rs`). |
| deis-runtime | `[machine_identity]`: the three files, `control_endpoint` (an `https://` URL naming an explicit port), `trust_domain` or `control_identity`, `management_listen`, `identity_reload_seconds` (`crates/deis-runtime/src/config.rs`) | `control.endpoint` is set and `[machine_identity]` is not; or the table has neither `control_endpoint` nor `management_listen`; or `management_listen` names the same port as `http_port`. |
| deis-proxy | `[machine_identity]`: the three files, `control_endpoint`, `trust_domain` (with `control_identity` as an override when the control plane's certificate was issued outside this naming convention) (`crates/deis-proxy/src/config/system/machine_identity.rs`) | `[control]` is present and `[machine_identity]` is not - that proxy would poll a route answering 404 for as long as it ran and route on whatever its own file said (`crates/deis-proxy/src/config/system/mod.rs`). |

## What each proof opens

Everything the platform's own processes say to each other runs on the control
plane's `[machine]` listener or on a node's `management_listen`, both of which
demand a client certificate. Two sockets, and what is on them:

**The control plane's machine listener** (`crates/deis-control/src/machine/listen.rs`)
serves the gRPC service `deis.machine.MachineChannel` - `Register`,
`Deregister`, `FetchAssignments`, `FetchRoutingTable` - the bundle download
beside it over HTTP/1.1 (`versions::BUNDLE_ROUTE`), and the long-lived control
channel `deis.control.ControlService/EstablishChannel`.

**A node's management listener** (`crates/deis-runtime/src/dispatcher/machine.rs`)
serves `deis.management.RuntimeManagement` and `deis.trigger.Trigger`, and
admits exactly one name: the control plane this node was configured to expect.
Every other certificate this fleet has ever signed is refused at the
connection, node and proxy certificates included.

| A holder of | can do |
|---|---|
| the control plane's private key | Drive any node that has registered a name and a management address: `ApplyDeployment` (run a bundle of the caller's choosing under any tenant's identity, **with that environment's decrypted secrets carried in the same request**), `UnloadApp`, `GetStatus` (every tenant's applications and metrics on that node), `StartInspect`, `HealthCheck`, and the cron trigger `OnScheduled`. `PushConfig`, `UpdateApp` and `ExecuteCommand` exist on the wire and answer `unimplemented` (`crates/deis-runtime/src/management/service.rs`). This is the only way any node is dialled: `crates/deis-control/src/runtime_client/mod.rs` builds one channel, over mutual TLS, and a node that proved no name or reported no management address is not driven at all and says so. |
| one node's private key | `Register` and `Deregister` **its own id only** - the id in the request is held against the id in the proved name and a mismatch is refused (`crates/deis-control/src/machine/service.rs`). `FetchAssignments` for its own id only, and that answer carries **every** environment's decrypted secrets, not that node's (see "What is not implemented"). `FetchRoutingTable`. The bundle download for any application and any version: the handler checks nothing about who is asking (`crates/deis-control/src/api/catalog/versions.rs`, `download_bundle`). And the control channel, for a place in the session list, heartbeats and whatever operator-issued command is addressed to the id it announced. |
| one proxy's private key | `FetchRoutingTable`, and the control channel. Not `FetchAssignments`: a proxy name yields no node id, so the call is refused. The bundle download is not scoped either, so a proxy certificate reaches it too. |
| an inspector session token, minted by `StartInspect` | A Chrome DevTools Protocol WebSocket **on the node's data port**, against a running isolate: full debugger access to a tenant's running code (`crates/deis-runtime/src/dispatcher/handlers.rs`). |

The control plane's human listener still carries `EstablishChannel` as well,
and it admits nobody there: that listener has no acceptor, so no connection on
it proves a name, and the handshake message says so rather than checking a
field (`crates/deis-control/src/api/grpc.rs`). The HTTP routes a node used to
call are not mounted anywhere - `deis_wire::conventions::NODE_REGISTRATION_ROUTE`,
`deis_wire::conventions::NODE_ASSIGNMENTS_ROUTE`, `/api/routing-table` and the
bundle download all answer **404** on the human listener, to every caller, with
or without a credential. Not 401: a credential cannot bring them back.

## What is still a shared secret

None of these moved to certificates, and each is a value copied into more than
one file.

| Value | Held by | What holding it is worth |
|---|---|---|
| `storage_auth_secret` on deis-control, the identical `auth_secret` on deis-store and deis-queue | three processes, one value | The control plane mints `base64(HMAC-SHA256(auth_secret, tenant))` for each tenant and writes it into that tenant's deployment; the backend recomputes the expected token from the tenant the call itself claims (`crates/deis-store/src/auth.rs`, `crates/deis-control/src/delivery/mod.rs`). The secret mints a token for **any** tenant, so holding it is holding every tenant's data in both backends. A token does not expire; the only lever is changing the secret, which invalidates every token at once. |
| `auth_secret` on deis-telemetry, the identical `[logging] auth_secret` on **every node** | deis-telemetry and every runtime node | One token for the whole service - `base64(HMAC-SHA256(auth_secret, "deis-telemetry"))`, the subject fixed - so it says "the holder is part of this platform" and names no tenant (`crates/deis-telemetry/src/auth.rs`, `crates/deis-runtime/src/telemetry.rs`). Holding it is reading every application's logs, traces and metric series. This is the one shared value a node's own configuration file still carries. |
| `api_keys` on deis-control | the configuration file, the CLI, and anything scripted against the API | An entry resolves to an owner principal with unscoped authority, compared in constant time against the file; the database holds no copy of it and cannot admit anyone (`crates/deis-control/src/api/authz.rs`). Minted tokens (`deis token …`) are the scoped alternative and are stored as digests. |
| `store_migration_admin_token`, `queue_migration_admin_token`, `telemetry_migration_admin_token` | deis-control and the backend it names | Freeze and inventory any tenant in that backend. Each is independent of the others and must not equal that backend's `auth_secret` - deis-store refuses to start its management service if it does (`crates/deis-store/src/server.rs`). |
| the master key in `secret_key_file` | deis-control alone | Not shared between machines, but one key for every tenant: every environment's secrets in the database are encrypted with it (`crates/deis-control/src/catalog/secrets.rs`). The encrypted rows are worth nothing without it and everything with it. |

## What the fleet-wide key was, and why it went

It is worth a paragraph because the current design is shaped by it.

`node_preshared_keys` was a list of equivalent values in the control plane's
configuration, and every node and proxy carried one of them as `[control]
secret`. A node's management plane accepted that value plus anything in
`[management] keys`. It authenticated a *holder*, never a machine: any holder
could register as any node, fetch any node's assignment list - which carries
decrypted secrets - download any bundle, read the routing table, and drive any
node's management plane as if it were the control plane. A copy of one
configuration file, one backup, or one compromised node was a copy of the whole
installation's machine authority. The routes that carried it sat on the same
listener as the dashboard, so one credential decided both.

All of it is gone. `node_preshared_keys` is not a field of any configuration
struct, and a control plane whose file still sets it refuses to start with a
message naming the setting and what to write instead
(`ControlConfig::refuse_a_retired_fleet_key` in
`crates/deis-control/src/config.rs`). `[control] secret` is not a field of the
shared `deis_wire::ControlConfig` either, and a node has no `[management]`
section at all. The plaintext management plane on the data port is gone with
it: a management path there is now read as ordinary application traffic and
answered by the application router, which has nothing to say about a request
naming no application (`crates/deis-runtime/src/dispatcher/mod.rs`).

What survived the change is the reason the sections above are worded the way
they are. A certificate says *which* machine is calling, and the machine
channel refuses a call that asks for another machine's answer - but what an
answer contains did not narrow at the same time. See "What is not
implemented".

## The node's data port is a trust boundary

The node's data port binds `0.0.0.0:http_port`
(`crates/deis-runtime/src/dispatcher/mod.rs`). Anything that can reach it can:

- Address **any application in any environment** on that node by sending
  `x-deis-app-id` and `x-deis-environment` headers. That includes preview
  environments and applications with no route in the catalog at all: the node
  routes on the headers, and having no public address is not a defence.
- Reach the inspector WebSocket, with a token from `StartInspect`.

What it can no longer reach is the management plane. Nothing administrative is
mounted on this port: the two gRPC prefixes fall through to the wildcard and
are answered as any request naming no application is.

None of this is reachable *through* deis-proxy: it resolves an inbound
`x-deis-app-id` against the applications it knows and refuses an id that names
none of them - an `_inspect:` target names none - it drops the whole `x-deis-`
header namespace from the client and stamps the routing headers itself
(`crates/deis-proxy/src/proxy/mod.rs`). That is exactly why the node's own port
has to be unreachable as well. The proxy is the only thing enforcing rules the
node does not, so a request that arrives at the node directly is a request that
skipped all of them.

**What must sit in front of it.** Every port below must be reachable only from
the hosts that are supposed to talk to it, enforced by a firewall or a private
network - not by the absence of a published address:

| Port | Who may reach it |
|---|---|
| Node data port (`http_port`) | deis-proxy, and whoever attaches an inspector session. Nothing else, and never the internet. The control plane no longer dials it: every call it makes into a node goes to the address below. |
| Node `[machine_identity] management_listen` | The one machine this node's `[machine_identity]` names as its control plane, and nobody else this fleet has ever signed a certificate for - every other proved name is refused at the connection. |
| Control plane's human listener (`bind_addr`, `port`) | The operators' network. Never the internet. It serves the dashboard and the API behind `api_keys` and minted tokens, and a control channel that admits nobody. |
| Control plane's `[machine]` listener | Machines holding a certificate this fleet signed. This is where the assignment list, the routing table and the bundle store live, so it must never be the internet either - the certificate demand is the authentication, not the containment. |
| deis-store and deis-queue gRPC ports | Runtime nodes, the control plane and the operators' network only. Each binds loopback by default and refuses to start bound anywhere else with no `auth_secret`; the optional migration service shares the listener under its own independent token. |
| deis-telemetry's gRPC port and its Prometheus port | The same set, and both count: one serves logs and traces, the other every application's metric series. |
| deis-proxy's listeners | The internet. This is the only one that belongs there. |

A `[machine_identity]` private key is a credential like any other, whatever
shape it has. Treat every host holding one, proxy included, as inside the
control plane's trust boundary, not in a DMZ in front of it: an attacker who
can read a proxy's key gets that proxy's control connection and routing table,
and one who can read a node's key gets `FetchAssignments`, which answers with
every tenant's plaintext, not only that node's own.

## File permissions

Nothing in the platform checks the mode of a configuration file. The only mode
that is checked is the control plane's `secret_key_file`, which warns when it
is group- or world-readable (`crates/deis-control/src/catalog/secrets.rs`).
Everything below is therefore the operator's to enforce:

| File | Required |
|---|---|
| The control plane's configuration (`-c`) | `0600`, owned by the service user. It holds `api_keys`, `storage_auth_secret`, `store_migration_admin_token`, `queue_migration_admin_token` and `telemetry_migration_admin_token`, and names the `[machine]` identity files. The three migration values must be different from each other and from every other credential. |
| The control plane's `secret_key_file` | `0600`. This is the master key every secret in the database is encrypted with; the encrypted rows are worth nothing without it, and everything with it. |
| Every machine's `[machine]` / `[machine_identity]` private key, with its certificate and trust root | `0600` on the private key at least, owned by that process's service user. This file is not shared - that is the whole reason a leaked copy only ever names the one machine it belongs to - but a node's private key still opens that node's certificate identity, and a compromised node's `FetchAssignments` still answers with every environment's secrets. Issuance, renewal and revocation are outside this platform; only the mode and the ownership are this document's business. |
| Every node's configuration | `0600`, owned by the node's service user. It no longer holds a fleet credential, but it is not free of secrets: `[logging] auth_secret` is deis-telemetry's own value, and the file also names the path of this node's private key and the one control plane this node obeys - which is what decides who may deploy to it. |
| deis-proxy's configuration | `0600`, owned by the proxy's service user. Same contents, same reasoning, on the one host that faces the internet. |
| deis-store, deis-queue and deis-telemetry configurations | `0600`, owned by the service user. They hold the application-data `auth_secret` and the independent `migration_admin_token`; possession of the latter can freeze and inventory any tenant in that backend. Never reuse one value for both domains. |
| `.demo/` written by `scripts/demo.sh` | The demo writes every configuration it generates, `state.env`, `secret.key` and every private key it mints as `0600`; `scripts/test-demo-witnesses.sh` holds it to that statically. Five of those files carry a credential value - `state.env`, `secret.key` and the store, queue and control-plane configurations - and the node's and the proxy's carry a path to a key file instead. It is still a workstation tool: it puts every process under one user, so the mode separates the demo from other accounts on the host and nothing else. Do not copy a `.demo/` configuration into a real deployment. |

## Rotating the credential

There is no fleet-wide value left to rotate on a schedule, and
`node_preshared_keys` cannot be put back - a configuration that sets it is
refused at startup. What rotation means now is two different things.

**Certificates: renewal, and nothing to restart.** Every process re-reads its
three identity files on a timer - `identity_reload_seconds`, sixty seconds by
default on all three - and adopts what it finds
(`MachineIdentity::watch` in `crates/deis-identity/src/lib.rs`). Write the new
certificate and key over the old paths and the next tick picks them up:
connections made from then on use the new material and connections already
open are left alone. Both listeners are handed replacement material when the
files change rather than going on presenting what the process started with -
the control plane's machine channel
(`crates/deis-control/src/machine/listen.rs`) and a node's management plane
(`crates/deis-runtime/src/dispatcher/machine.rs`). Nothing needs a restart, and the
interval is the longest a renewal can go unnoticed, so it wants to be far
shorter than the shortest certificate lifetime in use. Three things are worth
knowing:

- A re-read that fails leaves the material already in force in use, and says
  so at `warn`. If it keeps failing until the current certificate expires,
  every connection on the machine channel starts failing at once.
- A certificate that names a *different* machine than before is adopted and
  reported at `warn`: that process has changed identity mid-run, and anything
  that authorised the previous name will refuse it until it is told.
- Revocation is not part of this. A certificate stops being accepted when it
  expires or when the trust root it chains to is replaced on the peers, and
  replacing the trust root is an operation across every machine at once.

**The values that are still shared.** Each is a file edit plus a restart of
whoever reads it, and each has its own blast radius:

1. `storage_auth_secret` must change on deis-control and on deis-store and
   deis-queue together. Every capability token in flight is derived from it,
   so all of them are invalidated at the same instant; a node holding an old
   token is refused until the control plane hands it a new one with its next
   deployment.
2. deis-telemetry's `auth_secret` must change on the collector and in every
   node's `[logging] auth_secret` together. A node with the wrong value is not
   refused at startup: it serves requests normally and every push is rejected,
   so the symptom is empty `deis logs` and `deis trace` rather than a failure.
3. `api_keys` are checked against the file, so removing an entry and
   restarting is what withdraws it. A key removed from the file is marked
   revoked in the database at the next startup, because the file is the
   authority for these. Prefer minting scoped tokens and leaving `api_keys`
   empty; the count of unscoped credentials is reported at startup and is the
   number to drive to zero.
4. The three migration admin tokens are independent of everything else by
   design. Change one without touching the others.
5. `secret_key_file` is not rotatable by editing a line: every secret in the
   database is encrypted under it.

**If a private key may have leaked**, rotating it is not enough on its own.
Work out what the name on that certificate opened - the table under "What each
proof opens" - and treat everything it reached as read. For a node's key that
means every environment's secrets, because that is what its assignment list
carries; for the control plane's key it means every node's management plane.

## What is not implemented

**Per-tenant key separation (envelope encryption) is not implemented in this
release.** Every environment's secrets are encrypted in the control plane's
database with one master key from `secret_key_file`
(`crates/deis-control/src/catalog/secrets.rs`).

**An assignment list is not scoped per node.** `assignments_for` in
`crates/deis-control/src/api/catalog/assignments.rs` takes a `node_id`, uses it
for log lines, and builds the answer from `list_assignments()` - the whole
catalog. The machine channel checks that a caller asks for its *own* id and
refuses a mismatch, so a node cannot ask for another node's list; what it gets
back for its own is still every application's decrypted secrets. The bundle
download is not scoped either: any machine that completes the handshake may
download any version of any application.

So the shape of the risk changed and its size did not. A credential that named
no machine became one that names exactly one, and cannot be copied out of a
configuration file; the answer that credential opens is still the whole
platform's plaintext. Closing that gap means scoping the assignment list to
what the identified machine actually runs, and it has not been done.

## Related

- [crates/deis-control/DEPLOYMENT.md](../crates/deis-control/DEPLOYMENT.md) -
  deploying the control plane, including the firewall rules this document sets
  the requirements for.
- [crates/deis-control/API_DOCUMENTATION.md](../crates/deis-control/API_DOCUMENTATION.md) -
  the HTTP API in detail.
- [docs/architecture.zh.md](architecture.zh.md) - the design these boundaries
  fall out of.
