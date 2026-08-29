# Control Deployment Guide

**Version**: v0.1.0
**Last Updated**: 2026-01-11

---

## 📋 Pre-deployment Checklist

### System Requirements

- **Operating System**: Linux (Ubuntu 20.04+, CentOS 8+ recommended)
- **CPU**: 2+ cores
- **Memory**: 2GB+ (depending on the number of Nodes)
- **Disk**: 10GB+ (for database and logs)
- **Network**: able to reach every node it manages, and reachable by them

### Software Dependencies

- Rust 1.70+ (for compilation)
- Node.js 18+ (for frontend build)
- SQLite 3.x
- (Optional) Nginx/Caddy (reverse proxy)
- (Optional) Certbot (Let's Encrypt certificates)

---

## 🚀 Quick Deployment

### 1. Compile Project

```bash
# Clone project
git clone https://github.com/your-org/deis-control.git
cd deis-control

# Compile Release version
cargo build --release

# Compile frontend
cd web
npm install
npm run build
cd ..
```

### 2. Prepare Configuration File

```bash
# Copy the example configuration (deis-control.example.toml is the only
# one; the file it is copied to can be named anything -c is given)
cp deis-control.example.toml config.toml

# Edit configuration
vi config.toml
```

**Minimal Configuration** (`config.toml`):
```toml
bind_addr = "0.0.0.0"
port = 8080

# The credential every runtime node presents: on the gRPC handshake, and
# on the two HTTP endpoints that serve nodes. Required in practice - with
# no key here the assignment list refuses to hand a node the decrypted
# secrets of any application, so applications with secrets do not run.
node_preshared_keys = [
    "your-random-key-here-change-me-in-production"
]

# Require an API key on the dashboard and the human API (recommended).
# This setting does not reach the node endpoints above.
api_auth_enabled = true
api_keys = [
    "your-api-key-change-me"
]
```

### 3. Initialize Database

```bash
# The database will be created automatically upon first startup
mkdir -p /var/lib/deis-control
```

### 4. Start Service

```bash
# Test startup
./target/release/deis-control \
  -c config.toml \
  --database /var/lib/deis-control/deis-control.db

# Verify service
curl http://localhost:8080/health
# Should return: OK
```

---

## 🐳 Docker Deployment

### Dockerfile

```dockerfile
FROM rust:1.70 as builder

WORKDIR /app
COPY . .
RUN cargo build --release

FROM ubuntu:22.04

RUN apt-get update && \
    apt-get install -y ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/deis-control /usr/local/bin/
COPY config.toml /etc/deis-control/

VOLUME ["/var/lib/deis-control"]
EXPOSE 8080

CMD ["deis-control", "-c", "/etc/deis-control/config.toml", "--database", "/var/lib/deis-control/deis-control.db"]
```

### docker-compose.yml

```yaml
version: '3.8'

services:
  deis-control:
    build: .
    ports:
      - "8080:8080"  # HTTP API + the node gRPC channel, one port
    volumes:
      - deis-control-data:/var/lib/deis-control
      - ./config.toml:/etc/deis-control/config.toml:ro
    restart: unless-stopped
    environment:
      - RUST_LOG=info

volumes:
  deis-control-data:
```

### Start

```bash
docker-compose up -d

# View logs
docker-compose logs -f

# Stop
docker-compose down
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

    # Frontend static files
    location / {
        root /var/www/deis-control;
        try_files $uri $uri/ /index.html;
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
WorkingDirectory=/opt/deis-control
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

# Create directories and set permissions
sudo mkdir -p /var/lib/deis-control /etc/deis-control
sudo chown deis-control:deis-control /var/lib/deis-control
sudo chmod 700 /var/lib/deis-control

# Install binary file
sudo cp target/release/deis-control /usr/local/bin/
sudo chown root:root /usr/local/bin/deis-control
sudo chmod 755 /usr/local/bin/deis-control

# Copy configuration
sudo cp config.toml /etc/deis-control/
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
sudo ufw allow 8080/tcp  # HTTP API + the node gRPC channel
sudo ufw allow 443/tcp   # HTTPS (if using Nginx)
sudo ufw enable

# firewalld (CentOS)
sudo firewall-cmd --permanent --add-port=8080/tcp
sudo firewall-cmd --permanent --add-port=8080/tcp
sudo firewall-cmd --permanent --add-port=443/tcp
sudo firewall-cmd --reload
```

---

## 📊 Monitoring and Operations

### 1. Log Management

**Configure log level:**
```bash
# Environment variable method
export RUST_LOG=deis-control=info,warn

# In systemd service
Environment="RUST_LOG=deis-control=info"
```

**Log rotation** (`/etc/logrotate.d/deis-control`):
```
/var/log/deis-control/*.log {
    daily
    rotate 7
    compress
    delaycompress
    missingok
    notifempty
    create 0640 deis-control deis-control
    sharedscripts
    postrotate
        systemctl reload deis-control
    endscript
}
```

### 2. Prometheus Monitoring

**Prometheus configuration** (`prometheus.yml`):
```yaml
scrape_configs:
  - job_name: 'deis-control'
    static_configs:
      - targets: ['localhost:8080']
    metrics_path: '/api/metrics'
```

### 3. Health Check

**Script example** (`healthcheck.sh`):
```bash
#!/bin/bash
response=$(curl -s -o /dev/null -w "%{http_code}" http://localhost:8080/health)

if [ "$response" -eq 200 ]; then
    echo "Control is healthy"
    exit 0
else
    echo "Control is unhealthy (HTTP $response)"
    exit 1
fi
```

---

## 🔧 Troubleshooting

### Common Issues

#### 1. Service fails to start

```bash
# Check port occupation
sudo lsof -i :8080
sudo lsof -i :8080

# Check configuration file
deis-control -c /etc/deis-control/config.toml --check-config

# View detailed logs
RUST_LOG=debug deis-control -c config.toml
```

#### 2. Node cannot connect

- Check network connectivity: `telnet deis-control-host 8080`
- Check firewall rules
- Verify consistent PSK configuration
- View node-side logs

#### 3. Database locked

```bash
# Check database file permissions
ls -l /var/lib/deis-control/deis-control.db

# Check processes
sudo lsof /var/lib/deis-control/deis-control.db

# If recovery is needed
sqlite3 /var/lib/deis-control/deis-control.db ".backup /tmp/deis-control-backup.db"
```

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

### Backup

```bash
#!/bin/bash
# backup.sh

BACKUP_DIR="/var/backups/deis-control"
DATE=$(date +%Y%m%d_%H%M%S)

mkdir -p $BACKUP_DIR

# Backup database
sqlite3 /var/lib/deis-control/deis-control.db ".backup $BACKUP_DIR/deis-control-$DATE.db"

# Backup configuration
cp /etc/deis-control/config.toml $BACKUP_DIR/config-$DATE.toml

# Compress
tar czf $BACKUP_DIR/deis-control-backup-$DATE.tar.gz \
    $BACKUP_DIR/deis-control-$DATE.db \
    $BACKUP_DIR/config-$DATE.toml

# Clean up temporary files
rm $BACKUP_DIR/deis-control-$DATE.db
rm $BACKUP_DIR/config-$DATE.toml

# Retain backups from the last 30 days
find $BACKUP_DIR -name "*.tar.gz" -mtime +30 -delete
```

### Recovery

```bash
#!/bin/bash
# restore.sh

BACKUP_FILE=$1

if [ -z "$BACKUP_FILE" ]; then
    echo "Usage: $0 <backup_file.tar.gz>"
    exit 1
fi

# Stop service
sudo systemctl stop deis-control

# Extract backup
tar xzf $BACKUP_FILE -C /tmp

# Restore database
cp /tmp/deis-control-*.db /var/lib/deis-control/deis-control.db
chown deis-control:deis-control /var/lib/deis-control/deis-control.db

# Restore configuration
cp /tmp/config-*.toml /etc/deis-control/config.toml
chown deis-control:deis-control /etc/deis-control/config.toml

# Start service
sudo systemctl start deis-control
```

---

## 🔄 Upgrade Guide

### Upgrade Steps

```bash
# 1. Backup
./backup.sh

# 2. Download new version
wget https://github.com/your-org/deis-control/releases/download/v0.2.0/deis-control-v0.2.0-linux-amd64.tar.gz
tar xzf deis-control-v0.2.0-linux-amd64.tar.gz

# 3. Stop service
sudo systemctl stop deis-control

# 4. Replace binary file
sudo cp deis-control-v0.2.0/deis-control /usr/local/bin/
sudo chmod 755 /usr/local/bin/deis-control

# 5. Check configuration compatibility
deis-control -c /etc/deis-control/config.toml --check-config

# 6. Start service
sudo systemctl start deis-control

# 7. Verify
curl http://localhost:8080/health
sudo journalctl -u deis-control -n 50
```

---

## 📚 Related Documents

- [ARCHITECTURE.md](ARCHITECTURE.md) - Architecture document
- [TLS_SETUP.md](TLS_SETUP.md) - TLS configuration guide
- [API_DOCUMENTATION.md](API_DOCUMENTATION.md) - API documentation
- [DEVELOPMENT.md](DEVELOPMENT.md) - Development guide

---

**Maintainers**: Control Team
**License**: Apache 2.0
