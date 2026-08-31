//! The control plane's HTTP API, as this CLI uses it.
//!
//! Every type here mirrors what `deis-control` already serves, and every
//! field the CLI prints is deserialised straight off what the server
//! returns, so an added field there does not break an older CLI.

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
    /// Which day's platform this version was written against, and which
    /// behaviours that leaves on, as the control plane resolved it out of the
    /// manifest it stored.
    ///
    /// `None` covers two cases that read the same to a person: a control plane
    /// too old to say, and a stored manifest it could not resolve. Both are
    /// printed as "unknown", which is the honest answer - what is never
    /// printed as unknown is a version that simply declares no date, because
    /// that one comes back as `Some` with a null date and is exactly what an
    /// operator is looking for.
    #[serde(default)]
    pub compatibility: Option<deis_bundle::CompatibilityProfile>,
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
/// Both fields stay optional even though the control plane that serves this
/// route today refuses rather than answering with either one missing: this
/// is a deserialisation target for whatever a control plane sends, and an
/// older or newer one that answers with only an address must be reported by
/// the message below rather than by a parse failure that names a field.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageCredentials {
    #[serde(default)]
    pub store_endpoint: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
}

/// What this credential can see of one environment's current deployment.
///
/// A token scoped to one environment of an application can list that
/// application's environments and is refused a look at the others, so "I may
/// not see this one" is an ordinary answer here rather than a failure of the
/// whole command. `deis status` and `deis versions` walk every environment an
/// application has, and both would stop at the first refusal if this were an
/// error; instead they print what the credential can see and say which rows
/// it cannot.
#[derive(Debug, Clone)]
pub enum EnvironmentView {
    /// It runs this version. Boxed because it is much larger than the other
    /// two variants and every caller holds a list of these.
    Running(Box<DeploymentResponse>),
    /// It exists and has never been deployed. A state, not an error.
    NeverDeployed,
    /// The credential in hand does not reach this environment.
    NotPermitted,
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

/// One account, as the control plane records it.
///
/// The role is the ceiling on everything this account's tokens may do, so it
/// is the first thing a listing shows: a token can only ever be narrower.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub role: String,
    pub created_at: i64,
    /// Set while the account is switched off. Its tokens are refused and not
    /// revoked, so switching it back on restores exactly those credentials.
    #[serde(default)]
    pub disabled_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AccountsResponse {
    pub accounts: Vec<Account>,
}

/// One credential, as a listing shows it. There is deliberately no field a
/// secret could arrive in: the control plane stores a digest and has no
/// endpoint that reads a token back.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiToken {
    pub id: String,
    pub account: String,
    pub name: String,
    /// `minted` for a token this control plane issued, `config` for an
    /// `api_keys` entry in its configuration file.
    pub origin: String,
    /// The scope list, or the word `unscoped`.
    pub scopes: String,
    /// True when this credential answers to no scope at all - the old flat
    /// key. Read as a field rather than by recognising the word above.
    #[serde(default)]
    pub unscoped: bool,
    pub created_at: i64,
    #[serde(default)]
    pub last_used_at: Option<i64>,
    #[serde(default)]
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokensResponse {
    pub tokens: Vec<ApiToken>,
    /// How many of them are unscoped, counted by the control plane so every
    /// reader gets the same number.
    #[serde(default)]
    pub unscoped: usize,
}

/// What minting a token answers. The only message in this protocol that
/// carries a credential.
#[derive(Debug, Clone, Deserialize)]
pub struct MintedToken {
    pub token: ApiToken,
    pub secret: String,
}

/// One line of the audit log.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub at: i64,
    pub actor_kind: String,
    #[serde(default)]
    pub actor_account: Option<String>,
    #[serde(default)]
    pub actor_credential: Option<String>,
    pub action: String,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub peer: Option<String>,
    pub outcome: String,
    pub status: i64,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuditResponse {
    pub entries: Vec<AuditEntry>,
}

/// What `deis audit` is asking for. Every field is optional and they are
/// combined with "and", which is how an operator narrows a log down to the
/// one thing they are chasing.
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub account: Option<String>,
    pub action: Option<String>,
    pub outcome: Option<String>,
    pub since: Option<i64>,
    pub limit: Option<usize>,
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
            // 401 and 403 are different problems and lead to different next
            // steps, so they are not one arm: the first is a credential that
            // does not work at all, the second is one that works and does not
            // reach this far. The control plane's own message already names
            // the scope that would have been enough, so this only says who to
            // ask.
            StatusCode::UNAUTHORIZED => match self.token.is_some() {
                true => "\nThe credential this CLI sent was refused: it is unknown to this \
                         control plane, or it was revoked, or its account is switched off. Store \
                         a working one with `deis config set control.token <value>`."
                    .to_string(),
                false => format!(
                    "\nNo credential was sent. Store one with `deis config set control.token \
                     <value>`, or set ${} for this shell.",
                    crate::settings::ENV_CONTROL_TOKEN
                ),
            },
            StatusCode::FORBIDDEN => "\nThis credential works and is not allowed to do that. Ask \
                                      an owner of the platform for a token carrying the scope \
                                      named above (`deis token create`), or for a wider role on \
                                      your account."
                .to_string(),
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

    /// The version an environment currently points at, or why this credential
    /// cannot say.
    pub async fn current_deployment(
        &self,
        app_id: &str,
        environment: &str,
    ) -> Result<EnvironmentView> {
        let response = self
            .request(
                Method::GET,
                &format!("/api/apps/{app_id}/environments/{environment}/deployment"),
            )
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(EnvironmentView::NeverDeployed);
        }
        // Not an error: a credential scoped to one environment is *meant* to
        // be refused the others, and a command that walks every environment
        // of an application must be able to say so rather than stop.
        if response.status() == StatusCode::FORBIDDEN {
            return Ok(EnvironmentView::NotPermitted);
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
        Ok(EnvironmentView::Running(Box::new(
            serde_json::from_slice(&body)
                .with_context(|| format!("reading what {environment} of app {app_id} runs"))?,
        )))
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
    /// Served by `deis-control`'s
    /// `api::catalog::storage::storage_token`, behind the same API-key
    /// middleware as the deploy route and gated on the same application and
    /// environment having to exist. The token is minted there rather than
    /// this CLI being handed the shared `auth_secret`, which is the whole
    /// point of a capability token: a leaked CLI credential still only ever
    /// proves the holder may deploy and inspect *through the control plane's
    /// own checks*, and never hands out the key every tenant's token is
    /// derived from.
    ///
    /// A control plane with no deis-store address or no `storage_auth_secret`
    /// answers 409 with a message naming the key to set, which the generic
    /// error path below prints as it stands: the caller is told what to fix
    /// rather than handed an empty token that fails later inside deis-store.
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

    // ---- accounts, tokens and the audit log ---------------------------

    pub async fn list_accounts(&self) -> Result<Vec<Account>> {
        let response: AccountsResponse = self
            .send(
                self.request(Method::GET, "/api/accounts"),
                "listing the accounts",
            )
            .await?;
        Ok(response.accounts)
    }

    pub async fn create_account(&self, name: &str, role: &str) -> Result<Account> {
        self.send(
            self.request(Method::POST, "/api/accounts")
                .json(&serde_json::json!({ "name": name, "role": role })),
            &format!("creating account {name}"),
        )
        .await
    }

    /// Change an account's role, switch it off, or switch it back on. Every
    /// argument that is `None` leaves that half alone.
    pub async fn update_account(
        &self,
        name: &str,
        role: Option<&str>,
        disabled: Option<bool>,
    ) -> Result<Account> {
        let mut body = serde_json::Map::new();
        if let Some(role) = role {
            body.insert("role".to_string(), serde_json::json!(role));
        }
        if let Some(disabled) = disabled {
            body.insert("disabled".to_string(), serde_json::json!(disabled));
        }
        self.send(
            self.request(Method::PATCH, &format!("/api/accounts/{name}"))
                .json(&serde_json::Value::Object(body)),
            &format!("updating account {name}"),
        )
        .await
    }

    /// Mint a token. The secret in the answer is the only copy that will ever
    /// exist; the control plane keeps a digest and cannot show it again.
    ///
    /// `scopes` empty means "the account's role over every application",
    /// which is what the control plane fills in; `unscoped` asks for the old
    /// flat behaviour and is refused together with a scope list.
    pub async fn create_token(
        &self,
        account: &str,
        name: &str,
        scopes: &[String],
        unscoped: bool,
    ) -> Result<MintedToken> {
        let mut body = serde_json::Map::new();
        body.insert("name".to_string(), serde_json::json!(name));
        if unscoped {
            body.insert("unscoped".to_string(), serde_json::json!(true));
        } else if !scopes.is_empty() {
            body.insert("scopes".to_string(), serde_json::json!(scopes));
        }
        self.send(
            self.request(Method::POST, &format!("/api/accounts/{account}/tokens"))
                .json(&serde_json::Value::Object(body)),
            &format!("minting a token for account {account}"),
        )
        .await
    }

    /// Every credential the control plane knows about, and how many of them
    /// answer to no scope at all.
    pub async fn list_tokens(
        &self,
        account: Option<&str>,
        include_revoked: bool,
    ) -> Result<TokensResponse> {
        let mut path = format!("/api/tokens?include_revoked={include_revoked}");
        if let Some(account) = account {
            path.push_str(&format!("&account={account}"));
        }
        self.send(self.request(Method::GET, &path), "listing the tokens")
            .await
    }

    pub async fn revoke_token(&self, id: &str) -> Result<ApiToken> {
        self.send(
            self.request(Method::DELETE, &format!("/api/tokens/{id}")),
            &format!("revoking token {id}"),
        )
        .await
    }

    pub async fn read_audit(&self, filter: &AuditFilter) -> Result<Vec<AuditEntry>> {
        let mut query: Vec<String> = Vec::new();
        let mut push = |key: &str, value: Option<String>| {
            if let Some(value) = value {
                query.push(format!("{key}={value}"));
            }
        };
        push("app", filter.app.clone());
        push("environment", filter.environment.clone());
        push("account", filter.account.clone());
        push("action", filter.action.clone());
        push("outcome", filter.outcome.clone());
        push("since", filter.since.map(|s| s.to_string()));
        push("limit", filter.limit.map(|l| l.to_string()));

        let path = if query.is_empty() {
            "/api/audit".to_string()
        } else {
            format!("/api/audit?{}", query.join("&"))
        };
        let response: AuditResponse = self
            .send(self.request(Method::GET, &path), "reading the audit log")
            .await?;
        Ok(response.entries)
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
