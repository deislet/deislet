# Integration tests

Cross-component checks that unit tests cannot cover. Each binary drives a real
client against a running `deis-control` and exits non-zero on the first failure.

| Binary | Exercises |
|---|---|
| `integration_wire_control` | deis-wire SDK -> deis-control: connect, authenticate, register, heartbeat |
| `integration_proxy_control` | deis-proxy control client -> deis-control: full start sequence, readiness, graceful disconnect |

Both expect deis-control listening on port 9001 with the test secret
`a1b2c3d4e5f67890123456789012345678901234567890123456789012345678` in
`node_preshared_keys`.

Both binaries spell that address `http://127.0.0.1:9001`. They used to say
`ws://`, left over from when nodes really did speak WebSocket; they no longer
do - the link is gRPC over HTTP/2 - and that only ever worked because tonic
connects in plain text for anything that is not `https`, so the two schemes
reached the same socket. Every real configuration (`deis-proxy.toml`,
`deis-runtime.toml`, the demo) writes `http://`, and so does this.

That list is now also the credential for the control plane's HTTP node
endpoints - the assignment list, the node's own registration, and the bundle
download - because the assignment list carries every environment's decrypted
secrets. The paths are `deis_wire::conventions::NODE_ASSIGNMENTS_ROUTE` and
`NODE_REGISTRATION_ROUTE`; a runtime node presents the credential as
`[control] secret` in its own configuration, and without it those endpoints
answer 401 and the node never loads what it was assigned.

Both binaries authenticate with one hard-coded secret, so the control plane
has to be started with a configuration file that lists it. Starting it without
one does not fall back to accepting everybody: it generates a random key,
prints it in a WARN line, and refuses the secret these binaries send.

```bash
# terminal 1
cat > /tmp/deis-control-test.toml <<'TOML'
bind_addr = "127.0.0.1"
port = 9001
heartbeat_interval = 30
heartbeat_timeout = 90
node_preshared_keys = [
    "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678"
]
TOML
cargo run -p deis-control -- \
  --config /tmp/deis-control-test.toml \
  --database /tmp/deis-control-test.db

# terminal 2
cargo run -p deislet-integration-tests --bin integration_wire_control
cargo run -p deislet-integration-tests --bin integration_proxy_control
```

That value is the `TEST_SECRET` constant at the top of both binaries; change
one and the other stops authenticating. The four settings above it have no
serde defaults, so a file naming only the keys does not parse - the error is
`missing field 'bind_addr'`, not a fallback to the built-in defaults.

The package is a workspace member, so `cargo build --workspace` keeps these
binaries compiling against the current crate APIs even when they are not run.

For a full-stack end-to-end run - six processes, the example application
deployed with `deis deploy`, a request served through the node - use
`scripts/demo.sh` at the repository root.

There is no binary here for deis-runtime, and the reason is not that it stays
silent. It does register: on startup it `PUT`s its own registration and then
`GET`s its assignment list, both over HTTP with the node credential above, and
it fetches each bundle itself before reporting healthy. deis-control pushes to
it afterwards over the gRPC management plane. Both directions are exercised by
`scripts/demo.sh` - `verify` checks that the node registered, and that a node
id the control plane's configuration never mentioned can join the fleet by
registering alone - which needs a real node and a real control plane, so it
lives there rather than in a client binary.
