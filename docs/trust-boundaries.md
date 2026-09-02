# Trust boundaries and the fleet credential

This is the one place that states what the node credential opens, who holds
it, which ports have to be kept off the public internet, and how to rotate the
value. Everything else that mentions `node_preshared_keys` or `[control]
secret` points here rather than restating it, because the danger in this
credential is not in any one of its uses - it is in the join, and a reader who
meets the uses one at a time never sees it.

It describes the platform as it is in this release. Where something is not
implemented it says so; a plan is not a boundary.

## One credential, whole fleet

`node_preshared_keys` in the control plane's configuration file is a **list of
equivalent values**, not a set of per-node identities. Every value in the list
is accepted on every node-facing surface, and no surface asks which node is
calling:

- `crates/deis-control/src/api/middleware.rs` - the HTTP node endpoints accept
  any value in the list, and `api_auth_enabled` has no say over them.
- `crates/deis-control/src/handler.rs` and
  `crates/deis-control/src/api/grpc.rs` - the node gRPC channel checks the
  same list.
- `crates/deis-control/src/config.rs`, `node_call_credential` - the **first**
  non-empty value is the one this control plane presents when it drives a
  node. The rest are accepted, not presented; the list is therefore ordered,
  which is what makes the rotation below work.

So there is no such thing as "one node's key". There is the fleet credential,
and a holder of any one of its values.

## What one value opens

| Surface | What the holder can do |
|---|---|
| `deis_wire::conventions::NODE_ASSIGNMENTS_ROUTE` on the control plane | Read the assignment list. It carries **every environment's decrypted secret values** and **every tenant's storage capability token**. The `:id` in the path is used for log lines only - it is never checked against a registered node, and the list is identical for every node - so any id works, including one that does not exist. (`crates/deis-control/src/api/catalog/assignments.rs`) |
| `deis_wire::conventions::NODE_REGISTRATION_ROUTE` on the control plane | Register a machine of the caller's choosing as a node, or deregister a real one. A registered node is a delivery target and a cron candidate. |
| The bundle download on the control plane | Download any version of any application's code. |
| `/api/routing-table` on the control plane | Read the whole installation's routes: hosts, path prefixes, app ids, environment names. |
| The node gRPC control channel | Attach to the control plane as a node. |
| `deis.management.RuntimeManagement` on **every** node | `ApplyDeployment` (run a bundle of the caller's choosing under any tenant's identity), `UnloadApp`, `UpdateApp`, `PushConfig`, `ExecuteCommand`, `GetStatus` (every tenant's applications and metrics on that node), and `StartInspect`. (`crates/deis-runtime/src/dispatcher/auth.rs`) |
| `deis.trigger.Trigger` on **every** node | Fire queue and cron handlers. |
| The inspector | `StartInspect` mints a session token; that token opens a Chrome DevTools Protocol WebSocket **on the node's data port** against a running isolate. Full debugger access to a tenant's running code. (`crates/deis-runtime/src/dispatcher/handlers.rs`) |

Read as one sentence: **the credential equals every tenant's plaintext**, plus
administrative control of every node in the fleet.

## Who holds it

Three kinds of file carry the value, and each one is as dangerous as the
control plane's own:

1. **The control plane's configuration** - `node_preshared_keys`.
2. **Every node's configuration** - `[control] secret`, plus anything in
   `[management] keys` during a rotation.
3. **deis-proxy's configuration** - `[control] secret`. deis-proxy is not a
   runtime node; it holds a node credential because its catalog poll and its
   gRPC control channel authenticate with one.
   (`crates/deis-proxy/src/catalog/client.rs`)

The third one is the uncomfortable one. deis-proxy is the internet-facing
process. Its credential is often described by what it is *used for* - the
routing table, which carries nothing secret - and that description is only
half the truth: the same bearer value opens the assignment list, the bundle
download and every node's management plane. **Compromise of the deis-proxy
process, or read access to its configuration file, is compromise of every
tenant's secrets and of every node.** Treat the proxy host as being inside the
control plane's trust boundary, not in a DMZ in front of it.

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
| Node management port, if `[management] listen` moved it | The control plane only. |
| Control plane HTTP/gRPC port | Nodes, deis-proxy, and the operators' network. Never the internet - it is where the assignment list lives. |
| deis-proxy's listeners | The internet. This is the only one that belongs there. |

Setting `[management] listen` to a separate address takes the management and
trigger services off the data port entirely - their paths then fall through to
the application router and answer as any request naming no application does.
That is worth doing, and it is not a substitute for the firewall: the data
port still addresses every application and every environment on the node, and
it still carries the inspector.

`[management] listen` on a loopback address with no key configured is the
single-box case, and it stays open on purpose - the address is the
containment. Any other address with no key is shut rather than open.

## File permissions

Nothing in the platform checks the mode of a configuration file. The only mode
that is checked is the control plane's `secret_key_file`, which warns when it
is group- or world-readable
(`crates/deis-control/src/catalog/secrets.rs`). Everything below is therefore
the operator's to enforce:

| File | Required |
|---|---|
| The control plane's configuration (`-c`) | `0600`, owned by the service user. It holds `node_preshared_keys`, `api_keys` and `storage_auth_secret`. |
| The control plane's `secret_key_file` | `0600`. This is the master key every secret in the database is encrypted with; the encrypted rows are worth nothing without it, and everything with it. |
| Every node's configuration | `0600`, owned by the node's service user. It holds `[control] secret`. |
| deis-proxy's configuration | `0600`, owned by the proxy's service user. Same value, same consequences. |
| `.demo/*.toml` written by `scripts/demo.sh` | The demo writes every file it generates that carries a credential - `state.env`, `secret.key` and all six service configurations - as `0600`. It is still a workstation tool: it puts every process under one user, so the mode separates the demo from other accounts on the host and nothing else. Do not copy a `.demo/` configuration into a real deployment. |

## Rotating the credential

The code supports rotation without an outage, because the control plane
accepts every value in its list while presenting only the first, and a node
accepts `[control] secret` plus everything in `[management] keys`
(`crates/deis-runtime/src/config.rs`, `management_keys`). No process reloads
its configuration on a signal, so every step below is a restart of the
processes named in it - and only of those.

1. **Teach every node the new value.** Add it to `[management] keys` on each
   node and restart the nodes one at a time. Each node now answers to both the
   old and the new value; the control plane is still presenting the old one,
   so nothing is interrupted.
2. **Put the new value first in the control plane's `node_preshared_keys`,
   keeping the old one behind it**, and restart the control plane. The control
   plane now presents the new value on its pushes, and still accepts the old
   one on the pull side, so a node that has not been moved yet keeps fetching
   assignments and bundles.
3. **Move each node's `[control] secret` to the new value** and drop the old
   one from that node's `[management] keys`. Restart the nodes one at a time.
4. **Move deis-proxy's `[control] secret`** to the new value and run
   `deis-proxy -s reload` (or the control plane's `restart` command). The
   replacement process reads the file and takes the listening sockets over, so
   this step drops no connection.
5. **Remove the old value from `node_preshared_keys`** and restart the control
   plane. The old value now opens nothing.

Restarting nodes one at a time is what keeps traffic up: deis-proxy
health-checks its upstreams and takes a node out of rotation while it is down.
Restarting the control plane does not interrupt traffic at all - a node serves
what it already has when the control plane is unreachable.

**If a value may have leaked, the rotation above is not enough on its own.**
Whoever held it has had the assignment list, which means every secret in every
environment. Rotate the application secrets too, and assume any bundle on the
platform was readable.

## What is not implemented

**Per-tenant key separation (envelope encryption) is not implemented in this
release.** Every environment's secrets are encrypted in the control plane's
database with one master key from `secret_key_file`
(`crates/deis-control/src/catalog/secrets.rs`), and the assignment list hands
them out decrypted to any caller holding a node credential. There is no
per-tenant data key, no per-node scoping of the assignment list, and no
identity in the node credential beyond "the holder may act as a node".

The consequence is the sentence at the top of this document, and it is the
reason the boundaries above are boundaries rather than defence in depth:
one credential, one master key, and every tenant behind both.

## Related

- [crates/deis-control/DEPLOYMENT.md](../crates/deis-control/DEPLOYMENT.md) -
  deploying the control plane, including the firewall rules this document sets
  the requirements for.
- [crates/deis-control/API_DOCUMENTATION.md](../crates/deis-control/API_DOCUMENTATION.md) -
  the node endpoints in detail.
- [docs/architecture.zh.md](architecture.zh.md) - the design these boundaries
  fall out of.
