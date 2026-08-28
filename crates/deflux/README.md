# Deflux

Managed multi-site reverse proxy and load balancer for the Deislet platform.

Deflux is the traffic entry point: it terminates HTTP/1.1, HTTP/2 and TLS,
decides which application a request belongs to, and forwards it to a
[denix](../denix) node from a shared, health-checked upstream pool. Its
configuration is layered from a local file and from the supervisor
([denira](../denira)), and the binary can be upgraded without dropping
connections.

The data plane is [Pingora](https://github.com/cloudflare/pingora). Deflux
adds application routing, configuration management and the supervisor
integration on top of it; it does not implement its own HTTP parsing,
socket handling or process model.

## Request flow

1. **Resolve the application.** A non-empty `x-deis-app-id` header names
   the application and must match a configured id exactly; an unknown id
   is a `404`, never a fallback to the `Host`. Without the header the
   `Host` name is matched against the application list (exact domains
   first, then `*.wildcard` domains). Unknown hosts get a `404` once any
   application is configured. With no applications configured every
   request is forwarded unchanged, so a bare instance is a plain reverse
   proxy.
2. **Apply policy.** Disabled applications get a `403`. Plain HTTP requests
   for an application whose certificate is loaded (or that has a
   certificate configured and `tls.default_certificate` to fall back on)
   are redirected to HTTPS when `https.auto_redirect_to_https` is set. An
   application whose certificate failed to load stays reachable over plain
   HTTP instead of being redirected to a handshake that cannot succeed.
3. **Pick a backend** from the upstream pool using the configured strategy
   and forward the request. `x-deis-app-id` is set to the id deflux decided
   on, replacing whatever the client sent, so denix only sees ids that
   passed the checks above. With `https.auto_offload_https = false` the
   upstream leg keeps TLS.

Because the header is validated, denix's own targets such as
`_inspect:<app>` and `_system` cannot be reached through deflux; the
inspector is meant to be used on denix's own port.

TLS certificates belong to applications. During the handshake the SNI name
is looked up in the route table and that application's certificate is
presented; `tls.default_certificate` covers names no application claims.

## Configuration

See [`config.toml.example`](config.toml.example) for every key. Values are
layered: built-in baseline, then the file given with `-c`, then whatever
the supervisor pushes. Notable behaviour:

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
  certificate files) are resolved against the directory deflux was started
  from, and a `reload` starts the replacement from that same directory, so
  they keep resolving the same way across upgrades. Certificate paths in
  application configurations pushed by the supervisor are used as given;
  make them absolute.
- `[cache]` is parsed and validated but not applied to traffic yet.

Applications (`id`, `description`, `domains`, `certificates`, `enabled`,
`cache`) are not part of the file. They are pushed by the supervisor and persisted in `database`
(SQLite), so they survive restarts.

### Supervisor protocol

Deflux registers with denira over workit and then accepts:

| Push | Effect |
|---|---|
| `ConfigUpdate { key = "apps" }` | Replace the whole application list with the JSON array in `value_json`. |
| `ConfigUpdate { key = "app.<id>" }` | Insert or update one application. |
| `ConfigUpdate { key = "system" }` | Layer a traffic-policy overlay onto the system configuration (listeners, daemon settings, logging and the supervisor connection are ignored). |
| Command `shutdown` | Stop accepting connections, exit when in-flight requests finish. |
| Command `restart` / `reload` | Zero-downtime upgrade (see below). |
| Command `get_status` | Answer with instance state and uptime. |

`get_metrics` is answered with an explicit "not implemented".

If the supervisor is unreachable, at startup or later, deflux keeps
serving traffic and keeps retrying the connection (1 s doubling to 30 s
between attempts) until it succeeds or the process stops; a rejected secret
is logged at ERROR on every attempt.

## Running

```bash
deflux -c /etc/deflux/config.toml        # foreground
deflux -c /etc/deflux/config.toml -d     # daemon, writes pid_file
deflux -c /etc/deflux/config.toml -t     # check the configuration and exit
deflux -c /etc/deflux/config.toml -s reload
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

`reload` (or the supervisor's `restart`) starts a second `deflux --upgrade`
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
cargo test -p deflux
cargo clippy -p deflux --all-targets
../../scripts/test_graceful_restart.sh
../../scripts/demo.sh start              # full stack: denira + denix + deflux
```

`src/supervisor/mock.rs` is an in-process supervisor that the client tests
register against, so retry and reconnect behaviour is covered without denira.

## Layout

| Path | Contents |
|---|---|
| `src/main.rs` | Builds the pingora server from the configuration. |
| `src/proxy/` | `ProxyHttp` implementation, upstream pool, access log. |
| `src/tls.rs` | Certificate loading and SNI selection. |
| `src/config/` | Configuration model, layering, persistence, route table. |
| `src/supervisor/` | Supervisor client and its background service. |
| `src/restart.rs` | Upgrade and shutdown via pingora's signals. |
| `src/cli.rs` | Command line parsing. |
