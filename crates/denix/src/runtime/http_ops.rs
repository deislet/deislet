// HTTP Client Ops 定义
// 提供 fetch API 功能，允许 JavaScript 应用调用外部 HTTP 服务

use deno_core::{extension, op2, OpState};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

/// HTTP 请求结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchRequest {
    pub url: String,
    pub method: String,
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
    pub timeout: Option<u64>, // 超时时间（秒）
}

/// HTTP 响应结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

/// Fetch 操作
///
/// 发送 HTTP 请求到外部服务
///
/// # 参数
/// - request_json: 请求对象的 JSON 字符串
///
/// # 返回
/// - JSON 对象: { status: number, statusText: string, headers: Object, body: string }
///   或 { error: string }
#[op2(async)]
#[serde]
async fn op_fetch(
    _state: Rc<RefCell<OpState>>,
    #[string] request_json: String,
) -> JsonValue {
    // 解析请求
    let request: FetchRequest = match serde_json::from_str(&request_json) {
        Ok(req) => req,
        Err(e) => {
            return serde_json::json!({
                "error": format!("Invalid request JSON: {}", e)
            });
        }
    };

    // 验证 URL
    if request.url.is_empty() {
        return serde_json::json!({
            "error": "URL cannot be empty"
        });
    }

    // 创建 HTTP 客户端
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(request.timeout.unwrap_or(30)))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return serde_json::json!({
                "error": format!("Failed to create HTTP client: {}", e)
            });
        }
    };

    // 构建请求
    let method = match request.method.to_uppercase().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        "OPTIONS" => reqwest::Method::OPTIONS,
        _ => {
            return serde_json::json!({
                "error": format!("Unsupported HTTP method: {}", request.method)
            });
        }
    };

    let mut req_builder = client.request(method, &request.url);

    // 添加请求头
    for (key, value) in &request.headers {
        req_builder = req_builder.header(key, value);
    }

    // 添加请求体
    if let Some(body) = &request.body {
        req_builder = req_builder.body(body.clone());
    }

    // 发送请求
    let response = match req_builder.send().await {
        Ok(resp) => resp,
        Err(e) => {
            return serde_json::json!({
                "error": format!("HTTP request failed: {}", e)
            });
        }
    };

    // 提取响应信息
    let status = response.status().as_u16();
    let status_text = response.status().canonical_reason().unwrap_or("").to_string();

    // 提取响应头
    let mut headers = HashMap::new();
    for (key, value) in response.headers() {
        if let Ok(value_str) = value.to_str() {
            headers.insert(key.to_string(), value_str.to_string());
        }
    }

    // 读取响应体
    let body = match response.text().await {
        Ok(text) => text,
        Err(e) => {
            return serde_json::json!({
                "error": format!("Failed to read response body: {}", e)
            });
        }
    };

    // 返回响应
    serde_json::json!({
        "status": status,
        "statusText": status_text,
        "headers": headers,
        "body": body,
    })
}

// 定义 http extension
extension!(
    denix_http_ops,
    ops = [op_fetch],
);

/// 初始化 http ops extension
pub fn init_http_extension() -> deno_core::Extension {
    denix_http_ops::init()
}

#[cfg(test)]
#[path = "http_ops_test.rs"]
mod http_ops_test;
