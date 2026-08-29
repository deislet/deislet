# Control TLS/HTTPS Setup Guide

Control has built-in TLS support, enabling HTTPS for the HTTP API server.

## Quick Start

### 1. Generate Self-Signed Certificate (for testing)

```bash
openssl req -x509 -newkey rsa:4096 -nodes \
  -keyout key.pem -out cert.pem -days 365 \
  -subj "/CN=localhost"
```

### 2. Configure TLS

Add the following to `config.toml`:

```toml
[tls]
api_enabled = true   # one port carries the REST API and the node gRPC channel
cert_path = "cert.pem"
key_path = "key.pem"
```

### 3. Start Server

```bash
cargo run
```

The server will start in HTTPS mode:
```
🔒 TLS: ENABLED
📡 HTTPS API: https://localhost:8080
```

### 4. Testing

```bash
# Note: Self-signed certificates require the -k parameter to skip verification
curl -k https://localhost:8080/health
```

## Production Environment Configuration

### Option 1: Use Let's Encrypt (Recommended)

```bash
# 1. Install certbot
sudo apt install certbot  # Debian/Ubuntu
sudo yum install certbot  # CentOS/RHEL

# 2. Obtain certificate
sudo certbot certonly --standalone -d your-domain.com

# 3. Configure deis-control
```toml
[tls]
api_enabled = true
cert_path = "/etc/letsencrypt/live/your-domain.com/fullchain.pem"
key_path = "/etc/letsencrypt/live/your-domain.com/privkey.pem"
```

# 4. Set up automatic renewal
sudo crontab -e
# Add: 0 3 * * * certbot renew --quiet && systemctl restart deis-control
```

### Option 2: Use Reverse Proxy

#### Nginx Configuration

```nginx
server {
    listen 443 ssl http2;
    server_name deis-control.yourdomain.com;
    
    ssl_certificate /path/to/fullchain.pem;
    ssl_certificate_key /path/to/privkey.pem;
    
    # Modern TLS Configuration
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_ciphers HIGH:!aNULL:!MD5;
    ssl_prefer_server_ciphers on;
    
    # API Proxy
    location /api {
        proxy_pass http://localhost:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    # The node control channel is gRPC over HTTP/2 on the same port, not a
    # WebSocket upgrade. It needs a proxy that speaks h2c end to end;
    # terminating HTTP/1.1 in front of it breaks every node's channel.
    location /deis.control.ControlService/ {
        grpc_pass grpc://localhost:8080;
    }
}

# HTTP Redirect to HTTPS
server {
    listen 80;
    server_name deis-control.yourdomain.com;
    return 301 https://$server_name$request_uri;
}
```

#### Caddy Configuration (Automatic HTTPS)

```caddyfile
deis-control.yourdomain.com {
    # API Endpoints
    reverse_proxy /api/* localhost:8080

    # Node control channel: gRPC over HTTP/2, same port
    reverse_proxy /deis.control.ControlService/* h2c://localhost:8080

    
    # Caddy handles Let's Encrypt certificates automatically
}
```

## Certificate Hot Reload

The current version requires a service restart to load new certificates. Plans to support:

```bash
# Send SIGHUP signal to reload certificates
kill -HUP $(pidof deis-control)
```

## Security Best Practices

1. **Always use TLS 1.2+**
   - Control uses Rustls, which disables old protocols by default.

2. **Protect Private Keys**
   ```bash
   chmod 600 /path/to/privkey.pem
   chown deis-control:deis-control /path/to/privkey.pem
   ```

3. **Regularly Update Certificates**
   - Let's Encrypt certificates have a 90-day validity.
   - Set up automatic renewal.

4. **Use HSTS** (via reverse proxy)
   ```nginx
   add_header Strict-Transport-Security "max-age=31536000; includeSubDomains" always;
   ```

5. **Monitor Certificate Expiration**
   ```bash
   # Check certificate validity
   openssl x509 -in cert.pem -noout -enddate
   ```

## Troubleshooting

### Error: "Failed to build TLS config"

Check certificate format and paths:
```bash
# Verify certificate
openssl x509 -in cert.pem -text -noout

# Verify private key
openssl rsa -in key.pem -check
```

### Error: "TLS handshake failed"

1. Check if the certificate matches the domain.
2. Confirm time synchronization (certificate validity).
3. Check firewall rules.

### Node Control Channel Failure

- The channel is gRPC over HTTP/2, not a WebSocket. There is no `ws://` or
  `wss://` endpoint on this process at all.
- Behind a reverse proxy, confirm it forwards HTTP/2 (`grpc_pass` in Nginx,
  `h2c://` in Caddy) rather than downgrading to HTTP/1.1.

## Performance Optimization

1. **TLS Session Resumption**
   - Enabled by default in Rustls.

2. **HTTP/2**
   - Enable via reverse proxy.

3. **Certificate Chain Optimization**
   - Use the full certificate chain (fullchain.pem).

## API Authentication + TLS

Enabling both TLS and API Key authentication:

```toml
# Dual protection
api_auth_enabled = true
api_keys = ["your-secret-key"]

[tls]
api_enabled = true
cert_path = "cert.pem"
key_path = "key.pem"
```

```bash
curl -k -H "Authorization: Bearer your-secret-key" \
     https://localhost:8080/api/sessions
```

## Frequently Asked Questions

**Q: Is there a WebSocket endpoint to secure?**

A: No. This process serves the REST API and the node gRPC channel on one
port, and nothing else. Earlier versions of this document described a
WebSocket node link; that was never what shipped.

**Q: Can I provide both HTTP and HTTPS?**

A: The current version only allows choosing one. Recommendation:
- Development environment: HTTP
- Production environment: HTTPS + HTTP redirect (via reverse proxy)

**Q: Is client certificate authentication supported?**

A: Not supported in the current version. Plans to add mTLS support in future releases.

## Related Resources

- [Rustls Documentation](https://docs.rs/rustls/)
- [Let's Encrypt](https://letsencrypt.org/)
- [Mozilla SSL Configuration Generator](https://ssl-config.mozilla.org/)
