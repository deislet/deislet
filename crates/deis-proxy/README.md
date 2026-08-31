# Proxy

Managed multi-site reverse proxy and load balancer for the Deislet platform.

Proxy is the traffic entry point: it terminates HTTP/1.1, HTTP/2 and TLS,
decides which application a request belongs to, and forwards it to a
[deis-runtime](../deis-runtime) node from a shared, health-checked upstream pool. Its
configuration is layered from a local file and from the control
([deis-control](../deis-control)), and the binary can be upgraded without dropping
connections.

The data plane is [Pingora](https://github.com/cloudflare/pingora). Proxy
adds application routing, configuration management and the control
integration on top of it; it does not implement its own HTTP parsing,
socket handling or process model.

## Request flow

1. **Resolve the application**, from two sources with a fixed precedence.
   The **catalog's routing table**, polled from the control plane, is the
   one an operator can change without touching a machine, so where both
   can answer "which application", the catalog wins. The **local
   application list** is still here and still matters, for a different
   question: certificates, the cache policy, the enabled flag — what to do
   with a request once the application is known.

   A non-empty `x-deis-app-id` header names the application and nothing
   else, so it is answered first: the local list, then the catalog's
   default environment for that id. Without the header, the catalog is
   matched on host **and path prefix** — exact host, then `*.wildcard`
   (longest suffix first, so `*.eu.example.com` beats `*.example.com`),
   then an automatic per-environment address, then a route naming no host
   at all, which the catalog means as "any domain". Within one host,
   candidates are ordered by `priority` and then by prefix length, so
   `/api/v2` beats `/api` beats `/` and an operator can still override
   that. If the catalog claims nothing, the local list is matched on host.
   When neither the local list nor the catalog claims any host, every
   request is forwarded unchanged, so a bare instance is a plain reverse
   proxy; once either holds something, an unmatched request is a `404`.
2. **Apply policy.** Disabled applications get a `403`. Plain HTTP requests
   for an application whose certificate is loaded (or that has a
   certificate configured and `tls.default_certificate` to fall back on)
   are redirected to HTTPS when `https.auto_redirect_to_https` is set. An
   application whose certificate failed to load stays reachable over plain
   HTTP instead of being redirected to a handshake that cannot succeed.
3. **Pick a backend** from the upstream pool using the configured strategy
   and forward the request. `x-deis-app-id` is set to the id deis-proxy decided
   on, replacing whatever the client sent, so deis-runtime only sees ids that
   passed the checks above. With `https.auto_offload_https = false` the
   upstream leg keeps TLS.

Because the header is validated, deis-runtime's own targets such as
`_inspect:<app>` and `_system` cannot be reached through deis-proxy; the
inspector is meant to be used on deis-runtime's own port.

A request body larger than `max_request_body_bytes` is refused rather than
carried. A connection that has **completed** an upgrade is exempt, because
its frames are a stream and not a body — the test is whether the node
actually answered `101`, not whether the client asked for an upgrade. That
distinction is the fix for a real hole: while it asked the request headers
instead, an ordinary chunked upload that named an `Upgrade` it never
completed was carried to the node unbounded.

TLS certificates belong to applications. During the handshake the SNI name
is looked up in the route table and that application's certificate is
presented; `tls.default_certificate` covers names no application claims.

## Configuration

See [`config.toml.example`](config.toml.example) for every key. Values are
layered: built-in baseline, then the file given with `-c`, then whatever
the control pushes. Notable behaviour:

- `balancing_strategy` (`round_robin`, `random`, `ip_hash`,
  `header_hash(<Header>)`) is fixed for the lifetime of the process. The
  `upstreams` list is re-read on every `health_check_interval_secs` tick,
  so backends can be added and removed without a restart.
- `listen_ports` are plain HTTP, `tls.ports` terminate TLS; a port may not
  appear in both.
- `log.access_log` writes one line per request in the field order given by
  `format`; `log.error_log.path` redirects diagnostics to a file.
  `RUST_LOG` overrides the configured level.
- Relative paths (`database`, `pid_file`, `upgrade_sock`, log paths,
  certificate files) are resolved against the directory deis-proxy was started
  from, and a `reload` starts the replacement from that same directory, so
  they keep resolving the same way across upgrades. Certificate paths in
  application configurations pushed by the control are used as given;
  make them absolute.
- `[cache]` is parsed and validated but not applied to traffic yet: nothing
  implements `request_cache_filter` and pingora is built without its `cache`
  feature, so no response is stored and no `X-Cache-Status` is emitted.
- `[routing]` is where the catalog's routing table is configured:
  `control_poll_interval_secs` (5 by default), `max_table_bytes`, and
  `preview_zone`. The poll uses the `[control]` address and secret. Leaving
  `preview_zone` unset turns automatic addresses off entirely; setting it to,
  say, `preview.example.com` puts every environment at
  `<env>--<app>.preview.example.com`, with the default environment also at
  `<app>.preview.example.com`. The separator is `--` inside one DNS label
  rather than a dot because a wildcard certificate matches exactly one label:
  `*.preview.example.com` covers `pr-42--shop.preview.example.com` and would
  not cover `pr-42.shop.preview.example.com`. Point a wildcard DNS record at
  this instance and give it a certificate for the zone, or those names resolve
  to nothing and the handshake fails.
- **A failed poll changes nothing.** The last table this process accepted keeps
  routing indefinitely, because a data plane that starts refusing traffic when
  its control plane is unreachable turns a management outage into a
  customer-visible one. Each failure logs at `ERROR` saying which table is
  still routing and how old it is. An instance that has never once fetched the
  table has nothing to keep and falls back to the locally configured
  applications.

Applications (`id`, `description`, `domains`, `certificates`, `enabled`,
`cache`) are not part of the file. They are pushed by the control and persisted in `database`
(SQLite), so they survive restarts.

### Control protocol

Proxy registers with deis-control over deis-wire and then accepts:

| Push | Effect |
|---|---|
| `ConfigUpdate { key = "apps" }` | Replace the whole application list with the JSON array in `value_json`. |
| `ConfigUpdate { key = "app.<id>" }` | Insert or update one application. |
| `ConfigUpdate { key = "system" }` | Layer a traffic-policy overlay onto the system configuration (listeners, daemon settings, logging and the control connection are ignored). |
| Command `shutdown` | Stop accepting connections, exit when in-flight requests finish. |
| Command `restart` / `reload` | Zero-downtime upgrade (see below). |
| Command `get_status` | Answer with instance state and uptime. |

`get_metrics` is answered with an explicit "not implemented".

If the control is unreachable, at startup or later, deis-proxy keeps
serving traffic and keeps retrying the connection (1 s doubling to 30 s
between attempts) until it succeeds or the process stops; a rejected secret
is logged at ERROR on every attempt.

## Running

```bash
deis-proxy -c /etc/deis-proxy/config.toml        # foreground
deis-proxy -c /etc/deis-proxy/config.toml -d     # daemon, writes pid_file
deis-proxy -c /etc/deis-proxy/config.toml -t     # check the configuration and exit
deis-proxy -c /etc/deis-proxy/config.toml -s reload
```

In daemon mode set `log.error_log.path`: pingora detaches stderr, so
otherwise nothing after startup is logged. `user`/`group` make pingora
switch identity *before* it binds any listener, which requires starting as
root and ports the target user may bind; the recommended way to run
unprivileged is to leave them unset and let the service manager do it
(systemd `User=` with `AmbientCapabilities=CAP_NET_BIND_SERVICE`). A
listener that cannot be bound terminates the process rather than leaving a
daemon running without one.

`-s` signals the daemon named by `pid_file`:

| Signal | Effect |
|---|---|
| `stop` | Exit immediately. |
| `quit` | Stop accepting connections, exit when in-flight requests finish. |
| `reload` | Zero-downtime upgrade. |

### Zero-downtime upgrade

`reload` (or the control's `restart`) starts a second `deis-proxy --upgrade`
process with the same configuration and from the same working directory,
waits for it to report that its startup succeeded (configuration, database,
logs and certificates loaded), then sends `SIGQUIT` to the running one.
Pingora hands the listening sockets over the `upgrade_sock` Unix socket; the
new process starts accepting on them while the old one finishes its
in-flight requests and exits. No connection is refused in between. If the
replacement exits before it is ready, the running process logs the exit
status and keeps serving.

`scripts/test_graceful_restart.sh` at the repository root exercises this
end to end against a local backend while keeping requests flowing, in the
foreground and as a daemon started with relative paths.

## Development

```bash
cargo test -p deis-proxy
cargo clippy -p deis-proxy --all-targets
../../scripts/test_graceful_restart.sh
../../scripts/demo.sh start              # full stack: deis-control + deis-runtime + deis-proxy
```

`src/control/mock.rs` is an in-process control that the client tests
register against, so retry and reconnect behaviour is covered without deis-control.

## Layout

| Path | Contents |
|---|---|
| `src/main.rs` | Builds the pingora server from the configuration. |
| `src/proxy/` | `ProxyHttp` implementation, upstream pool, access log. |
| `src/tls.rs` | Certificate loading and SNI selection. |
| `src/config/` | Configuration model, layering, persistence, route table. |
| `src/catalog/` | The control plane's routing table: the poller, the wire shape, and the compiled table a request is matched against. |
| `src/control/` | Control client and its background service. |
| `src/restart.rs` | Upgrade and shutdown via pingora's signals. |
| `src/cli.rs` | Command line parsing. |
