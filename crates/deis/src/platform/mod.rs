//! The control plane's HTTP API, as this CLI uses it.
//!
//! Every type here mirrors what `deis-control` already serves, with one
//! documented exception: [`Control::storage_credentials`] names a route
//! `deis-control` does not yet have, needed so `deis kv`, `deis db` and `deis
//! blob` can reach deis-store the way every other platform command reaches
//! its service - through the credential already on file, not a secret pasted
//! by hand. Its doc comment is the contract; see there before assuming it
//! works against a running platform.
//!
//! Every other field the CLI prints is deserialised straight off what the
//! server already returns, so an added field there does not break an older
//! CLI.

pub mod storage;
pub mod telemetry;

use anyhow::{Context, Result, bail};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};

/// How long one control-plane call may take.
///
/// Generous because a deploy pushes to every node before it answers, and a
/// node that is slow to load a version is not a reason to abandon a deploy
/// that has already been recorded.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// An application as the catalog records it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct App {
    pub id: String,
    pub created_at: i64,
}

/// One immutable version.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Version {
    pub id: String,
    pub app_id: String,
    /// The per-application counter a person says out loud: "v3".
    pub number: i64,
    pub bundle_sha: String,
    pub created_at: i64,
    #[serde(default)]
    pub manifest: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionsResponse {
    pub versions: Vec<Version>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UploadResponse {
    pub version: Version,
    /// False when a byte-identical bundle was already stored, in which case
    /// the existing version is returned unchanged and no new number is used.
    pub created: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Environment {
    pub id: String,
    pub app_id: String,
    pub name: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnvironmentsResponse {
    pub environments: Vec<Environment>,
}

/// One pointer row: this environment ran this version from this moment.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Deployment {
    pub id: String,
    pub app_id: String,
    pub env_id: String,
    pub version_id: String,
    pub created_at: i64,
    #[serde(default)]
    pub note: Option<String>,
}

/// What a deploy, a rollback or a read of the current pointer answers.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeploymentResponse {
    pub deployment: Deployment,
    pub environment: String,
    pub version: Version,
    /// Absent when nothing was pushed, which is the case for a plain read.
    #[serde(default)]
    pub delivery: Option<DeliveryReport>,
}

/// Where a version actually landed.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeliveryReport {
    pub reason: String,
    pub app_id: String,
    pub environment: String,
    pub version_id: String,
    /// Nodes that applied the version and now serve it.
    #[serde(default)]
    pub delivered: Vec<String>,
    /// Nodes that did not. They keep serving whatever they had.
    #[serde(default)]
    pub failed: Vec<NodeFailure>,
    /// Set when nothing was pushed at all, with the reason.
    #[serde(default)]
    pub skipped: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeFailure {
    pub node: String,
    pub error: String,
}

/// A secret's metadata. There is deliberately no field for the value: the
/// control plane has no endpoint that returns one.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecretMeta {
    pub name: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SecretsResponse {
    pub secrets: Vec<SecretMeta>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecretChangeResponse {
    pub secret: SecretMeta,
    pub delivery: DeliveryReport,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecretDeletedResponse {
    pub deleted: String,
    pub delivery: DeliveryReport,
}

/// What `deis kv`, `deis db` and `deis blob` need to reach deis-store as one
/// tenant.
///
/// Both fields are optional for the same reason they are optional in
/// `deis-control`'s own `delivery::StorageWiring`: a platform with no
/// deis-store configured has no `store_endpoint`, and a deis-store bound to
/// loopback with no `auth_secret` needs no `token` at all - the tenant header
/// alone is enough there, which is why `token` absent is a normal answer and
/// not a failure to authenticate.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageCredentials {
    #[serde(default)]
    pub store_endpoint: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
}

/// One runtime node as the control plane knows it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Node {
    pub id: String,
    pub addr: String,
    #[serde(default)]
    pub healthy: bool,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodesResponse {
    pub nodes: Vec<Node>,
}

/// One (application, environment) as one node reports it right now.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeAppStatus {
    pub app_id: String,
    pub environment: String,
    pub state: String,
    #[serde(default)]
    pub request_count: i64,
    #[serde(default)]
    pub error_count: i64,
    #[serde(default)]
    pub error_message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodeStatusResponse {
    #[serde(default)]
    pub apps: Vec<NodeAppStatus>,
}

/// A client for one control plane.
pub struct Control {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Control {
    pub fn new(base: &str, token: Option<&str>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.map(|t| t.to_string()),
            http,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self.http.request(method, self.url(path));
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    /// Send a request and read its body as `T`, turning every failure into a
    /// message that says what to do next.
    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        builder: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<T> {
        let response = builder.send().await.map_err(|e| self.transport_error(e))?;
        let status = response.status();
        let body = response.bytes().await.unwrap_or_default();
        if !status.is_success() {
            bail!(self.http_error(status, &body, what));
        }
        serde_json::from_slice(&body).with_context(|| {
            format!(
                "{what}: the control plane answered {status} with a body this CLI could not read; \
                 it may be a newer or older version than this build"
            )
        })
    }

    fn transport_error(&self, e: reqwest::Error) -> anyhow::Error {
        anyhow::anyhow!(
            "could not reach the control plane at {}: {e}\nCheck that deis-control is running and \
             that the address is right (see `deis config list`).",
            self.base
        )
    }

    fn http_error(&self, status: StatusCode, body: &[u8], what: &str) -> String {
        let detail = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
        let detail = if detail.is_empty() {
            "no detail given".to_string()
        } else {
            detail
        };

        let advice = match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => match self.token.is_some() {
                true => "\nThe credential this CLI sent was refused. Store the right one with \
                         `deis config set control.token <value>`."
                    .to_string(),
                false => format!(
                    "\nNo credential was sent. Store one with `deis config set control.token \
                     <value>`, or set ${} for this shell.",
                    crate::settings::ENV_CONTROL_TOKEN
                ),
            },
            StatusCode::NOT_FOUND => {
                // Every 404 this CLI can provoke means the same thing: it
                // named an application, environment or version the catalog
                // does not have. Two commands list all three.
                "\nThe control plane has no such record. `deis versions` lists an application's \
                 versions; `deis status` lists its environments and what each runs."
                    .to_string()
            }
            StatusCode::SERVICE_UNAVAILABLE => {
                "\nThe control plane is missing something it needs - a secret master key, most \
                 often. Its own log says which."
                    .to_string()
            }
            _ => String::new(),
        };

        format!("{what} failed: the control plane answered {status}: {detail}{advice}")
    }

    // ---- applications -------------------------------------------------

    pub async fn get_app(&self, app_id: &str) -> Result<Option<App>> {
        let response = self
            .request(Method::GET, &format!("/api/apps/{app_id}"))
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = response.status();
        let body = response.bytes().await.unwrap_or_default();
        if !status.is_success() {
            bail!(self.http_error(status, &body, format!("looking up app {app_id}").as_str()));
        }
        Ok(Some(serde_json::from_slice(&body).with_context(|| {
            format!("reading the description of app {app_id}")
        })?))
    }

    pub async fn create_app(&self, app_id: &str) -> Result<App> {
        self.send(
            self.request(Method::POST, "/api/apps")
                .json(&serde_json::json!({ "id": app_id })),
            &format!("creating app {app_id}"),
        )
        .await
    }

    /// Create the application unless it is already there. Returns true when
    /// this call is what created it.
    pub async fn ensure_app(&self, app_id: &str) -> Result<bool> {
        if self.get_app(app_id).await?.is_some() {
            return Ok(false);
        }
        self.create_app(app_id).await?;
        Ok(true)
    }

    // ---- versions -----------------------------------------------------

    /// Upload a bundle as a new version.
    ///
    /// `sha256` is sent along so the control plane refuses a truncated upload
    /// instead of recording it as a version that no node can load.
    pub async fn upload_version(
        &self,
        app_id: &str,
        bundle: Vec<u8>,
        sha256: &str,
    ) -> Result<UploadResponse> {
        self.send(
            self.request(
                Method::POST,
                &format!("/api/apps/{app_id}/versions?sha256={sha256}"),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/zip")
            .body(bundle),
            &format!("uploading a bundle for app {app_id}"),
        )
        .await
    }

    pub async fn list_versions(&self, app_id: &str) -> Result<Vec<Version>> {
        let response: VersionsResponse = self
            .send(
                self.request(Method::GET, &format!("/api/apps/{app_id}/versions")),
                &format!("listing the versions of app {app_id}"),
            )
            .await?;
        Ok(response.versions)
    }

    // ---- environments -------------------------------------------------

    pub async fn list_environments(&self, app_id: &str) -> Result<Vec<Environment>> {
        let response: EnvironmentsResponse = self
            .send(
                self.request(Method::GET, &format!("/api/apps/{app_id}/environments")),
                &format!("listing the environments of app {app_id}"),
            )
            .await?;
        Ok(response.environments)
    }

    /// Create the environment unless it is already there. Returns true when
    /// this call is what created it.
    pub async fn ensure_environment(&self, app_id: &str, environment: &str) -> Result<bool> {
        let response = self
            .request(
                Method::GET,
                &format!("/api/apps/{app_id}/environments/{environment}"),
            )
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if response.status().is_success() {
            return Ok(false);
        }
        if response.status() != StatusCode::NOT_FOUND {
            let status = response.status();
            let body = response.bytes().await.unwrap_or_default();
            bail!(self.http_error(
                status,
                &body,
                &format!("looking up environment {environment} of app {app_id}")
            ));
        }
        let _: Environment = self
            .send(
                self.request(Method::POST, &format!("/api/apps/{app_id}/environments"))
                    .json(&serde_json::json!({ "name": environment })),
                &format!("creating environment {environment} of app {app_id}"),
            )
            .await?;
        Ok(true)
    }

    // ---- deployments --------------------------------------------------

    pub async fn deploy(
        &self,
        app_id: &str,
        environment: &str,
        version_id: &str,
        note: Option<&str>,
    ) -> Result<DeploymentResponse> {
        let mut body = serde_json::json!({ "version_id": version_id });
        if let Some(note) = note {
            body["note"] = serde_json::Value::String(note.to_string());
        }
        self.send(
            self.request(
                Method::POST,
                &format!("/api/apps/{app_id}/environments/{environment}/deployments"),
            )
            .json(&body),
            &format!("pointing {environment} of app {app_id} at a version"),
        )
        .await
    }

    /// Move the pointer backwards. `version_number` unset means "the previous
    /// distinct version this environment ran".
    pub async fn rollback(
        &self,
        app_id: &str,
        environment: &str,
        version_number: Option<i64>,
        note: Option<&str>,
    ) -> Result<DeploymentResponse> {
        let mut body = serde_json::Map::new();
        if let Some(number) = version_number {
            body.insert("version_number".to_string(), serde_json::json!(number));
        }
        if let Some(note) = note {
            body.insert("note".to_string(), serde_json::json!(note));
        }
        self.send(
            self.request(
                Method::POST,
                &format!("/api/apps/{app_id}/environments/{environment}/rollback"),
            )
            .json(&serde_json::Value::Object(body)),
            &format!("rolling {environment} of app {app_id} back"),
        )
        .await
    }

    /// The version an environment currently points at. `None` when it has
    /// never been deployed, which is a state and not an error.
    pub async fn current_deployment(
        &self,
        app_id: &str,
        environment: &str,
    ) -> Result<Option<DeploymentResponse>> {
        let response = self
            .request(
                Method::GET,
                &format!("/api/apps/{app_id}/environments/{environment}/deployment"),
            )
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = response.status();
        let body = response.bytes().await.unwrap_or_default();
        if !status.is_success() {
            bail!(self.http_error(
                status,
                &body,
                &format!("reading what {environment} of app {app_id} runs")
            ));
        }
        Ok(Some(serde_json::from_slice(&body).with_context(|| {
            format!("reading what {environment} of app {app_id} runs")
        })?))
    }

    // ---- secrets ------------------------------------------------------

    pub async fn put_secret(
        &self,
        app_id: &str,
        environment: &str,
        name: &str,
        value: &str,
    ) -> Result<SecretChangeResponse> {
        self.send(
            self.request(
                Method::PUT,
                &format!("/api/apps/{app_id}/environments/{environment}/secrets/{name}"),
            )
            .json(&serde_json::json!({ "value": value })),
            &format!("storing secret {name} of {environment}"),
        )
        .await
    }

    pub async fn list_secrets(&self, app_id: &str, environment: &str) -> Result<Vec<SecretMeta>> {
        let response: SecretsResponse = self
            .send(
                self.request(
                    Method::GET,
                    &format!("/api/apps/{app_id}/environments/{environment}/secrets"),
                ),
                &format!("listing the secrets of {environment} of app {app_id}"),
            )
            .await?;
        Ok(response.secrets)
    }

    pub async fn delete_secret(
        &self,
        app_id: &str,
        environment: &str,
        name: &str,
    ) -> Result<SecretDeletedResponse> {
        self.send(
            self.request(
                Method::DELETE,
                &format!("/api/apps/{app_id}/environments/{environment}/secrets/{name}"),
            ),
            &format!("deleting secret {name} of {environment}"),
        )
        .await
    }

    // ---- storage --------------------------------------------------------

    /// The address and capability token `deis kv`, `deis db` and `deis blob`
    /// need to act as one tenant of deis-store.
    ///
    /// **This route does not exist on `deis-control` yet.** It belongs beside
    /// the other `/api/apps/:app_id/environments/:environment/*` routes,
    /// behind the same bearer-token middleware as the rest of this file, and
    /// its handler is a thin read of what `deis-control` already computes for
    /// a deployment: `delivery::StorageWiring::from_config(&config).store`
    /// for the address and `.token_for(app_id, environment)` for the token -
    /// see that type's doc comment for why each is optional. Minting the
    /// token there rather than handing this CLI the shared `auth_secret`
    /// keeps the secret itself on the control plane and the store, which is
    /// the whole point of a capability token: a leaked CLI credential (the
    /// bearer token in `credentials.toml`) still only ever proves the holder
    /// may deploy and inspect *through the control plane's own checks*, never
    /// hands out the key every tenant's token is derived from.
    ///
    /// Until that route exists, every call here fails with a 404 that the
    /// generic error path below reports as "the control plane answered 404";
    /// that is expected and is not a bug in this CLI.
    pub async fn storage_credentials(
        &self,
        app_id: &str,
        environment: &str,
    ) -> Result<StorageCredentials> {
        self.send(
            self.request(
                Method::GET,
                &format!("/api/apps/{app_id}/environments/{environment}/storage-token"),
            ),
            &format!("obtaining a storage capability token for {environment} of app {app_id}"),
        )
        .await
    }

    // ---- nodes --------------------------------------------------------

    /// Every runtime node this control plane knows about.
    pub async fn list_nodes(&self) -> Result<Vec<Node>> {
        let response: NodesResponse = self
            .send(
                self.request(Method::GET, "/api/runtime"),
                "listing the runtime nodes",
            )
            .await?;
        Ok(response.nodes)
    }

    /// What one node reports it is running.
    ///
    /// Answers `None` rather than failing when that one node cannot be
    /// reached: a fleet report is more useful with a hole in it than not at
    /// all, and the hole is printed.
    pub async fn node_status(&self, node: &str) -> Result<Option<Vec<NodeAppStatus>>> {
        let response = self
            .request(
                Method::GET,
                &format!("/api/runtime/{node}/status?apps=true"),
            )
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(_) => return Ok(None),
        };
        if !response.status().is_success() {
            return Ok(None);
        }
        let body = response.bytes().await.unwrap_or_default();
        match serde_json::from_slice::<NodeStatusResponse>(&body) {
            Ok(status) => Ok(Some(status.apps)),
            Err(_) => Ok(None),
        }
    }
}
