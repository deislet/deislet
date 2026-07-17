# Denira Deployment Guide

**Version**: v0.1.0
**Last Updated**: 2026-01-11

---

## 📋 Pre-deployment Checklist

### System Requirements

- **Operating System**: Linux (Ubuntu 20.04+, CentOS 8+ recommended)
- **CPU**: 2+ cores
- **Memory**: 2GB+ (depending on the number of Workers)
- **Disk**: 10GB+ (for database and logs)
- **Network**: Network environment capable of accessing Worker nodes

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
git clone https://github.com/your-org/denira.git
cd denira

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
# Copy example configuration
cp config.example.toml config.toml

# Edit configuration
vi config.toml
```

**Minimal Configuration** (`config.toml`):
```toml
bind_addr = "0.0.0.0"
port = 8080

# Change to a strong random key
worker_preshared_keys = [
    "your-random-key-here-change-me-in-production"
]

# Enable API authentication (recommended)
api_auth_enabled = true
api_keys = [
    "your-api-key-change-me"
]
```

### 3. Initialize Database

```bash
# The database will be created automatically upon first startup
mkdir -p /var/lib/denira
```

### 4. Start Service

```bash
# Test startup
./target/release/denira \
  -c config.toml \
  --database /var/lib/denira/denira.db

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

COPY --from=builder /app/target/release/denira /usr/local/bin/
COPY config.toml /etc/denira/

VOLUME ["/var/lib/denira"]
EXPOSE 8080

CMD ["denira", "-c", "/etc/denira/config.toml", "--database", "/var/lib/denira/denira.db"]
```

### docker-compose.yml

```yaml
version: '3.8'

services:
  denira:
    build: .
    ports:
      - "8080:8080"  # WebSocket
      - "8080:8080"  # HTTP + gRPC
    volumes:
      - denira-data:/var/lib/denira
      - ./config.toml:/etc/denira/config.toml:ro
    restart: unless-stopped
    environment:
      - RUST_LOG=info

volumes:
  denira-data:
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
  -keyout /etc/denira/key.pem \
  -out /etc/denira/cert.pem \
  -days 365 \
  -subj "/CN=denira.yourdomain.com"
```

**Let's Encrypt certificate (production):**
```bash
# Install certbot
sudo apt install certbot

# Obtain certificate
sudo certbot certonly --standalone -d denira.yourdomain.com

# Configuration file
[tls]
api_enabled = true
ws_enabled = false
cert_path = "/etc/letsencrypt/live/denira.yourdomain.com/fullchain.pem"
key_path = "/etc/letsencrypt/live/denira.yourdomain.com/privkey.pem"
```

**Auto-renewal:**
```bash
# Add to crontab
sudo crontab -e

# Check and renew at 3 AM on the 1st of every month
0 3 1 * * certbot renew --quiet && systemctl restart denira
```

#### Option B: Nginx Reverse Proxy

**Nginx configuration** (`/etc/nginx/sites-available/denira`):
```nginx
server {
    listen 443 ssl http2;
    server_name denira.yourdomain.com;

    ssl_certificate /etc/letsencrypt/live/denira.yourdomain.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/denira.yourdomain.com/privkey.pem;

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

        # WebSocket support
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
    }

    # Worker WebSocket
    location /ws {
        proxy_pass http://localhost:8080;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
    }

    # Frontend static files
    location / {
        root /var/www/denira;
        try_files $uri $uri/ /index.html;
    }
}

# HTTP redirect to HTTPS
server {
    listen 80;
    server_name denira.yourdomain.com;
    return 301 https://$server_name$request_uri;
}
```

**Enable configuration:**
```bash
sudo ln -s /etc/nginx/sites-available/denira /etc/nginx/sites-enabled/
sudo nginx -t
sudo systemctl reload nginx
```

### 2. Systemd Service

Create service file (`/etc/systemd/system/denira.service`):

```ini
[Unit]
Description=Denira Cluster Supervisor
After=network.target

[Service]
Type=simple
User=denira
Group=denira
WorkingDirectory=/opt/denira
ExecStart=/usr/local/bin/denira \
  -c /etc/denira/config.toml \
  --database /var/lib/denira/denira.db

# Restart policy
Restart=always
RestartSec=10

# Resource limits
LimitNOFILE=65536
LimitNPROC=4096

# Logging
StandardOutput=journal
StandardError=journal
SyslogIdentifier=denira

[Install]
WantedBy=multi-user.target
```

**Start service:**
```bash
# Create user
sudo useradd -r -s /bin/false denira

# Create directories and set permissions
sudo mkdir -p /var/lib/denira /etc/denira
sudo chown denira:denira /var/lib/denira
sudo chmod 700 /var/lib/denira

# Install binary file
sudo cp target/release/denira /usr/local/bin/
sudo chown root:root /usr/local/bin/denira
sudo chmod 755 /usr/local/bin/denira

# Copy configuration
sudo cp config.toml /etc/denira/
sudo chown denira:denira /etc/denira/config.toml
sudo chmod 600 /etc/denira/config.toml

# Start service
sudo systemctl daemon-reload
sudo systemctl enable denira
sudo systemctl start denira

# View status
sudo systemctl status denira

# View logs
sudo journalctl -u denira -f
```

### 3. Firewall Configuration

```bash
# UFW (Ubuntu)
sudo ufw allow 8080/tcp  # WebSocket
sudo ufw allow 8080/tcp  # HTTP API
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
export RUST_LOG=denira=info,warn

# In systemd service
Environment="RUST_LOG=denira=info"
```

**Log rotation** (`/etc/logrotate.d/denira`):
```
/var/log/denira/*.log {
    daily
    rotate 7
    compress
    delaycompress
    missingok
    notifempty
    create 0640 denira denira
    sharedscripts
    postrotate
        systemctl reload denira
    endscript
}
```

### 2. Prometheus Monitoring

**Prometheus configuration** (`prometheus.yml`):
```yaml
scrape_configs:
  - job_name: 'denira'
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
    echo "Denira is healthy"
    exit 0
else
    echo "Denira is unhealthy (HTTP $response)"
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
denira -c /etc/denira/config.toml --check-config

# View detailed logs
RUST_LOG=debug denira -c config.toml
```

#### 2. Worker cannot connect

- Check network connectivity: `telnet denira-host 8080`
- Check firewall rules
- Verify consistent PSK configuration
- View worker-side logs

#### 3. Database locked

```bash
# Check database file permissions
ls -l /var/lib/denira/denira.db

# Check processes
sudo lsof /var/lib/denira/denira.db

# If recovery is needed
sqlite3 /var/lib/denira/denira.db ".backup /tmp/denira-backup.db"
```

#### 4. TLS certificate issues

```bash
# Verify certificate
openssl x509 -in /etc/denira/cert.pem -text -noout

# Check certificate validity period
openssl x509 -in /etc/denira/cert.pem -noout -enddate

# Test TLS connection
openssl s_client -connect denira.yourdomain.com:8080
```

---

## 📦 Backup and Recovery

### Backup

```bash
#!/bin/bash
# backup.sh

BACKUP_DIR="/var/backups/denira"
DATE=$(date +%Y%m%d_%H%M%S)

mkdir -p $BACKUP_DIR

# Backup database
sqlite3 /var/lib/denira/denira.db ".backup $BACKUP_DIR/denira-$DATE.db"

# Backup configuration
cp /etc/denira/config.toml $BACKUP_DIR/config-$DATE.toml

# Compress
tar czf $BACKUP_DIR/denira-backup-$DATE.tar.gz \
    $BACKUP_DIR/denira-$DATE.db \
    $BACKUP_DIR/config-$DATE.toml

# Clean up temporary files
rm $BACKUP_DIR/denira-$DATE.db
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
sudo systemctl stop denira

# Extract backup
tar xzf $BACKUP_FILE -C /tmp

# Restore database
cp /tmp/denira-*.db /var/lib/denira/denira.db
chown denira:denira /var/lib/denira/denira.db

# Restore configuration
cp /tmp/config-*.toml /etc/denira/config.toml
chown denira:denira /etc/denira/config.toml

# Start service
sudo systemctl start denira
```

---

## 🔄 Upgrade Guide

### Upgrade Steps

```bash
# 1. Backup
./backup.sh

# 2. Download new version
wget https://github.com/your-org/denira/releases/download/v0.2.0/denira-v0.2.0-linux-amd64.tar.gz
tar xzf denira-v0.2.0-linux-amd64.tar.gz

# 3. Stop service
sudo systemctl stop denira

# 4. Replace binary file
sudo cp denira-v0.2.0/denira /usr/local/bin/
sudo chmod 755 /usr/local/bin/denira

# 5. Check configuration compatibility
denira -c /etc/denira/config.toml --check-config

# 6. Start service
sudo systemctl start denira

# 7. Verify
curl http://localhost:8080/health
sudo journalctl -u denira -n 50
```

---

## 📚 Related Documents

- [ARCHITECTURE.md](ARCHITECTURE.md) - Architecture document
- [TLS_SETUP.md](TLS_SETUP.md) - TLS configuration guide
- [API_DOCUMENTATION.md](API_DOCUMENTATION.md) - API documentation
- [DEVELOPMENT.md](DEVELOPMENT.md) - Development guide

---

**Maintainers**: Denira Team
**License**: Apache 2.0
