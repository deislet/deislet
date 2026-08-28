# Integration tests

Cross-component checks that unit tests cannot cover. Each binary drives a real
client against a running `deis-control` and exits non-zero on the first failure.

| Binary | Exercises |
|---|---|
| `integration_wire_control` | deis-wire SDK -> deis-control: connect, authenticate, register, heartbeat |
| `integration_proxy_control` | deis-proxy supervisor client -> deis-control: full start sequence, readiness, graceful disconnect |

Both expect deis-control listening on `ws://127.0.0.1:9001` with the test secret
`a1b2c3d4e5f67890123456789012345678901234567890123456789012345678` in
`worker_preshared_keys`.

```bash
# terminal 1
cargo run -p deis-control -- --port 9001 --database /tmp/deis-control-test.db

# terminal 2
cargo run -p deislet-integration-tests --bin integration_wire_control
cargo run -p deislet-integration-tests --bin integration_proxy_control
```

The package is a workspace member, so `cargo build --workspace` keeps these
binaries compiling against the current crate APIs even when they are not run.

For a full-stack end-to-end run (deis-control + deis-runtime + deis-proxy, example app deployed
through deis-control, request served through deis-proxy) use `scripts/demo.sh` at the
repository root.

deis-runtime no longer registers with deis-control; it is an autonomous node that deis-control
polls over its gRPC management plane. That path is covered by `scripts/demo.sh`
rather than by a client binary here.
