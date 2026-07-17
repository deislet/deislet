use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, warn};

use crate::message::WorkerType;
use crate::session::SessionManager;

#[derive(Clone)]
pub struct AppsState {
    pub sessions: Arc<SessionManager>,
    pub http_client: Client,
}

#[derive(Debug, Serialize)]
pub struct AppsResponse {
    pub apps: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

fn find_denix_worker(sessions: &SessionManager) -> Option<String> {
    for entry in sessions.iter() {
        let session = entry.value();
        if matches!(session.worker_type, WorkerType::Denix(_)) {
            if let Some(port) = session.ports.first() {
                let host = if session.host == "0.0.0.0" {
                    "127.0.0.1"
                } else {
                    &session.host
                };
                return Some(format!("http://{}:{}", host, port));
            }
        }
    }
    None
}

pub async fn list_apps(
    State(state): State<AppsState>,
) -> impl IntoResponse {
    let Some(denix_url) = find_denix_worker(&state.sessions) else {
        warn!("No Denix worker available for apps proxy");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "No Denix worker available".to_string(),
            }),
        )
            .into_response();
    };

    let url = format!("{}/api/apps", denix_url);
    match state.http_client.get(&url).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                match resp.json::<serde_json::Value>().await {
                    Ok(data) => (StatusCode::OK, Json(data)).into_response(),
                    Err(e) => {
                        error!("Failed to parse apps response: {}", e);
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(ErrorResponse {
                                error: format!("Failed to parse response: {}", e),
                            }),
                        )
                            .into_response()
                    }
                }
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                (
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    Json(ErrorResponse { error: body }),
                )
                    .into_response()
            }
        }
        Err(e) => {
            error!("Failed to proxy apps request: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: format!("Proxy error: {}", e),
                }),
            )
                .into_response()
        }
    }
}

pub async fn get_app(
    State(state): State<AppsState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(denix_url) = find_denix_worker(&state.sessions) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "No Denix worker available".to_string(),
            }),
        )
            .into_response();
    };

    let url = format!("{}/api/apps/{}", denix_url, id);
    match state.http_client.get(&url).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                match resp.json::<serde_json::Value>().await {
                    Ok(data) => (StatusCode::OK, Json(data)).into_response(),
                    Err(e) => {
                        error!("Failed to parse app response: {}", e);
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(ErrorResponse {
                                error: format!("Failed to parse response: {}", e),
                            }),
                        )
                            .into_response()
                    }
                }
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                (
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    Json(ErrorResponse { error: body }),
                )
                    .into_response()
            }
        }
        Err(e) => {
            error!("Failed to proxy get app request: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: format!("Proxy error: {}", e),
                }),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CreateAppRequest {
    pub id: String,
    pub code: String,
    #[serde(default)]
    pub subdomain: Option<String>,
}

pub async fn create_app(
    State(state): State<AppsState>,
    Json(payload): Json<CreateAppRequest>,
) -> impl IntoResponse {
    let Some(denix_url) = find_denix_worker(&state.sessions) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "No Denix worker available".to_string(),
            }),
        )
            .into_response();
    };

    let url = format!("{}/api/apps", denix_url);
    match state
        .http_client
        .post(&url)
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().is_success() {
                match resp.json::<serde_json::Value>().await {
                    Ok(data) => (StatusCode::CREATED, Json(data)).into_response(),
                    Err(e) => {
                        error!("Failed to parse create app response: {}", e);
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(ErrorResponse {
                                error: format!("Failed to parse response: {}", e),
                            }),
                        )
                            .into_response()
                    }
                }
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                (
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    Json(ErrorResponse { error: body }),
                )
                    .into_response()
            }
        }
        Err(e) => {
            error!("Failed to proxy create app request: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: format!("Proxy error: {}", e),
                }),
            )
                .into_response()
        }
    }
}

pub async fn delete_app(
    State(state): State<AppsState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(denix_url) = find_denix_worker(&state.sessions) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "No Denix worker available".to_string(),
            }),
        )
            .into_response();
    };

    let url = format!("{}/api/apps/{}", denix_url, id);
    match state.http_client.delete(&url).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                (StatusCode::NO_CONTENT, Json(serde_json::json!({}))).into_response()
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                (
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    Json(ErrorResponse { error: body }),
                )
                    .into_response()
            }
        }
        Err(e) => {
            error!("Failed to proxy delete app request: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: format!("Proxy error: {}", e),
                }),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UpdateDomainsRequest {
    pub custom_domains: Vec<String>,
}

pub async fn update_app_domains(
    State(state): State<AppsState>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateDomainsRequest>,
) -> impl IntoResponse {
    let Some(denix_url) = find_denix_worker(&state.sessions) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "No Denix worker available".to_string(),
            }),
        )
            .into_response();
    };

    let url = format!("{}/api/apps/{}/domains", denix_url, id);
    match state
        .http_client
        .put(&url)
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().is_success() {
                match resp.json::<serde_json::Value>().await {
                    Ok(data) => (StatusCode::OK, Json(data)).into_response(),
                    Err(e) => {
                        error!("Failed to parse update domains response: {}", e);
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(ErrorResponse {
                                error: format!("Failed to parse response: {}", e),
                            }),
                        )
                            .into_response()
                    }
                }
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                (
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    Json(ErrorResponse { error: body }),
                )
                    .into_response()
            }
        }
        Err(e) => {
            error!("Failed to proxy update domains request: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: format!("Proxy error: {}", e),
                }),
            )
                .into_response()
        }
    }
}
