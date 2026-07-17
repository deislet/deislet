//! HTTP proxy handler implementation
//!
//! Handles HTTP request parsing, upstream selection, and response forwarding.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, error, warn};

use crate::config::system::{BalancingStrategy, UpstreamServer};
use crate::config::ConfigManager;

pub struct ProxyHandler {
    config_manager: Arc<ConfigManager>,
    round_robin_counter: AtomicUsize,
    http_client: reqwest::Client,
}

impl ProxyHandler {
    pub fn new(config_manager: Arc<ConfigManager>) -> Self {
        let http_client = reqwest::Client::builder()
            .pool_max_idle_per_host(32)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            config_manager,
            round_robin_counter: AtomicUsize::new(0),
            http_client,
        }
    }

    pub async fn handle_connection(&self, mut stream: TcpStream) -> Result<()> {
        let peer_addr = stream.peer_addr().ok();
        debug!("Handling connection from {:?}", peer_addr);

        let (reader, mut writer) = stream.split();
        let mut buf_reader = BufReader::new(reader);

        let request = match self.parse_request(&mut buf_reader).await {
            Ok(req) => req,
            Err(e) => {
                warn!("Failed to parse request: {}", e);
                let response = "HTTP/1.1 400 Bad Request\r\nContent-Length: 11\r\n\r\nBad Request";
                let _ = writer.write_all(response.as_bytes()).await;
                return Ok(());
            }
        };

        debug!(
            "Parsed request: {} {} from {:?}",
            request.method, request.path, peer_addr
        );

        let upstream = match self.select_upstream().await {
            Some(u) => u,
            None => {
                warn!("No upstream servers available");
                let response =
                    "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 22\r\n\r\nNo upstream available";
                let _ = writer.write_all(response.as_bytes()).await;
                return Ok(());
            }
        };

        debug!("Selected upstream: {}:{}", upstream.host, upstream.port);

        match self.forward_request(&request, &upstream).await {
            Ok(response) => {
                self.write_response(&mut writer, &response).await?;
            }
            Err(e) => {
                error!("Failed to forward request: {}", e);
                let response = format!(
                    "HTTP/1.1 502 Bad Gateway\r\nContent-Length: {}\r\n\r\n{}",
                    e.to_string().len(),
                    e
                );
                let _ = writer.write_all(response.as_bytes()).await;
            }
        }

        Ok(())
    }

    async fn parse_request<R: AsyncBufReadExt + Unpin>(
        &self,
        reader: &mut R,
    ) -> Result<HttpRequest> {
        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .await
            .context("Failed to read request line")?;

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 3 {
            anyhow::bail!("Invalid request line: {}", request_line.trim());
        }

        let method = parts[0].to_string();
        let path = parts[1].to_string();
        let version = parts[2].to_string();

        let mut headers = Vec::new();
        let mut content_length: usize = 0;
        let mut host = String::new();

        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .context("Failed to read header")?;

            if line.trim().is_empty() {
                break;
            }

            if let Some((name, value)) = line.trim().split_once(':') {
                let name = name.trim().to_lowercase();
                let value = value.trim().to_string();

                if name == "content-length" {
                    content_length = value.parse().unwrap_or(0);
                } else if name == "host" {
                    host = value.clone();
                }

                headers.push((name, value));
            }
        }

        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader
                .read_exact(&mut body)
                .await
                .context("Failed to read body")?;
        }

        Ok(HttpRequest {
            method,
            path,
            version,
            host,
            headers,
            body,
        })
    }

    async fn select_upstream(&self) -> Option<UpstreamServer> {
        let system_config = self.config_manager.get_system_config().await;
        let upstreams = system_config.upstreams?;

        if upstreams.is_empty() {
            return None;
        }

        let strategy = system_config
            .balancing_strategy
            .unwrap_or(BalancingStrategy::RoundRobin);

        match strategy {
            BalancingStrategy::RoundRobin => {
                let index = self.round_robin_counter.fetch_add(1, Ordering::Relaxed) % upstreams.len();
                Some(upstreams[index].clone())
            }
            _ => Some(upstreams[0].clone()),
        }
    }

    async fn forward_request(
        &self,
        request: &HttpRequest,
        upstream: &UpstreamServer,
    ) -> Result<ProxyResponse> {
        let url = format!(
            "http://{}:{}{}",
            upstream.host, upstream.port, request.path
        );

        debug!("Forwarding to: {}", url);

        let mut req_builder = match request.method.as_str() {
            "GET" => self.http_client.get(&url),
            "POST" => self.http_client.post(&url),
            "PUT" => self.http_client.put(&url),
            "DELETE" => self.http_client.delete(&url),
            "PATCH" => self.http_client.patch(&url),
            "HEAD" => self.http_client.head(&url),
            "OPTIONS" => self.http_client.request(reqwest::Method::OPTIONS, &url),
            _ => self.http_client.get(&url),
        };

        for (name, value) in &request.headers {
            if name != "host" && name != "connection" && name != "content-length" {
                req_builder = req_builder.header(name.as_str(), value.as_str());
            }
        }

        if !request.body.is_empty() {
            req_builder = req_builder.body(request.body.clone());
        }

        let response = req_builder.send().await.context("Failed to send request to upstream")?;

        let status = response.status().as_u16();
        let status_text = response.status().canonical_reason().unwrap_or("OK").to_string();

        let mut headers = Vec::new();
        for (name, value) in response.headers() {
            if let Ok(v) = value.to_str() {
                headers.push((name.to_string(), v.to_string()));
            }
        }

        let body = response.bytes().await.context("Failed to read response body")?;

        Ok(ProxyResponse {
            status,
            status_text,
            headers,
            body: body.to_vec(),
        })
    }

    async fn write_response<W: AsyncWriteExt + Unpin>(
        &self,
        writer: &mut W,
        response: &ProxyResponse,
    ) -> Result<()> {
        let status_line = format!("HTTP/1.1 {} {}\r\n", response.status, response.status_text);
        writer.write_all(status_line.as_bytes()).await?;

        let mut has_content_length = false;
        for (name, value) in &response.headers {
            if name.to_lowercase() == "content-length" {
                has_content_length = true;
            }
            if name.to_lowercase() != "transfer-encoding" {
                let header = format!("{}: {}\r\n", name, value);
                writer.write_all(header.as_bytes()).await?;
            }
        }

        if !has_content_length {
            let header = format!("Content-Length: {}\r\n", response.body.len());
            writer.write_all(header.as_bytes()).await?;
        }

        writer.write_all(b"\r\n").await?;
        writer.write_all(&response.body).await?;
        writer.flush().await?;

        Ok(())
    }
}

#[derive(Debug)]
#[allow(dead_code)]
struct HttpRequest {
    method: String,
    path: String,
    version: String,
    host: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Debug)]
struct ProxyResponse {
    status: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}
