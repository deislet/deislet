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

1. **Resolve the application.** An explicit `x-deis-app-id` header wins;
   otherwise the `Host` name is matched against the application list
   (exact domains first, then `*.wildcard` domains). Unknown hosts get a
   `404` once any application is configured. With no applications
   configured every request is forwarded unchanged, so a bare instance is a
   plain reverse proxy.
2. **Apply policy.** Disabled applications get a `403`. Plain HTTP requests
   for an application that has a certificate are redirected to HTTPS when
   `https.auto_redirect_to_https` is set.
3. **Pick a backend** from the upstream pool using the configured strategy
   and forward the request. `x-deis-app-id` is added when absent so denix
   knows which application to run. With `https.auto_offload_https = false`
   the upstream leg keeps TLS.

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
- `[cache]` is parsed and validated but not applied to traffic yet.

Applications (`id`, `domains`, `certificates`, `enabled`) are not part of
the file. They are pushed by the supervisor and persisted in `database`
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

## Running

```bash
deflux -c /etc/deflux/config.toml        # foreground
deflux -c /etc/deflux/config.toml -d     # daemon, writes pid_file
deflux -c /etc/deflux/config.toml -t     # check the configuration and exit
deflux -c /etc/deflux/config.toml -s reload
```

`-s` signals the daemon named by `pid_file`:

| Signal | Effect |
|---|---|
| `stop` | Exit immediately. |
| `quit` | Stop accepting connections, exit when in-flight requests finish. |
| `reload` | Zero-downtime upgrade. |

### Zero-downtime upgrade

`reload` (or the supervisor's `restart`) starts a second `deflux --upgrade`
process with the same configuration, then sends `SIGQUIT` to the running
one. Pingora hands the listening sockets over the `upgrade_sock` Unix
socket; the new process starts accepting on them while the old one finishes
its in-flight requests and exits. No connection is refused in between.

`scripts/test_graceful_restart.sh` at the repository root exercises this
end to end against a local backend while keeping requests flowing.

## Development

```bash
cargo test -p deflux
cargo clippy -p deflux --all-targets
../../scripts/test_graceful_restart.sh
../../scripts/demo.sh start              # full stack: denira + denix + deflux
```

`src/bin/mock_superv.rs` is a minimal supervisor for exercising the client
without denira.

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
