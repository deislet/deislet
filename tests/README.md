# Integration tests

Cross-component checks that unit tests cannot cover. Each binary drives a real
client against a running `denira` and exits non-zero on the first failure.

| Binary | Exercises |
|---|---|
| `integration_workit_denira` | workit SDK -> denira: connect, authenticate, register, heartbeat |
| `integration_deflux_denira` | deflux supervisor client -> denira: full start sequence, readiness, graceful disconnect |

Both expect denira listening on `ws://127.0.0.1:9001` with the test secret
`a1b2c3d4e5f67890123456789012345678901234567890123456789012345678` in
`worker_preshared_keys`.

```bash
# terminal 1
cargo run -p denira -- --port 9001 --database /tmp/denira-test.db

# terminal 2
cargo run -p deislet-integration-tests --bin integration_workit_denira
cargo run -p deislet-integration-tests --bin integration_deflux_denira
```

The package is a workspace member, so `cargo build --workspace` keeps these
binaries compiling against the current crate APIs even when they are not run.

For a full-stack end-to-end run (denira + denix + deflux, example app deployed
through denira, request served through deflux) use `scripts/demo.sh` at the
repository root.

denix no longer registers with denira; it is an autonomous node that denira
polls over its gRPC management plane. That path is covered by `scripts/demo.sh`
rather than by a client binary here.
