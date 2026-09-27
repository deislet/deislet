# Control Deployment Guide

**Version**: v0.1.0
**Last Updated**: 2026-09-02

---

## 📋 Pre-deployment Checklist

### System Requirements

- **Operating System**: Linux (Ubuntu 20.04+, CentOS 8+ recommended)
- **CPU**: 2+ cores
- **Memory**: 2GB+ (depending on the number of Nodes)
- **Disk**: 10GB+ (for database and logs)
- **Network**: able to reach every node it manages, and reachable by them

### Software Dependencies

- Rust 1.85+ (the workspace is edition 2024; 1.70 cannot build it)
- Node.js 18+ (for the console build, which the binary embeds at compile time)
- `sqlite3`, 3.8.2 or newer for its `-readonly` flag (the backup and
  troubleshooting procedures below use `.backup`, and every one of them opens
  the live database read-only; a `cp` of a database that is being written can
  copy a torn one without saying so)
- `openssl` (generates the secret master key)
- `curl` and `jq` (the readiness check below)
- (Optional) Nginx/Caddy (reverse proxy)
- (Optional) Certbot (Let's Encrypt certificates)

---

## 🚀 Quick Deployment

### 1. Compile Project

`deis-control` is one crate of the `deislet` workspace, not a repository of
its own, and it embeds the web console into the binary at compile time. So the
console is built **first**: with `crates/deis-control/web/dist` missing,
`cargo build` fails outright rather than producing a binary without a console.

```bash
# Clone the workspace this crate lives in
git clone https://github.com/deislet/deislet.git
cd deislet

# The console, first. Its output is gitignored, and the compile below
# embeds it, so a fresh clone has to do this before anything else.
cd crates/deis-control/web && npm ci && npm run build && cd ../../..

# Then the binary. Lands in target/release/deis-control.
cargo build --release -p deis-control
```

### 2. Prepare Configuration File

```bash
sudo mkdir -p /etc/deis-control

# Copy the example configuration (deis-control.example.toml is the only
# one; the file it is copied to can be named anything -c is given)
sudo cp crates/deis-control/deis-control.example.toml /etc/deis-control/config.toml

# Edit configuration
sudo vi /etc/deis-control/config.toml
```

**Minimal Configuration** (`config.toml`) - this is the whole file, and it
starts:

```toml
# Every key below is required: the four without a default are refused at
# parse time by name, and the fifth is what makes secrets work at all. A
# file missing `heartbeat_interval` does not start with a warning, it does
# not start at all: "TOML parse error ... missing field heartbeat_interval".
# The [machine] table further down is required too, for a different reason:
# it is not one key with a default, it is the whole listener a node or a
# proxy reaches, and a file with no [machine] table refuses to start.
bind_addr = "0.0.0.0"
port = 8080

# How often this control plane polls each node it knows about, in seconds,
# and how long a node may be silent before it is reported unresponsive.
# There is no default for either: they decide how quickly a dead node is
# noticed, and a number nobody chose is the wrong number to inherit.
heartbeat_interval = 15
heartbeat_timeout = 90

# This control plane's own certificate, private key and trust root, and
# where a node or a proxy reaches them. Not optional: registration, the
# assignment list, the bundle download and the routing table are served
# only here, over mutual TLS, and a file with no [machine] table refuses to
# start rather than come up dead to its fleet. Issuing, renewing and
# revoking the three files is outside this process - deis-control only
# re-reads them, every identity_reload_seconds, so a renewal needs no
# restart. Each peer's certificate carries one URI subject alternative
# name, a SPIFFE name - spiffe://<trust domain>/deislet/node/<id> for a
# runtime node, spiffe://<trust domain>/deislet/proxy/<id> for deis-proxy -
# and that proved name, not a value in this file, is what says which
# machine is calling.
[machine]
bind_addr = "127.0.0.1"
port = 8443
certificate = "/etc/deis-control/certs/control.pem"
private_key = "/etc/deis-control/certs/control-key.pem"
trust_root = "/etc/deis-control/certs/trust-root.pem"

# node_preshared_keys is RETIRED.  The control plane will refuse to start
# if this key appears anywhere in the configuration file.  Every door it
# once guarded (assignments, bundles, registration, routing table) is now
# served exclusively on the [machine] listener with mutual TLS — see
# docs/adr/025-machine-identity-and-capabilities.md.
#
# To migrate a fleet: configure [machine] on every node and proxy, verify
# that the fleet works with node_preshared_keys set to an empty list, then
# delete this section and remove it from every node's [control] secret.
# The refuse_a_retired_fleet_key check in config.rs names the exact lines
# to remove.
# node_preshared_keys = []

# Require an API key on the dashboard and the human API (recommended).
# This setting does not reach the machine channel above.
api_auth_enabled = true
api_keys = [
    "your-api-key-change-me"
]

# The 32-byte key every environment secret is encrypted with. Without it
# this control plane starts, answers /health with OK, and refuses every
# secret endpoint with 503 - and serves each node an assignment list with
# every application that has a secret withheld from it. Create it before
# the first start (see below), and back it up: the encrypted values in the
# database are worth nothing without this file.
secret_key_file = "/etc/deis-control/secret.key"

# Where uploaded bundles are kept - the only copy of every version's code.
# Absolute on purpose; see the comment in the example configuration.
bundle_dir = "/var/lib/deis-control/bundles"
```

### 3. Initialize Database

The database is created on first start. The two things that are *not*
created for you are the directory it lives in and the secret master key -
and a control plane started without the key runs, and is unusable for every
application that has a secret.

The `[machine]` certificate, private key and trust root are a harder
precondition than the master key: they are not generated and not optional,
and a control plane started with any of the three missing or unreadable does
not run at all rather than run degraded. Provisioning them - an operator's
own certificate authority, or an existing one - is outside this document's
script; what deis-control needs is three PEM files at the paths named in
`[machine]`, the leaf carrying one URI subject alternative name,
`spiffe://<trust domain>/deislet/control`.

```bash
# Database and bundle store. bundle_dir is under this path, and it is the
# only copy of every version's code, so it belongs on storage that survives
# a restart and is in the backup.
sudo mkdir -p /var/lib/deis-control/bundles

# The secret master key: 32 bytes, as 64 hex characters. Generate it once,
# per installation, and never regenerate it - the values already in the
# database decrypt with this key and no other. Losing it loses every
# secret; a copy of it that other local users can read is not a secret at
# all, which is why deis-control complains about the mode.
sudo openssl rand -hex 32 | sudo tee /etc/deis-control/secret.key > /dev/null
sudo chmod 600 /etc/deis-control/secret.key
```

### 4. Start Service

```bash
# Test startup
./target/release/deis-control \
  -c /etc/deis-control/config.toml \
  --database /var/lib/deis-control/deis-control.db
```

**`/health` is a liveness probe, and only that.** It answers the constant
`OK` for as long as the process is running and accepting connections. It is
deliberately that dumb - an unauthenticated prober must not be able to read
the state of the platform off it - so it says nothing about whether the
bundle store is present, whether the master key is readable, or whether any
application can run. A control plane that has lost both still answers `OK`.

So `curl /health` proves the process is up, and the startup log is what says
whether it can do anything:

```bash
curl http://localhost:8080/health          # expect: OK - the process is alive

# The three lines that decide whether this installation works at all.
# Each of them is an ERROR or a WARN, and each names what stops working.
journalctl -u deis-control -n 200 | grep -E 'Master key unusable|Bundle directory is not usable|No node credential'
# Expect no output. Any hit here is an installation that answers OK and
# cannot serve.
```

The readiness check that answers "can it serve" is in **Monitoring and
Operations** below; it is also the one to run after a restore.

**Run one control plane per database.** The design assumes a single process.
If two are started on one database file even so - a manual start beside the
service unit, an overlap during a cut-over - they do not both fire cron: the
first to take the `scheduler_lease` row fires, and the other logs once at
`WARN` ("Another control plane holds the cron scheduler lease") and fires
nothing until the holder stops refreshing the row for three ticks or releases
it on a clean stop. Everything else - deployment pushes, health polling, the
API - runs in both processes without coordination, so an overlap is something
to end, not to run with.

---

## 🐳 Docker Deployment

### Dockerfile

Build it from the root of the `deislet` workspace, not from this crate's
directory: the binary is one crate of that workspace. Two things the obvious
Dockerfile gets wrong and this one does not - the toolchain has to be new
enough for edition 2024, and the console has to be built before `cargo`
runs, because `deis-control` embeds `crates/deis-control/web/dist` at compile
time and the build fails outright without it.

```dockerfile
FROM rust:1.85 AS builder

# Node, for the console the binary embeds.
RUN apt-get update && \
    apt-get install -y --no-install-recommends nodejs npm && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

# The console first; cargo fails without its output.
RUN cd crates/deis-control/web && npm ci && npm run build
RUN cargo build --release -p deis-control

FROM ubuntu:22.04

RUN apt-get update && \
    apt-get install -y ca-certificates sqlite3 && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/deis-control /usr/local/bin/

# The configuration, the master key and the [machine] certificate/private
# key/trust root are mounted at run time, not baked in: a key in an image
# layer is a key in every registry that image reaches.
VOLUME ["/var/lib/deis-control"]
EXPOSE 8080 8443

CMD ["deis-control", "-c", "/etc/deis-control/config.toml", "--database", "/var/lib/deis-control/deis-control.db"]
```

### docker-compose.yml

```yaml
services:
  deis-control:
    build: .
    ports:
      - "8080:8080"  # HTTP API + the gRPC control channel deis-proxy holds open
      - "8443:8443"  # [machine]: registration, assignments, bundle download, routing table
    volumes:
      # Database and bundle store. The bundle store has to be on this volume:
      # it is the only copy of every version's code, and a container that
      # keeps it in the writable layer loses every version on the next
      # `docker compose up --build`.
      - deis-control-data:/var/lib/deis-control
      - ./config.toml:/etc/deis-control/config.toml:ro
      # The secret master key. Without this mount the container starts,
      # answers /health with OK, and withholds every application that has a
      # secret from every node.
      - ./secret.key:/etc/deis-control/secret.key:ro
      # [machine]'s certificate, private key and trust root. Without this
      # mount the container does not start at all - unlike the master key
      # above, this is not a degraded run, it is a refusal.
      - ./certs:/etc/deis-control/certs:ro
    restart: unless-stopped
    environment:
      - RUST_LOG=info

volumes:
  deis-control-data:
```

The `config.toml` beside the compose file is the minimal configuration from
above, with the paths pointing inside the container:

```toml
secret_key_file = "/etc/deis-control/secret.key"
bundle_dir = "/var/lib/deis-control/bundles"

[machine]
bind_addr = "0.0.0.0"
port = 8443
certificate = "/etc/deis-control/certs/control.pem"
private_key = "/etc/deis-control/certs/control-key.pem"
trust_root = "/etc/deis-control/certs/trust-root.pem"
```

`docker compose down -v` deletes that volume, and with it the database **and**
the bundle store. Take a backup first; see **Backup and Recovery** below.

### Start

```bash
# Create the master key once, beside the compose file, before the first start.
openssl rand -hex 32 > secret.key && chmod 600 secret.key

# The [machine] certificate, private key and trust root, in ./certs/.
# Provisioning them is outside this document's script - an operator's own
# certificate authority, or an existing one - but without the three files at
# the paths above, this container will not start at all.

docker compose up -d

# View logs
docker compose logs -f

# Stop. Without -v the data volume survives, which is what you want.
docker compose down
```

---

## 🔐 Production Environment Configuration

### 1. TLS/HTTPS Configuration

#### Option A: Built-in TLS

**Generate self-signed certificate (for testing):**
```bash
openssl req -x509 -newkey rsa:4096 -nodes \
  -keyout /etc/deis-control/key.pem \
  -out /etc/deis-control/cert.pem \
  -days 365 \
  -subj "/CN=deis-control.yourdomain.com"
```

**Let's Encrypt certificate (production):**
```bash
# Install certbot
sudo apt install certbot

# Obtain certificate
sudo certbot certonly --standalone -d deis-control.yourdomain.com

# Configuration file
[tls]
api_enabled = true
cert_path = "/etc/letsencrypt/live/deis-control.yourdomain.com/fullchain.pem"
key_path = "/etc/letsencrypt/live/deis-control.yourdomain.com/privkey.pem"
```

**Auto-renewal:**
```bash
# Add to crontab
sudo crontab -e

# Check and renew at 3 AM on the 1st of every month
0 3 1 * * certbot renew --quiet && systemctl restart deis-control
```

#### Option B: Nginx Reverse Proxy

**Nginx configuration** (`/etc/nginx/sites-available/deis-control`):
```nginx
server {
    listen 443 ssl http2;
    server_name deis-control.yourdomain.com;

    ssl_certificate /etc/letsencrypt/live/deis-control.yourdomain.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/deis-control.yourdomain.com/privkey.pem;

    # Modern TLS configuration
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_ciphers HIGH:!aNULL:!MD5;
    ssl_prefer_server_ciphers on;

    # HSTS
    add_header Strict-Transport-Security "max-age=31536000; includeSubDomains" always;

    # API endpoints
    location /api {
        proxy_pass http://localhost:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    # The node control channel is gRPC over HTTP/2 on the same port, not a
    # WebSocket upgrade: it needs h2c end to end.
    location /deis.control.ControlService/ {
        grpc_pass grpc://localhost:8080;
    }

    # The console. It is compiled into the binary and served from the same
    # port as the API, so there is no static directory to point at: proxy it
    # like everything else. A `root` here would serve an empty directory and
    # a 404 for the dashboard.
    location / {
        proxy_pass http://localhost:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}

# HTTP redirect to HTTPS
server {
    listen 80;
    server_name deis-control.yourdomain.com;
    return 301 https://$server_name$request_uri;
}
```

**Enable configuration:**
```bash
sudo ln -s /etc/nginx/sites-available/deis-control /etc/nginx/sites-enabled/
sudo nginx -t
sudo systemctl reload nginx
```

### 2. Systemd Service

Create service file (`/etc/systemd/system/deis-control.service`):

```ini
[Unit]
Description=Control Cluster Control
After=network.target

[Service]
Type=simple
User=deis-control
Group=deis-control
# A directory the service user owns and that the steps below actually
# create. systemd refuses to start the unit when this path does not exist,
# and the paths in the configuration are absolute anyway, so there is
# nothing to gain from pointing it somewhere else.
WorkingDirectory=/var/lib/deis-control
ExecStart=/usr/local/bin/deis-control \
  -c /etc/deis-control/config.toml \
  --database /var/lib/deis-control/deis-control.db

# Restart policy
Restart=always
RestartSec=10

# Shutdown: on stop, systemd sends SIGTERM and deis-control stops accepting
# new connections, then finishes the requests it already has (bundle
# uploads and deployment writes among them) before leaving. It gives that
# drain 15 seconds, so leave the stop timeout above that or systemd will
# SIGKILL mid-request - the exact thing the drain avoids.
KillSignal=SIGTERM
TimeoutStopSec=30

# Resource limits
LimitNOFILE=65536
LimitNPROC=4096

# Logging
StandardOutput=journal
StandardError=journal
SyslogIdentifier=deis-control

[Install]
WantedBy=multi-user.target
```

**Start service:**
```bash
# Create user
sudo useradd -r -s /bin/false deis-control

# Create directories and set permissions. The bundle store is under the
# data directory and has to exist before the first upload; the service
# creates it if it can write there, which is what the chown below is for.
sudo mkdir -p /var/lib/deis-control/bundles /etc/deis-control
sudo chown -R deis-control:deis-control /var/lib/deis-control
sudo chmod 700 /var/lib/deis-control

# The secret master key, once per installation. See "Initialize Database"
# above for what happens without it.
sudo openssl rand -hex 32 | sudo tee /etc/deis-control/secret.key > /dev/null
sudo chown deis-control:deis-control /etc/deis-control/secret.key
sudo chmod 600 /etc/deis-control/secret.key

# The [machine] certificate, private key and trust root - see "Initialize
# Database" above for why this one is not optional. Provisioning them is
# outside this document's script; place the three files this operator's
# certificate authority issued at the paths [machine] names, then lock the
# private key down the same way as the master key.
sudo mkdir -p /etc/deis-control/certs
# sudo cp control.pem control-key.pem trust-root.pem /etc/deis-control/certs/
sudo chown -R deis-control:deis-control /etc/deis-control/certs
sudo chmod 600 /etc/deis-control/certs/control-key.pem

# Install binary file
sudo cp target/release/deis-control /usr/local/bin/
sudo chown root:root /usr/local/bin/deis-control
sudo chmod 755 /usr/local/bin/deis-control

# The configuration written in "Prepare Configuration File" above. It holds
# the node credential and the API keys, so it is readable by this user only.
sudo chown deis-control:deis-control /etc/deis-control/config.toml
sudo chmod 600 /etc/deis-control/config.toml

# Start service
sudo systemctl daemon-reload
sudo systemctl enable deis-control
sudo systemctl start deis-control

# View status
sudo systemctl status deis-control

# View logs
sudo journalctl -u deis-control -f
```

### 3. Firewall Configuration

```bash
# UFW (Ubuntu)
sudo ufw allow 8080/tcp  # HTTP API + the gRPC control channel
sudo ufw allow 8443/tcp  # The [machine] channel: registration, assignments,
                         # the bundle download, the routing table
sudo ufw allow 443/tcp   # HTTPS (if using Nginx)
sudo ufw enable

# firewalld (CentOS)
sudo firewall-cmd --permanent --add-port=8080/tcp
sudo firewall-cmd --permanent --add-port=8443/tcp
sudo firewall-cmd --permanent --add-port=443/tcp
sudo firewall-cmd --reload
```

Open 8080 to deis-proxy and to the operators' network - not to the internet,
and not to the nodes, which have no reason to reach it any more. Open the
`[machine]` port (8443 above) to the nodes and to deis-proxy instead: it
serves the assignment list, which hands out every environment's decrypted
secrets, but only to a caller that completes a mutual TLS handshake proving a
certificate this fleet signed - a plain connection, or one presenting the
wrong certificate, is refused before either port answers with anything. The
node data ports need a rule of their own, on the node hosts, for the same
kind of reason.
[docs/trust-boundaries.md](../../docs/trust-boundaries.md) states which port
may be reached from where, the file permissions each configuration needs, and
how to rotate a credential without an outage.

---

## 📊 Monitoring and Operations

### 1. Log Management

**Configure log level.** The filter names a *module path*, and a module path
uses underscores: `deis_control`, never `deis-control`. The hyphenated
spelling is not an error and is not warned about - it simply matches nothing,
and because it is the only directive it also turns off the default, so the
service runs completely silent. That includes the startup ERROR lines that
say the master key is unreadable or the bundle directory is not writable.

```bash
# Everything this crate logs at info, everything else at warn.
export RUST_LOG=deis_control=info,warn

# In the systemd service
Environment="RUST_LOG=deis_control=info,warn"
```

**Log rotation.** Under the systemd unit above, output goes to the journal
(`StandardOutput=journal`), so rotation is `journalctl`'s and there is nothing
to configure here:

```bash
# How much the journal keeps
sudo journalctl --vacuum-time=30d
# Or permanently, in /etc/systemd/journald.conf: MaxRetentionSec=30day
```

Only if you redirect the service's output to a file of your own does logrotate
come into it, and then it has to be `copytruncate`: the process writes to the
descriptor it was started with, has no reload that reopens a log file, and
`systemctl reload` on this unit fails outright because it declares no
`ExecReload`.

```
/var/log/deis-control/*.log {
    daily
    rotate 7
    compress
    delaycompress
    missingok
    notifempty
    copytruncate
}
```

### 2. Prometheus Monitoring

`/api/metrics` is behind the same credential as the rest of the human API, so
with `api_auth_enabled = true` - which the minimal configuration above sets -
an unauthenticated scrape gets **401**, not a page of numbers.

**Two commands, in this order.** A fresh control plane has exactly one
account, `config-key`, created for the `api_keys` in its configuration file;
there is no `ops`, no `prometheus` and nothing else until somebody makes one.
`deis token create --account <name>` against a name that does not exist
answers `404 Not Found: account <name> not found` and mints nothing - while
the redirection below has already created the file, so a scraper set up from
the second command alone starts with a zero-byte token and every scrape
answering 401. The account name here is the one
[`API_DOCUMENTATION.md`](API_DOCUMENTATION.md#get-prometheus-format-metrics)
uses, so the two procedures are the same procedure.

```bash
# 1. The account. `viewer` is the narrowest role, and the role only bounds
#    what tokens may be minted for it.
deis account create prometheus --role viewer

# 2. Its credential. `read:*/*` is the narrowest scope that works: the
#    exposition names no application but counts all of them, so the gate asks
#    for every application rather than any one, and a token scoped to a single
#    app is refused with 403 and told exactly which scope it lacks.
#
#    Do not point a scraper at an api_keys entry instead: those are unscoped
#    and can deploy, rotate secrets and mint any tenant's storage token. This
#    token can read everything and change nothing - the catalog, the routes
#    and the audit log are readable with it, which is worth knowing before the
#    file lands on a monitoring host. The secret is printed once and never
#    again, so it goes straight into the file the scraper reads.
deis token create --account prometheus --scope 'read:*/*' --json scrape \
  | jq -r .secret > /etc/prometheus/deis-control.token
chmod 600 /etc/prometheus/deis-control.token
```

**Prometheus configuration** (`prometheus.yml`):
```yaml
scrape_configs:
  - job_name: 'deis-control'
    static_configs:
      - targets: ['localhost:8080']
    metrics_path: '/api/metrics'
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/deis-control.token
```

`deis-telemetry` exports on a port of its own, and its credential is not one
of these: it has no accounts and no scopes. Its scrape carries
`base64(HMAC-SHA256(auth_secret, "deis-telemetry"))`, the same one string
every producer and every `deis logs` / `deis tail` / `deis trace` presents,
and it is required only once `auth_secret` is set - which is required as soon
as either of that service's addresses is not a loopback address. See
`crates/deis-telemetry/deis-telemetry.example.toml`; `deis token create`
mints nothing that service will accept.

Two series there are worth an alert on any installation:
`deis_telemetry_store_writable` going to 0 means the collector is refusing
every log line and trace it is handed, and `deis_telemetry_storage_failures_total`
climbing means something underneath it is failing whatever surface noticed.

### 3. Health Check

`/health` answers a constant `OK` while the process is alive, so a probe on
it is a liveness probe and belongs in a supervisor's restart policy. It is
not a readiness check and must not be used as one: the failure it cannot see
is exactly the one that follows a bad restore, where the process is perfectly
alive and every application is missing its code, its secrets, or both.

**Liveness** (`healthcheck.sh`) - restart the process when this fails:

```bash
#!/bin/bash
response=$(curl -s -o /dev/null -w "%{http_code}" http://localhost:8080/health)

if [ "$response" -eq 200 ]; then
    echo "Control is alive"
    exit 0
else
    echo "Control is not answering (HTTP $response)"
    exit 1
fi
```

**Readiness** - whether it can actually serve. Restarting does not fix any of
these, so this belongs in an alert, not in a restart policy:

```bash
#!/bin/bash
# readiness.sh <api-key> <machine-base-url> <client-cert> <client-key> <ca-cert>
#
# The bundle download used to answer a node-key bearer token on this same
# port; it is on the [machine] listener now and answers only a caller that
# completes a mutual TLS handshake with a certificate this fleet signed, so
# this probe needs one of the certificates minted for that trust root - a
# short-lived one issued for this check is fine, it never touches secrets.
set -u
API=$1; MACHINE_BASE=$2; CERT=$3; KEY=$4; CA=$5; BASE=http://localhost:8080
fail=0

# 1. Every version's bundle is on disk. A version whose file is missing
#    answers 500 here, and no node can ever load it.
for row in $(curl -sf -H "Authorization: Bearer $API" "$BASE/api/apps" \
             | jq -r '.apps[].id'); do
  for vid in $(curl -sf -H "Authorization: Bearer $API" "$BASE/api/apps/$row/versions" \
               | jq -r '.versions[].id'); do
    code=$(curl -s -o /dev/null -w '%{http_code}' \
           --cert "$CERT" --key "$KEY" --cacert "$CA" \
           "$MACHINE_BASE/api/apps/$row/versions/$vid/bundle")
    [ "$code" = 200 ] || { echo "bundle missing: $row $vid (HTTP $code)"; fail=1; }
  done
done

# 2. The master key decrypts what is in the database. Listing names needs no
#    key; writing one needs the key and the existing rows to agree with it.
code=$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
       -H "Authorization: Bearer $API" -H 'content-type: application/json' \
       -d '{"value":"readiness-probe"}' \
       "$BASE/api/apps/<app>/environments/<env>/secrets/DEIS_READINESS")
# 503 here is a missing or unreadable secret_key_file; 500 is a master key
# that does not decrypt what is already in the database, which is the shape
# of a restore that took the database from one host and the key from another.
[ "$code" = 200 ] || { echo "secret storage unusable (HTTP $code)"; fail=1; }

exit $fail
```

Point 1 is the check that would have caught the restore this document used to
describe. Point 2 needs an application and environment to write into; use a
throwaway one, and delete the probe secret afterwards:

```bash
curl -s -X DELETE -H "Authorization: Bearer $API" \
  "$BASE/api/apps/<app>/environments/<env>/secrets/DEIS_READINESS"
```

---

## 🔧 Troubleshooting

### Common Issues

#### 1. Service fails to start

```bash
# Check port occupation
sudo lsof -i :8080

# Check the configuration file. There is no --check-config flag: the parse
# happens at startup, so a start is the check. `-p 0` takes an ephemeral
# port and a throwaway database, so this can be run beside a live instance;
# Ctrl-C it once it says "Listening". A file missing one of the required
# keys never gets that far - it fails with "missing field <key>", naming
# the key, before anything is bound or written.
deis-control -c /etc/deis-control/config.toml --database /tmp/probe.db -p 0

# View detailed logs
RUST_LOG=debug deis-control -c /etc/deis-control/config.toml
```

#### 2. Node cannot connect

- Check network connectivity to the `[machine]` port, not 8080: a node no
  longer reaches this control plane on the human listener at all -
  `telnet deis-control-host 8443` (or whatever `[machine].port` is)
- Check firewall rules on that port
- Verify the node's `[machine_identity]` certificate, private key and trust
  root are readable, current, and chain to the same trust root this control
  plane's `[machine]` table names - a mismatched or expired certificate is
  refused during the TLS handshake, before this control plane's own logs see
  the connection at all
- View node-side logs

#### 3. Database locked

A lock is contention, not damage: some other process is holding the write lock
on a file this one wants. The catalog runs on SQLite's default rollback
journal rather than WAL - nothing here sets `journal_mode`, and
`PRAGMA journal_mode` on a live catalog answers `delete` - so one writer
excludes every reader for as long as it is writing. That makes the question
"who else has this file open", and nothing else.

`-readonly` on each command below is the load-bearing part, and it is why
this section no longer opens a plain `sqlite3` prompt. Without the flag the
shell opens the database **read-write**: a second writer attached to the live
catalog, which is the exact condition being diagnosed. It queues for the same
lock; interrupted mid-statement it leaves a hot journal the service has to
roll back before it can serve; and run under `sudo` beside a service running
as `deis-control` it creates that journal owned by root, after which the
service cannot write at all and a lock that would have cleared on its own has
become an outage. With `-readonly`, SQLite refuses every write before it
reaches the file.

```bash
# Who has the file open. This is the answer most of the time.
sudo lsof /var/lib/deis-control/deis-control.db

# Permissions - the directory as well as the file. SQLite writes its journal
# beside the database, so a writable file in a directory the service cannot
# write to is a database it cannot write either.
ls -ld /var/lib/deis-control
ls -l /var/lib/deis-control/deis-control.db

# Is the file itself damaged? Asked without becoming a writer.
sqlite3 -readonly /var/lib/deis-control/deis-control.db 'PRAGMA integrity_check;'

# A copy to work on, through SQLite's own backup API so it is consistent even
# while the service is running. Poke at the copy; leave the live file alone.
sqlite3 -readonly /var/lib/deis-control/deis-control.db \
  ".backup /tmp/deis-control-copy.db"
```

Stop the service before running anything that writes. A `sqlite3` prompt on
the live catalog reaches applications, versions, accounts, scoped tokens and
the audit log with no authorization gate in front of it and no audit line
behind it - the audit log has no delete anywhere in this process, and that
prompt is the one way around it.

#### 4. TLS certificate issues

```bash
# Verify certificate
openssl x509 -in /etc/deis-control/cert.pem -text -noout

# Check certificate validity period
openssl x509 -in /etc/deis-control/cert.pem -noout -enddate

# Test TLS connection
openssl s_client -connect deis-control.yourdomain.com:8080
```

---

## 📦 Backup and Recovery

### What has to be in the backup

Four things, and the database is only one of them. The procedure in this
document used to be the database and the configuration file; a restore from
that answers `/health` with `OK`, lists every application and every version in
the console, and cannot run a single one of them.

| What | Where | What is lost without it |
|---|---|---|
| The catalog database | `--database`, e.g. `/var/lib/deis-control/deis-control.db` | Applications, versions, environments, deployment pointers, routes, accounts, scoped tokens, the encrypted secrets and the audit log. |
| **The bundle store** | `bundle_dir`, e.g. `/var/lib/deis-control/bundles` | Every version's code. The database keeps a *path*, not the bytes. A node fetching a version whose file is gone gets `500 bundle ... is recorded but missing on disk`, and re-uploading cannot repair it: the manifest is inside the archive whose sha256 **is** the version id, so an upload of the same source produces a different version. Rollback to any earlier version becomes impossible. |
| **The secret master key** | `secret_key_file`, e.g. `/etc/deis-control/secret.key` | Every environment secret. The values in the database are encrypted with this key and decrypt with no other. Without the file, secret endpoints answer 503 and **every application that has a secret is withheld from each node's assignment list** - the list comes back marked incomplete, so a node reconciling against it keeps whatever it is already running and starts none of them. On a node that was rebuilt with the platform, that is nothing at all. |
| The configuration file | `-c`, e.g. `/etc/deis-control/config.toml` | `secret_key_file` names the master key that decrypts environment secrets and the capability signing keys in the database; a restored control plane with a different key refuses to start rather than sign with keys deis-store and deis-queue have never seen. `node_preshared_keys` matters the same way only for a fleet still mid-migration - a node or a proxy that already has a `[machine_identity]` certificate does not read this file's values at all, because it authenticates by the name on its certificate instead. |

Not in this table, and not covered by this document's backup script: the
`[machine]` table's own certificate, private key and trust root. This file
only names their paths; the three PEM files themselves are the operator's own
public-key infrastructure, issued and rotated outside this process, so their
backup is that system's concern rather than this one's. What is worth
remembering here is what is lost without them - not the data, but the
listener: a control plane restored with a `[machine]` table pointing at
files that are not on the restored host refuses to start, the same as a
first install with none provisioned yet.

The other components hold state of their own and are not covered by this
document's script. In the order it matters:

* **deis-store** - `deis-store snapshot --out DIR` and
  `deis-store restore --from DIR`. Use them. Every database there runs in WAL
  mode, so `cp -r` or `rsync` of the data directory can copy a tree that is
  missing the most recent commits or is simply corrupt, and it does not fail
  when you take it - only when you restore it. The snapshot covers the SQLite
  databases *and* the objects, and writes a `snapshot.json` naming both counts;
  a running service takes the same snapshots on a timer once `snapshot_dir` is
  set.
* **deis-queue** - `deis-queue snapshot --out DIR` and
  `deis-queue restore --from DIR`. The command uses `VACUUM INTO`, records the
  database digest plus message/dead-letter counts, and gives a restored data
  directory a new backend incarnation. Never copy its WAL-mode directory with
  `cp`; in-flight messages are the loss.
* **deis-proxy** - the ACME storage directory (`tls.acme.storage_dir`, default
  `acme`). It holds the account key and the issued certificates. Losing it is
  recoverable but not free: the account is registered again and every
  certificate re-issued, against the issuing authority's rate limits.
* **deis-telemetry** - `<data_dir>/telemetry.sqlite`. Logs, traces and metric
  samples; losing them loses history, not the ability to serve. Back it up if
  the history is worth something, with `sqlite3 -readonly ... ".backup ..."`
  for the same two reasons: WAL, and never a second writer.
* **deis-runtime** - nothing. A node's data directory is derived state: its
  assignment list and the bundles it has fetched. It rebuilds both from the
  control plane on its next poll, which is exactly why the control plane's
  bundle store has to be in the backup.

### Backup

```bash
#!/bin/bash
# backup.sh - the things a control plane cannot be rebuilt without.
set -euo pipefail

DB=/var/lib/deis-control/deis-control.db
BUNDLES=/var/lib/deis-control/bundles
KEY=/etc/deis-control/secret.key
CONF=/etc/deis-control/config.toml
# The machine identity from [machine], which is what every node and every
# proxy in this fleet checks before it will talk to this control plane. See
# the note under the archive line for when these belong in here and when they
# do not.
CERTS=/etc/deis-control/certs

BACKUP_DIR=/var/backups/deis-control
DATE=$(date +%Y%m%d_%H%M%S)
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$BACKUP_DIR"

# The database, through SQLite's own backup API rather than cp: this runs on a
# timer against a control plane that is serving, and a copy taken part-way
# through a transaction is missing the journal that would complete it. Such a
# file opens perfectly the day it is made and is short of its last commits, or
# corrupt, on the day it is restored. (The catalog is on the default rollback
# journal, not WAL - `PRAGMA journal_mode` answers `delete` - which changes
# how a torn copy goes wrong and not that it does.)
#
# -readonly for the same reason the troubleshooting section uses it: a backup
# must never be the second writer on the live catalog. Without the flag this
# line opens the database read-write, queues behind the service for the write
# lock, and - run from root's cron beside a service running as deis-control -
# can leave a journal file the service is not allowed to write.
sqlite3 -readonly "$DB" ".backup '$STAGE/deis-control.db'"

# The bundle store: the only copy of every version's code.
cp -a "$BUNDLES" "$STAGE/bundles"

# The master key, and the configuration that has to match the rest of the
# fleet. Both hold credentials, so the archive is written 0600.
cp -a "$KEY" "$STAGE/secret.key"
cp -a "$CONF" "$STAGE/config.toml"

# The machine identity. A restored control plane with the database, the key
# and the bundles but no certificate comes up, answers /health, serves the
# console - and no node or proxy in the fleet will complete a handshake with
# it, so nothing is deployed, nothing registers, and no routing table is
# fetched. It looks healthier than a restore that failed outright.
#
# Whether these three files belong in this archive is the one judgement call
# in this script. They are not derived state and nothing rebuilds them, so if
# the authority that issued them is a one-off on somebody's laptop, this
# archive is the only copy and it has to carry them - and the archive is then
# as sensitive as the private key in it, which is why the whole thing is 0600
# already. If the fleet has a real issuer, re-issue on the restored host
# instead and drop this line: a backup that carries a private key around is
# one more place that key can be read from.
cp -a "$CERTS" "$STAGE/certs"

# The absolute path the bundle store lived at. The restore needs it; see
# below for why.
echo "$BUNDLES" > "$STAGE/bundle_dir.path"

umask 077
tar czf "$BACKUP_DIR/deis-control-$DATE.tar.gz" -C "$STAGE" .

find "$BACKUP_DIR" -name '*.tar.gz' -mtime +30 -delete
```

### Recovery

**Copying the bundle files back is not enough if the path changed.** Each
version row holds an absolute `bundle_path`, and only a path stored *relative*
is re-anchored at startup - an absolute one from a host that no longer exists
is left exactly as it was. Bundle files restored to a different absolute path
are therefore restored to somewhere nothing will look, and the symptom is
identical to not having copied them at all: `500 recorded but missing on disk`
on every node's fetch, while `/health` goes on answering `OK`.

That is what `bundle_dir.path` in the archive is for. The script below reads
it, and when the store has moved it rewrites both the recorded paths and
`bundle_dir` in the restored configuration.

```bash
#!/bin/bash
# restore.sh <backup_file.tar.gz>
set -euo pipefail

BACKUP_FILE=${1:?Usage: $0 <backup_file.tar.gz>}
DB=/var/lib/deis-control/deis-control.db
BUNDLES=/var/lib/deis-control/bundles
KEY=/etc/deis-control/secret.key
CONF=/etc/deis-control/config.toml
CERTS=/etc/deis-control/certs

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
tar xzf "$BACKUP_FILE" -C "$STAGE"

sudo systemctl stop deis-control

install -o deis-control -g deis-control -m 600 "$STAGE/deis-control.db" "$DB"
install -o deis-control -g deis-control -m 600 "$STAGE/secret.key"       "$KEY"
install -o deis-control -g deis-control -m 600 "$STAGE/config.toml"      "$CONF"

# The machine identity, if this archive carries one. Restored 0600 and owned
# by the service user, like the master key: a private key readable by anything
# else on the host is that machine's identity handed over, and this one names
# the control plane.
if [ -d "$STAGE/certs" ]; then
    rm -rf "$CERTS"
    cp -a "$STAGE/certs" "$CERTS"
    chown -R deis-control:deis-control "$CERTS"
    chmod 700 "$CERTS"
    chmod 600 "$CERTS"/*
else
    echo "no machine identity in this archive; re-issue one at $CERTS before" \
         "starting, or no node or proxy will complete a handshake with this" \
         "control plane" >&2
fi

rm -rf "$BUNDLES"
cp -a "$STAGE/bundles" "$BUNDLES"
chown -R deis-control:deis-control "$BUNDLES"

# If the bundle store is not at the path it had on the source host, two
# things still name a machine that no longer exists: the path recorded with
# each version, and bundle_dir in the configuration that came out of the
# archive. Both are rewritten to $BUNDLES, which is where the operator
# running this script has already said the store now lives.
#
# Doing only the first is the trap: the restored versions would serve
# perfectly while every new upload was written to the old path, and nobody
# would find out until the next restore.
WAS=$(cat "$STAGE/bundle_dir.path")
if [ "$WAS" != "$BUNDLES" ]; then
    echo "bundle store moved from $WAS to $BUNDLES; rewriting the recorded paths"
    sqlite3 "$DB" \
      "UPDATE versions SET bundle_path = '$BUNDLES' || '/' || app_id || '/' || bundle_sha || '.zip';"

    if grep -qE '^[[:space:]]*bundle_dir[[:space:]]*=' "$CONF"; then
        sed -i -E "s|^[[:space:]]*bundle_dir[[:space:]]*=.*|bundle_dir = \"$BUNDLES\"|" "$CONF"
    else
        # It was commented out, so the old host was running on the default.
        printf '\nbundle_dir = "%s"\n' "$BUNDLES" >> "$CONF"
    fi
    grep -n '^bundle_dir' "$CONF"
fi

sudo systemctl start deis-control
```

### Verifying a restore

Run the readiness script from **Monitoring and Operations** above. `/health`
answering `OK` is not a verification of anything, and neither is the console
listing the applications: both look exactly the same on a restore that has
lost the bundle store and the master key.

This procedure was checked by restoring, on a control plane holding one
application, one version, one deployment and one secret. What each restore
produced:

Every row below was produced before the fleet moved to mutual TLS, so each of
them assumes a control plane a node can reach at all. Since that move there is
a row above all of them: **no machine identity restored** - `/health` answers
`200 OK`, the console lists everything, and not one node or proxy completes a
handshake, so nothing registers, nothing is deployed and no routing table is
fetched. That failure is invisible to every check in this table, which is why
the identity is in the script above and why the readiness script, not
`/health`, is what says a restore worked.

| Restore | `/health` | Versions listed | Node fetches the bundle | Node's assignment list |
|---|---|---|---|---|
| Database + configuration only (what this document used to say) | `200 OK` | yes | `500 recorded but missing on disk` | `[]` - the application is withheld, because the master key is gone and its environment cannot be built |
| All four, bundle store at a new path, paths not rewritten | `200 OK` | yes | `500 recorded but missing on disk` | complete, with the secret decrypted - so a node accepts the assignment and then fails to fetch the code |
| All four, bundle store back at its original path | `200 OK` | yes | `200`, the bundle | complete, with the secret decrypted |

The middle row is the one worth reading twice: every file was in the backup and
the restore still did not work, because a path in the database outlived the
machine it named.

---

## 🔄 Upgrade Guide

### Upgrade Steps

```bash
# 1. Backup - everything backup.sh takes, not just the database. An upgrade
#    that has to be rolled back needs the bundle store, the master key and
#    the machine identity as much as a restore from a lost machine does.
./backup.sh

# 2. Download new version
wget https://github.com/your-org/deis-control/releases/download/v0.2.0/deis-control-v0.2.0-linux-amd64.tar.gz
tar xzf deis-control-v0.2.0-linux-amd64.tar.gz

# 3. Stop service
sudo systemctl stop deis-control

# 4. Replace binary file
sudo cp deis-control-v0.2.0/deis-control /usr/local/bin/
sudo chmod 755 /usr/local/bin/deis-control

# 5. Start service
sudo systemctl start deis-control

# 6. Verify. /health only says the process is alive; the log is where a
#    configuration key the new version stopped accepting shows up, and the
#    readiness script is what says it can serve.
curl http://localhost:8080/health
sudo journalctl -u deis-control -n 50
./readiness.sh "$API_KEY" "https://localhost:8443" "$CLIENT_CERT" "$CLIENT_KEY" "$CA_CERT"
```

---

## 📚 Related Documents

- [ARCHITECTURE.md](ARCHITECTURE.md) - Architecture document
- [docs/trust-boundaries.md](../../docs/trust-boundaries.md) - what the node
  credential opens, which ports must be firewalled, the file permissions
  every configuration needs, and the rotation procedure
- [TLS_SETUP.md](TLS_SETUP.md) - TLS configuration guide
- [API_DOCUMENTATION.md](API_DOCUMENTATION.md) - API documentation
- [DEVELOPMENT.md](DEVELOPMENT.md) - Development guide

---

**Maintainers**: Control Team
**License**: Apache 2.0
