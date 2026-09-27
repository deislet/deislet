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
fleet-wide value they all used to hold does not exist any more. The human API
changed with them: there is no anonymous path and no key shared through a
configuration file any more - every caller presents a token, resolved against
a hash in the control plane's database, and a control plane with no credential
at all mints a one-time bootstrap token written to a file only its own host can
read. Telemetry moved too: deis-telemetry holds no secret and accepts only
capabilities the control plane signed (see "Telemetry" below). What did not
change: `storage_auth_secret` and the matching `auth_secret` on deis-store and
deis-queue are still values copied between files, and whoever holds one holds
everything it opens. A reader who concludes that everything
moved to certificates will protect the wrong files, which is why this document
says which is which before it says anything else.

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
| deis-runtime | `[machine_identity]`: the three files, `control_endpoint` (an `https://` URL naming an explicit port), `trust_domain` or `control_identity`, `management_listen`, `identity_reload_seconds` (`crates/deis-runtime/src/config.rs`) | `[machine_identity]` is absent; or the table has neither `control_endpoint` nor `management_listen`; or `management_listen` names the same port as `data_listen`. |
| deis-proxy | `[machine_identity]`: the three files, `control_endpoint`, `trust_domain` (with `control_identity` as an override when the control plane's certificate was issued outside this naming convention) (`crates/deis-proxy/src/config/system/machine_identity.rs`) | `[control]` is present and `[machine_identity]` is not - that proxy would poll a route answering 404 for as long as it ran and route on whatever its own file said (`crates/deis-proxy/src/config/system/mod.rs`). |

## What each proof opens

Everything the platform's own processes say to each other runs on the control
plane's `[machine]` listener or on a node's `management_listen`, both of which
demand a client certificate. Two sockets, and what is on them:

**The control plane's machine listener** (`crates/deis-control/src/machine/listen.rs`)
serves the gRPC service `deis.machine.MachineChannel` - `Register`,
`Deregister`, `FetchAssignments`, `FetchRoutingTable`, `FetchSecretValues`,
`IssueCapability` - the bundle download beside it over HTTP/1.1
(`versions::BUNDLE_ROUTE`), the long-lived control channel
`deis.control.ControlService/EstablishChannel`, the signed capability
revocation list (`/capability/revocation-list`), and the capability verifying
keys as a JWKS document (`/.well-known/deis/capability-jwks.json`). The keys
are public, but whoever answers for that document decides which capabilities
deis-store, deis-queue and deis-telemetry honour, so it is served here and
nowhere else: the human listener has neither it nor the revocation list.

**A node's management listener** (`crates/deis-runtime/src/dispatcher/machine.rs`)
serves `deis.management.RuntimeManagement` and `deis.trigger.Trigger`, and
admits exactly one name: the control plane this node was configured to expect.
Every other certificate this fleet has ever signed is refused at the
connection, node and proxy certificates included.

**The data services** - deis-store, deis-queue and deis-telemetry - each hold a
machine identity of their own under `[machine_identity]`:
`spiffe://<trust domain>/deislet/store/<id>`, `.../queue/<id>` and
`.../telemetry/<id>`, issued by the same authority as every other machine
(`crates/deis-identity/src/names.rs`). A service refuses to start with a
certificate naming any other kind of machine. The identity is used twice:

* their gRPC listeners speak TLS only, presenting that certificate
  (`deis_identity::accept_tls`, `crates/deis-identity/src/service.rs`). There is
  no plaintext listener, so the bearer capability every call carries is never
  readable on the wire. The listener asks the caller for no certificate - a
  person's `deis kv` holds none - and what the caller may do is decided by the
  capability, per call. Every caller checks the far end by role: the
  certificate must chain to the fleet's trust root and name the kind of
  service it meant to reach (`VerifyServerIsOfKind`), so a queue's certificate,
  signed by the same authority, does not answer for a store, and a machine that
  merely holds a store's address gets a refused handshake instead of the
  capability. Runtime nodes and the control plane check against their own
  machine identity's root and trust domain; the CLI checks against the root
  named by `DEIS_SERVICES_CA` or `[services] ca`.
* they read the JWKS from the control plane's machine listener under mutual
  TLS, proving that certificate and requiring the control plane's name in the
  same trust domain (`deis_capability::ControlClient`). `control_endpoint` must
  be `https://`; plain `http://` is refused at startup unless it names a
  loopback address, where nothing on the network sits between the two
  processes.

Binding a capability to the caller's certificate (`cnf`) is not implemented,
so a capability copied off a node still works from anywhere that can reach the
service until it expires; the TLS above only keeps it from being read in
transit or handed to the wrong machine.

| A holder of | can do |
|---|---|
| the control plane's private key | Drive any node that has registered a name and a management address: `ApplyDeployment` (run a bundle of the caller's choosing under any tenant's identity; the request carries no secret value and no storage credential - the node fetches both itself, under its own name), `UnloadApp`, `GetStatus` (every tenant's applications and metrics on that node), `StartInspect`, `HealthCheck`, and the cron trigger `OnScheduled`. `PushConfig`, `UpdateApp` and `ExecuteCommand` exist on the wire and answer `unimplemented` (`crates/deis-runtime/src/management/service.rs`). This is the only way any node is dialled: `crates/deis-control/src/runtime_client/mod.rs` builds one channel, over mutual TLS, and a node that proved no name or reported no management address is not driven at all and says so. |
| one node's private key | `Register` and `Deregister` **its own id only** - the id in the request is held against the id in the proved name and a mismatch is refused (`crates/deis-control/src/machine/service.rs`). `FetchAssignments` for its own id only; the answer carries no secret value. `FetchSecretValues` for the secret revisions of any deployment on that list - which is **every** environment's, not that node's (see "What is not implemented") - each fetch written to the audit log before any value is returned, and refused if the row cannot be written. `IssueCapability` in its own name, 15 minutes each, naming only tenants of deployments assigned to that node - one resource outside them refuses the whole request, and the refusal is written to the audit log (`crates/deis-control/src/machine/issuance.rs`); every capability issued is recorded in the issuance table. The revocation list. Whatever it fetched is also in its sealed offline cache (`<data_dir>/offline-cache/`, AES-256-GCM under a key derived from this private key), so the key opens that file too. `FetchRoutingTable`. The bundle download for any application and any version: the handler checks nothing about who is asking (`crates/deis-control/src/api/catalog/versions.rs`, `download_bundle`). And the control channel, for a place in the session list, heartbeats and whatever operator-issued command is addressed to the id it announced. |
| one proxy's private key | `FetchRoutingTable`, and the control channel. Not `FetchAssignments`: a proxy name yields no node id, so the call is refused. The bundle download is not scoped either, so a proxy certificate reaches it too. |
| an inspector session token, minted by `StartInspect` | A Chrome DevTools Protocol WebSocket **on the node's `management_listen`**, against a running isolate: full debugger access to a tenant's running code (`crates/deis-runtime/src/dispatcher/machine.rs`). The socket it used to live on - the data port - now admits only a proxy's certificate, so the token alone no longer reaches it: the caller proves the control plane's name first. |

The control plane's human listener still carries `EstablishChannel` as well,
and it admits nobody there: that listener has no acceptor, so no connection on
it proves a name, and the handshake message says so rather than checking a
field (`crates/deis-control/src/api/grpc.rs`). The HTTP routes a node used to
call are not mounted anywhere - `deis_wire::conventions::NODE_REGISTRATION_ROUTE`,
`deis_wire::conventions::NODE_ASSIGNMENTS_ROUTE`, `/api/routing-table` and the
bundle download all answer **404** on the human listener, to every caller, with
or without a credential. Not 401: a credential cannot bring them back.

## How a person proves who they are

The human API has one way in, and it is a token. There is no anonymous path:
being a loopback peer proves nothing, coming from the dashboard's own origin
proves nothing, having arrived through deis-proxy proves nothing - the checks
an earlier release used to grant an anonymous principal are gone, and a
request with no credential is refused with 401 whatever Host it names and
whatever address it comes from (`crates/deis-control/src/api/authz.rs`). A
control plane that used to run with authentication off has no setting left to
do it with: `api_auth_enabled` and `api_keys` are not fields of the
configuration any more, and a configuration file that still carries them is
refused at startup like any unknown field. Development, the demo and a
production installation take the same code path from the first request on -
there is no dev branch of the authentication, and a build labelled as not
production-ready wears that as a label, not as an exception.

What is new on a first start is how the first token comes into existence. A
control plane whose database holds no live credential at all mints a one-time
bootstrap token and writes it to `<data_dir>/bootstrap.token`, mode `0600`,
created exclusively - a file that already exists is never overwritten
(`crates/deis-control/src/identity/store.rs`). The startup log names the file
and the token's id, never the value. The bootstrap token can do exactly one
thing: create the first owner account and a first token for it. The moment
that succeeds the bootstrap token is revoked and the file is deleted; if it is
never used it expires on its own, and the next restart mints a fresh one.
`deis login --bootstrap <path>` is the way in, and on the same host it finds
the file at its default path by itself.

A headless install presets its own secret instead: `deis-control bootstrap
--token-file <path>` installs the hash of the token the operator generated
offline, and the same file logs in. Any previous bootstrap row stops working
when a new one is installed - there is one way in, and it is the one just
installed.

## What is still a shared secret

None of these moved to certificates, and each is a value copied into more than
one file.

| Value | Held by | What holding it is worth |
|---|---|---|
| `storage_auth_secret` on deis-control, the identical `auth_secret` on deis-store and deis-queue | three processes, one value | The control plane mints `base64(HMAC-SHA256(auth_secret, tenant))` for each tenant and writes it into that tenant's deployment; the backend recomputes the expected token from the tenant the call itself claims (`crates/deis-store/src/auth.rs`, `crates/deis-control/src/delivery/mod.rs`). The secret mints a token for **any** tenant, so holding it is holding every tenant's data in both backends. A token does not expire; the only lever is changing the secret, which invalidates every token at once. |
| `store_migration_admin_token`, `queue_migration_admin_token`, `telemetry_migration_admin_token` | deis-control and the backend it names | Freeze and inventory any tenant in that backend. Each is independent of the others and must not equal that backend's `auth_secret` - deis-store refuses to start its management service if it does (`crates/deis-store/src/server.rs`). |
| the master key in `secret_key_file` | deis-control alone | Not shared between machines, but one key for every tenant: every environment's secrets in the database are encrypted with it (`crates/deis-control/src/catalog/secrets.rs`). The encrypted rows are worth nothing without it and everything with it. |

## Telemetry

deis-telemetry has no secret of its own and no mode without the check. Every
push, query, follow stream and Prometheus scrape presents a capability with
audience `telemetry`, verified against the keys the control plane publishes
at `/.well-known/deis/capability-jwks.json` (`capability_jwks_url`), with the
same 15-minute ceiling, revocation list and T8 grace as deis-store and
deis-queue (`crates/deis-telemetry/src/auth.rs`). A capability's resources say
what it may touch, one (tenant, binding) pair each:

| Binding | Issued to | What it allows |
|---|---|---|
| `ingest` for `<app>.<env>` | a node, over the machine channel (`IssueCapability`), for each application environment it runs | Push that tenant's logs, traces and metric samples. A record for any tenant the capability does not name is refused and counted as rejected. It reads nothing. |
| `ingest` for `@platform` | every node | Push the node's own records - the ones with no application (process metrics, component log lines). |
| `read` for `<app>.<env>` | a person, from `GET /api/v1/apps/{app}/environments/{env}/telemetry-token` (needs `read:{app}/{env}`) or `GET /api/v1/apps/{app}/telemetry-token` (needs `read:{app}/*`, every environment) | Query and follow that tenant's records and nothing else: a read must name an application the capability reaches, and whatever comes back is filtered to the tenants it names. It writes nothing. |
| `read` for `@platform` | a person holding `read:*/*`, from `GET /api/platform/telemetry-token` | Read everything, every tenant and the nodes' own records. The Prometheus page is every series at once, so it accepts only this. |

Every mint on the human side is recorded in the audit log whatever its
outcome (`telemetry_token.mint`, `telemetry_token.mint_platform`), like the
storage-token mint. What a compromised node can do here is what it can do
with its capability: write the tenants it runs and its own records, and read
nothing. A Prometheus server scraping the page needs a fresh platform read
capability at least every 15 minutes (a `bearer_token_file` kept current by
something holding `read:*/*`); there is no long-lived scrape credential.

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

## What the platform assumes about the code it runs

Deislet assumes that the application code within one installation is trusted;
the V8 isolate is resource isolation, not a security sandbox. The same
sentence appears in the README and in
[ADR-018](adr/018-platform-support-boundary.md) on purpose: it is the trust
model, not a caveat, and it is what decides which attacks this platform tries
to stop. The platform defends against resource exhaustion and misuse - an
application that allocates without bound, spins without ending, or calls an
endpoint it was never pointed at - and every limit in this document exists for
that. It does not defend against an application that sets out to escape its
isolate, and a hostile tenant should not be hosted under this assumption. The
corollary is stated under "What is not implemented": every node runs every
application and holds every application's secrets, which is a property for an
installation whose applications share an owner and a hole for one whose
tenants do not trust each other.

## The node's data port is a trust boundary

The node's data port binds `data_listen`
(`crates/deis-runtime/src/dispatcher/mod.rs`) and admits a caller only over
mutual TLS: the node presents the leaf from its `[machine_identity]`, the
caller must present a certificate that chains to the same trust root and
names a **proxy** of this installation (`deislet/proxy/...` in the URI
subject alternative name), and a handshake that proves anything else - or
nothing at all - is closed before a byte of any request is read. The demo's
three witnesses for this are `check_the_data_port_admits_a_fleet_proxy_and_nothing_else`
(`scripts/demo/checks/accounts.sh`), and the handshakes themselves are tested
end to end in `crates/deis-runtime/tests/management_auth_test.rs`.

What a caller with a proxy certificate can then reach:

- Address **any application in any environment** on that node by sending
  `x-deis-app-id` and `x-deis-environment` headers. That includes preview
  environments and applications with no route in the catalog at all: the node
  routes on the headers, and having no public address is not a defence.
- Reach the inspector WebSocket - not here any more: it moved to the
  `[machine_identity] management_listen`, where the caller has already proved
  the control plane's name.

What it can no longer reach is the management plane. Nothing administrative is
mounted on this port: the two gRPC prefixes fall through to the wildcard and
are answered as any request naming no application is.

The certificate demand is the whole of the story now, and the stopgap it
deleted is worth naming because it shaped the fetch policy below. When this
port answered plaintext HTTP, any process on the machine - this node's own
applications included - could have opened it with routing headers of its own
choosing; the port's containment was the firewall plus the node's refusal of
its own addresses. The refusal stays. It is no longer the only thing between
a tenant and another tenant's code - the handshake is - but an application
that could reach the port at all should still be refused before the
conversation starts, and the fetch policy below is what does that.

A proxy certificate in hand is not a small thing: whoever holds one can
address every application on the node exactly as the real proxy can. That is
why the trust boundary does not move - the port still must not be reachable
from the internet, and a leaked proxy key is handled the same way as a leaked
node key, under the rule at the end of this section.

None of this is reachable *through* deis-proxy. The edge routes on the `Host`
header and the path alone: the client's whole `x-deis-` header namespace is
deleted at the very top of the request filter, before any routing input is
read, and what was deleted is counted as forged, logged at debug and measured
(`crates/deis-proxy/src/proxy/mod.rs`). A header does not name an application
any more - there is no second routing entry left, so a request carrying
`x-deis-app-id: other` for host A resolves A, and a host this edge has no
table for is refused whatever headers accompany it. The node's `_system`
application and its `_inspect:*` debugger channel were reachable only by
header, so they are unreachable from the edge by name. That is exactly why the
node's own port has to be unreachable as well. The proxy is the only thing
enforcing rules the node does not, so a request that arrives at the node
directly is a request that skipped all of them.

**What must sit in front of it.** Every port below must be reachable only from
the hosts that are supposed to talk to it, enforced by a firewall or a private
network - not by the absence of a published address:

| Port | Who may reach it |
|---|---|
| Node data port (`data_listen`, mutual TLS) | deis-proxy, and whoever attaches an inspector session over a certificate naming this installation's control plane. Every caller proves a proxy's name in the handshake; a plaintext caller, a caller with no certificate, one naming another machine, and one signed by an outside authority all get a closed connection. The control plane no longer dials it: every call it makes into a node goes to the address below. |
| Node `[machine_identity] management_listen` | The one machine this node's `[machine_identity]` names as its control plane, and nobody else this fleet has ever signed a certificate for - every other proved name is refused at the connection. |
| Control plane's human listener (`bind_addr`, `port`) | The operators' network. Never the internet. It serves the dashboard and the API - every caller presents a token, and there is no anonymous path - plus a control channel that admits nobody. |
| Control plane's `[machine]` listener | Machines holding a certificate this fleet signed. This is where the assignment list, the routing table and the bundle store live, so it must never be the internet either - the certificate demand is the authentication, not the containment. |
| deis-store and deis-queue gRPC ports (TLS only) | Runtime nodes, the control plane and the operators' network only. Each binds loopback by default; every call carries a capability whatever the address, and a plaintext caller gets no answer. The optional migration service shares the listener under its own independent token. |
| deis-telemetry's gRPC port (TLS only) and its Prometheus port | The same set, and both count: one serves logs and traces, the other every application's metric series. Every call on either presents a control-signed capability, and neither has a mode without that check. The Prometheus port is still plain HTTP, so the bearer a scraper sends is readable on the way: keep it on loopback or a network only the scraper reaches. |
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
| The control plane's configuration (`-c`) | `0600`, owned by the service user. It holds `storage_auth_secret`, `store_migration_admin_token`, `queue_migration_admin_token` and `telemetry_migration_admin_token`, and names the `[machine]` identity files. The three migration values must be different from each other and from every other credential. |
| The control plane's `secret_key_file` | `0600`. This is the master key every secret in the database is encrypted with; the encrypted rows are worth nothing without it, and everything with it. |
| The control plane's `<data_dir>/bootstrap.token` | `0600`, written by the control plane itself on a first start and deleted the moment the bootstrap token is used. Until then it is an operator credential - it exists to create the first owner account and nothing else - so treat it like one and do not let it out of the host. It is not part of a backup of the data directory: it is short-lived by design, and backing one up means carrying an unused way in. |
| Every machine's `[machine]` / `[machine_identity]` private key, with its certificate and trust root | `0600` on the private key at least, owned by that process's service user. This file is not shared - that is the whole reason a leaked copy only ever names the one machine it belongs to - but a node's private key still opens that node's certificate identity, and a compromised node's `FetchSecretValues` still answers for every environment's secrets. Issuance, renewal and revocation are outside this platform; only the mode and the ownership are this document's business. |
| Every node's configuration | `0600`, owned by the node's service user. It holds no shared credential - its telemetry capability is issued by the control plane at run time - but the file names the path of this node's private key and the one control plane this node obeys - which is what decides who may deploy to it. |
| deis-proxy's configuration | `0600`, owned by the proxy's service user. Same contents, same reasoning, on the one host that faces the internet. |
| deis-store, deis-queue and deis-telemetry configurations | `0600`, owned by the service user. They hold the independent `migration_admin_token` - possession of it can freeze and inventory any tenant in that backend - and name the service's `[machine_identity]` files. The private key is `0600` like every other machine's: whoever holds a store's key can answer for a store and collect the capabilities sent to it. |
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

- **Expiry is announced, not discovered.** Nothing here issues or renews a
  certificate, so forgetting to renew one would otherwise be invisible right
  up to the instant it expires - and then every connection on the machine
  channel fails at the same moment, for a reason none of the processes would
  name. So each process logs, at startup and on every reload tick, how long
  its own certificate has left
  (`MachineIdentity::announce_expiry` / `report_expiry`). The threshold is a
  fraction of the certificate's own lifetime - `warn` at four fifths spent,
  `error` at nineteen twentieths - rather than a number of days, because this
  platform is handed both four-day certificates and multi-year ones, and a
  fixed threshold would either say nothing useful on one or fire on every
  correct startup on the other. A warning that fires when nothing is wrong is
  how an operator learns to skip the one that matters. One absolute rule sits
  beside the fractions: under 48 hours left always warns, because a 24 hour
  control-plane outage starting then could outlast the certificate.
- **A certificate must be issued for at least 72 hours.** A machine rides out
  a control-plane outage of up to 24 hours on what it already holds, and
  nothing can renew its certificate while the control plane is down. A
  certificate issued for less (`not_after - not_before < 72h`) is refused when
  it is loaded (`IdentityError::LifetimeTooShort`, checked in
  `crates/deis-identity/src/material.rs`) - at startup, and on a rotation,
  where the material already in force stays in use.
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

1. Store and queue capabilities are not derived from a shared value any
   more: each is signed by the control plane's issuer key, lives fifteen
   minutes and is recorded in the issuance table. Withdrawing them is a call,
   not a file edit - see "Taking capabilities back" below.
2. Human credentials are tokens, resolved against hashes in the control
   plane's database, so there is no file-held key to rotate: withdrawing one
   is `deis token revoke`. An unused bootstrap token expires on its own, and a
   used one is already revoked. The count of unscoped credentials is reported
   at startup and is the number to drive to zero.
3. The three migration admin tokens are independent of everything else by
   design. Change one without touching the others.
4. `secret_key_file` is not rotatable by editing a line: every secret in the
   database is encrypted under it.

## Taking capabilities back

Every capability the control plane signs - for a node through
`IssueCapability`, for an operator through `.../storage-token`, for its own
calls to deis-queue - is written to the issuance table
(`capability_issuances`: `jti`, subject, node id, audience, `kid`, epoch,
issue and expiry time, and every tenant and binding it grants) before it is
handed out, and not handed out if it cannot be (`crates/deis-control/src/capability_ledger.rs`).
An administrator can revoke from that record, and every call is in the audit
log:

| Call | Revokes |
|---|---|
| `DELETE /api/v1/capabilities/{jti}` (`capability.revoke`) | one capability |
| `POST /api/v1/capabilities/revocations/nodes/{node_id}` (`capability.revoke_node`) | every unexpired capability issued to that node |
| `POST /api/v1/apps/{app}/environments/{env}/capability-revocations` (`capability.revoke_tenant`) | every unexpired capability naming that tenant, whoever holds it |
| `POST /api/v1/capabilities/auth-epoch` (`capability.auth_epoch.bump`) | every capability issued so far: the epoch floor rises |

Verifiers learn of it from the revocation list: every revoked `jti` that
could still be accepted, and the current auth epoch, **signed with the issuer
key** in the capability wire format and served on the machine listener only.
deis-store (for kv, sql and blob alike - one verifier behind all three),
deis-queue and deis-telemetry poll it every 30 seconds, check the signature
against the same JWKS capabilities are checked against, and refuse a list
that is older than two minutes, from the future, or older than the one they
last adopted - so a list edited on the way, or an old one replayed, changes
nothing. A revocation is therefore in force within one poll.

Two things a revocation does not do. It does not stop a node asking again:
a node still in the fleet and still running the tenant is issued a new
capability on its next renewal - to shut a machine out, remove it from the
fleet and stop trusting its certificate. And it does not reach a verifier cut
off from the control plane: in T8 degraded mode a verifier enforces the last
list it adopted.

**T8 degraded mode.** A verifier that has not adopted a list for two poll
intervals is degraded. Only then, and only for a capability whose subject is
a runtime node's SPIFFE name (`spiffe://<trust domain>/deislet/node/<id>`,
parsed by `deis-identity`), does it accept an expired capability - and only
if it was still valid (give or take 30 seconds) when the last list arrived,
is at most 24 hours past its expiry, and is not revoked by that list. Human
capabilities never get grace. Each verifier exports `deis_auth_degraded`,
`deis_auth_degraded_since_seconds`, `deis_auth_grace_accepted_total` and
`deis_auth_revocation_last_sync_seconds` - deis-telemetry on its `/metrics`,
deis-store and deis-queue on `metrics_listen` when it is set.

A verifier **restarted** while the control plane is unreachable has adopted
no list and holds no keys (the JWKS is not persisted either), so it refuses
every capability until the control plane is back: it reports itself degraded
but has nothing to extend grace from. This is deliberate. Persisting the time
of the last sync alone would let such a verifier accept expired capabilities
without the revocations that came with that sync.

**If a private key may have leaked**, rotating it is not enough on its own.
Work out what the name on that certificate opened - the table under "What each
proof opens" - and treat everything it reached as read. For a node's key that
means every environment's secrets, because its assignment list lets it fetch
all of them, and its offline cache already holds the ones it fetched; for the control plane's key it means every node's management plane.

## What is not implemented

**Per-tenant key separation (envelope encryption) is not implemented in this
release.** Every environment's secrets are encrypted in the control plane's
database with one master key from `secret_key_file`
(`crates/deis-control/src/catalog/secrets.rs`).

**Every node runs every application, so every node holds every
application's secrets.** This is worth stating carefully, because the code
looks like a missing filter and is not one.

`assignments_for` in `crates/deis-control/src/api/catalog/assignments.rs`
takes a `node_id`, uses it for log lines, and builds the answer from
`list_assignments()` - the whole catalog. The machine channel does check that a
caller asks for its *own* id and refuses a mismatch, so no node can ask for
another node's list. But the list it gets for itself carries every
application's decrypted secrets, and adding a filter there would change
nothing: a deployment plan's `target_ids` comes from
`Deliverer::serving_target_ids` (`crates/deis-control/src/delivery/mod.rs`),
which is every node the control plane knows of. There is no placement model in
this release - no way to say that an application runs on some nodes and not
others - so there is nothing for a filter to filter by. The bundle download is
unscoped for the same reason: any machine that completes the handshake may
download any version of any application.

What a node can obtain on its own initiative is narrower than what it is
handed. `FetchSecretValues` and `IssueCapability` both go through
`deployments_visible_to` in the same file, the one function a placement model
will narrow: today it admits every current assignment to a node that is in the
fleet (registered with the name it proved, or configured) and nothing to one
that is not, plus in-flight plans that name the node as a target. A node can
therefore not mint a capability for a tenant nothing deployed, and a machine
holding a certificate this fleet signed but not in the fleet gets no
capability and no secret at all.

So the shape of the risk changed and its size did not. A credential that named
no machine became one that names exactly one and cannot be copied out of a
configuration file; the answer that credential opens is still the whole
platform's plaintext.

**What this means for whether the platform is safe to run.** For an
installation whose applications all belong to one organisation, this is a
property and not a hole: every node is trusted with everything because
everything is the same owner's. For an installation hosting tenants who do not
trust each other, it is a hole, and closing it is not a patch to
`assignments_for` - it needs a placement model first (which nodes may run this
application), and then the scoping follows from it. See
[ADR-025](adr/025-machine-identity-and-capabilities.md).

## Related

- [crates/deis-control/DEPLOYMENT.md](../crates/deis-control/DEPLOYMENT.md) -
  deploying the control plane, including the firewall rules this document sets
  the requirements for.
- [crates/deis-control/API_DOCUMENTATION.md](../crates/deis-control/API_DOCUMENTATION.md) -
  the HTTP API in detail.
- [docs/architecture.zh.md](architecture.zh.md) - the design these boundaries
  fall out of.
