//! Shared management contract for inventorying and fencing backend tenants.
//!
//! Application data credentials never enter this module. A backend registers
//! the gRPC service generated here only when it has an explicit migration
//! administrator credential, and [`MigrationAuth`] accepts that credential
//! only as an `Authorization: Bearer ...` value.
//!
//! [`TenantWriteFences`] is the commit boundary, not a status flag. Every
//! mutating backend operation keeps a [`WritePermit`] until its durable commit
//! has finished. Acquiring a fence takes the same tenant lock exclusively, so
//! it waits for all earlier writers, persists the fence, and only then returns
//! a receipt. Later writers see the persisted fence and fail closed. An
//! interactive SQL transaction must keep its permit for the transaction's
//! entire lifetime, including `COMMIT`.

use deis_wire::CanonicalId;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedRwLockReadGuard, RwLock};
use tonic::{Code, Request, Status};

/// Standard metadata header used by the migration management service.
pub const MIGRATION_AUTH_HEADER: &str = "authorization";

/// Generated messages and the shared `BackendMigration` service.
pub mod proto {
    tonic::include_proto!("deis.backend.migration.v1");
}

/// A durable proof that the backend stopped accepting writes for one tenant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FenceReceipt {
    pub tenant: String,
    pub migration_id: CanonicalId,
    pub expected_application_revision: u64,
    pub fence_epoch: u64,
    pub acquired_at_unix_seconds: i64,
    pub receipt_sha256: String,
    pub backend_incarnation_id: CanonicalId,
}

impl From<FenceReceipt> for proto::FenceReceipt {
    fn from(value: FenceReceipt) -> Self {
        Self {
            tenant: value.tenant,
            migration_id: value.migration_id.to_string(),
            expected_application_revision: value.expected_application_revision,
            fence_epoch: value.fence_epoch,
            acquired_at_unix_seconds: value.acquired_at_unix_seconds,
            receipt_sha256: value.receipt_sha256,
            backend_incarnation_id: value.backend_incarnation_id.to_string(),
        }
    }
}

/// One backend component's deterministic inventory evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentInventory {
    pub component: String,
    pub item_count: u64,
    pub logical_bytes: u64,
    pub sha256: String,
    pub orphan_count: u64,
    pub unchecked_orphan_count: u64,
}

impl From<ComponentInventory> for proto::ComponentInventory {
    fn from(value: ComponentInventory) -> Self {
        Self {
            component: value.component,
            item_count: value.item_count,
            logical_bytes: value.logical_bytes,
            sha256: value.sha256,
            orphan_count: value.orphan_count,
            unchecked_orphan_count: value.unchecked_orphan_count,
        }
    }
}

/// Length-delimited SHA-256 input shared by all component inventories.
///
/// Every field is prefixed by its byte length, so different tuples cannot
/// acquire the same byte stream through separator ambiguity. Callers add
/// records in their component's canonical sort order.
pub struct InventoryHasher {
    digest: Sha256,
}

impl InventoryHasher {
    #[must_use]
    pub fn new(component: &str) -> Self {
        let mut this = Self {
            digest: Sha256::new(),
        };
        this.field(b"deislet-backend-inventory-v1");
        this.field(component.as_bytes());
        this
    }

    pub fn field(&mut self, value: &[u8]) {
        self.digest.update((value.len() as u64).to_be_bytes());
        self.digest.update(value);
    }

    pub fn optional_field(&mut self, value: Option<&[u8]>) {
        match value {
            Some(value) => {
                self.digest.update([1]);
                self.field(value);
            }
            None => self.digest.update([0]),
        }
    }

    #[must_use]
    pub fn finish(
        self,
        component: impl Into<String>,
        item_count: u64,
        logical_bytes: u64,
        orphan_count: u64,
        unchecked_orphan_count: u64,
    ) -> ComponentInventory {
        ComponentInventory {
            component: component.into(),
            item_count,
            logical_bytes,
            sha256: format!("{:x}", self.digest.finalize()),
            orphan_count,
            unchecked_orphan_count,
        }
    }
}

/// A request to acquire or release one tenant's writer fence.
#[derive(Clone, Debug)]
pub struct FenceRequest {
    pub tenant: String,
    pub migration_id: CanonicalId,
    pub expected_application_revision: u64,
    pub expected_backend_incarnation_id: CanonicalId,
    token_sha256: String,
}

impl FenceRequest {
    pub fn parse(
        tenant: impl Into<String>,
        migration_id: &str,
        expected_application_revision: u64,
        fence_token: &str,
        expected_backend_incarnation_id: &str,
    ) -> Result<Self, FenceError> {
        let tenant = tenant.into();
        if tenant.is_empty() || tenant.len() > 256 {
            return Err(FenceError::Invalid(
                "tenant must contain 1 to 256 bytes".to_string(),
            ));
        }
        let migration_id = CanonicalId::from_str(migration_id)
            .map_err(|error| FenceError::Invalid(format!("invalid migration_id: {error}")))?;
        if expected_application_revision == 0 {
            return Err(FenceError::Invalid(
                "expected_application_revision must be positive".to_string(),
            ));
        }
        if fence_token.len() < 32 {
            return Err(FenceError::Invalid(
                "fence_token must contain at least 32 bytes".to_string(),
            ));
        }
        let expected_backend_incarnation_id =
            CanonicalId::from_str(expected_backend_incarnation_id).map_err(|error| {
                FenceError::Invalid(format!("invalid expected_backend_incarnation_id: {error}"))
            })?;
        Ok(Self {
            tenant,
            migration_id,
            expected_application_revision,
            expected_backend_incarnation_id,
            token_sha256: sha256_hex(fence_token.as_bytes()),
        })
    }
}

#[derive(Clone, Debug)]
struct FenceRecord {
    receipt: FenceReceipt,
    token_sha256: String,
    receipt_version: u32,
}

type TenantFence = Arc<RwLock<Option<FenceRecord>>>;

struct FenceStore {
    path: PathBuf,
    backend_incarnation_id: CanonicalId,
    tenants: Mutex<HashMap<String, TenantFence>>,
}

/// Durable, per-tenant writer fences shared by all components in a backend.
#[derive(Clone)]
pub struct TenantWriteFences {
    inner: Arc<FenceStore>,
}

impl TenantWriteFences {
    /// Open the fence journal and restore every active fence before serving.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, FenceError> {
        let path = path.into();
        let loaded_path = path.clone();
        let (backend_incarnation_id, records) =
            tokio::task::spawn_blocking(move || load_records(&loaded_path)).await??;
        Ok(Self::from_records(path, backend_incarnation_id, records))
    }

    /// Synchronous startup form for a service whose existing constructor is
    /// synchronous. Request-time transitions still run on blocking tasks.
    pub fn open_blocking(path: impl Into<PathBuf>) -> Result<Self, FenceError> {
        let path = path.into();
        let (backend_incarnation_id, records) = load_records(&path)?;
        Ok(Self::from_records(path, backend_incarnation_id, records))
    }

    fn from_records(
        path: PathBuf,
        backend_incarnation_id: CanonicalId,
        records: Vec<FenceRecord>,
    ) -> Self {
        let tenants = records
            .into_iter()
            .map(|record| {
                (
                    record.receipt.tenant.clone(),
                    Arc::new(RwLock::new(Some(record))),
                )
            })
            .collect();
        Self {
            inner: Arc::new(FenceStore {
                path,
                backend_incarnation_id,
                tenants: Mutex::new(tenants),
            }),
        }
    }

    /// Stable identity of this physical backend data incarnation.
    ///
    /// The journal is deliberately excluded from snapshots. A restored data
    /// directory therefore receives a new identity and cannot impersonate
    /// the backend against which a coordinator acquired its fence.
    #[must_use]
    pub fn backend_incarnation_id(&self) -> CanonicalId {
        self.inner.backend_incarnation_id
    }

    fn tenant_lock(&self, tenant: &str) -> TenantFence {
        let mut tenants = self
            .inner
            .tenants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tenants
            .entry(tenant.to_string())
            .or_insert_with(|| Arc::new(RwLock::new(None)))
            .clone()
    }

    /// Admit one mutation and keep it ordered before any later fence.
    pub async fn permit(&self, tenant: &str) -> Result<WritePermit, FenceError> {
        let guard = self.tenant_lock(tenant).read_owned().await;
        if let Some(record) = guard.as_ref() {
            return Err(FenceError::Fenced(record.receipt.clone()));
        }
        Ok(WritePermit { _guard: guard })
    }

    /// Keep an operation ordered with fence acquisition without rejecting a
    /// tenant that is already fenced. SQL query endpoints use this together
    /// with SQLite's `readonly` classification: reads remain available for
    /// migration copy, while a statement that can write is refused.
    pub async fn read_permit(&self, tenant: &str) -> WritePermit {
        WritePermit {
            _guard: self.tenant_lock(tenant).read_owned().await,
        }
    }

    /// Return the active receipt, if this tenant is fenced.
    pub async fn inspect(&self, tenant: &str) -> Option<FenceReceipt> {
        self.tenant_lock(tenant)
            .read()
            .await
            .as_ref()
            .map(|record| record.receipt.clone())
    }

    /// Require an active fence before inventory or copy begins.
    pub async fn require(&self, tenant: &str) -> Result<FenceReceipt, FenceError> {
        self.inspect(tenant).await.ok_or_else(|| {
            FenceError::NotFenced(format!(
                "tenant {tenant:?} has no active writer fence; inventory would race application writes"
            ))
        })
    }

    /// Pin an active fence for the full duration of inventory/copy work.
    /// Release waits for this guard, so application writes cannot resume
    /// halfway through a supposedly stable inventory.
    pub async fn fenced_read(
        &self,
        tenant: &str,
    ) -> Result<(FenceReceipt, WritePermit), FenceError> {
        let guard = self.tenant_lock(tenant).read_owned().await;
        let receipt = guard.as_ref().map(|record| record.receipt.clone()).ok_or_else(|| {
            FenceError::NotFenced(format!(
                "tenant {tenant:?} has no active writer fence; inventory would race application writes"
            ))
        })?;
        Ok((receipt, WritePermit { _guard: guard }))
    }

    /// Wait for earlier writers, persist a monotonically increasing epoch,
    /// then make all later writes fail closed.
    pub async fn acquire(&self, request: FenceRequest) -> Result<FenceReceipt, FenceError> {
        let this = self.clone();
        // This task owns the state transition. Cancelling the caller drops
        // only the JoinHandle; the transition still finishes, so a committed
        // SQLite row can never be absent from the process's in-memory gate.
        tokio::spawn(async move { this.acquire_owned(request).await }).await?
    }

    async fn acquire_owned(&self, request: FenceRequest) -> Result<FenceReceipt, FenceError> {
        self.require_incarnation(request.expected_backend_incarnation_id)?;
        let lock = self.tenant_lock(&request.tenant);
        let mut state = lock.write().await;
        if let Some(existing) = state.as_ref() {
            if same_request(existing, &request) {
                return Ok(existing.receipt.clone());
            }
            return Err(FenceError::Conflict(format!(
                "tenant {:?} is already fenced by migration {} at application revision {}",
                request.tenant,
                existing.receipt.migration_id,
                existing.receipt.expected_application_revision
            )));
        }

        let path = self.inner.path.clone();
        let persisted = request.clone();
        let record =
            tokio::task::spawn_blocking(move || persist_acquire(&path, &persisted)).await??;
        *state = Some(record.clone());
        Ok(record.receipt)
    }

    /// Release only the exact fence the coordinator acquired.
    ///
    /// This is the rollback path before cutover. A stale token, revision or
    /// migration ID cannot release somebody else's newer fence.
    pub async fn release(&self, request: FenceRequest) -> Result<u64, FenceError> {
        let this = self.clone();
        tokio::spawn(async move { this.release_owned(request).await }).await?
    }

    async fn release_owned(&self, request: FenceRequest) -> Result<u64, FenceError> {
        self.require_incarnation(request.expected_backend_incarnation_id)?;
        let lock = self.tenant_lock(&request.tenant);
        let mut state = lock.write().await;
        let Some(existing) = state.as_ref() else {
            return Err(FenceError::NotFenced(format!(
                "tenant {:?} has no active writer fence",
                request.tenant
            )));
        };
        if !same_request(existing, &request) {
            return Err(FenceError::Conflict(format!(
                "the release credential does not match tenant {:?}'s active fence",
                request.tenant
            )));
        }
        let epoch = existing.receipt.fence_epoch;
        let path = self.inner.path.clone();
        let released = request.clone();
        tokio::task::spawn_blocking(move || persist_release(&path, &released, epoch)).await??;
        *state = None;
        Ok(epoch)
    }

    fn require_incarnation(&self, expected: CanonicalId) -> Result<(), FenceError> {
        if expected != self.inner.backend_incarnation_id {
            return Err(FenceError::IncarnationMismatch {
                expected,
                actual: self.inner.backend_incarnation_id,
            });
        }
        Ok(())
    }
}

/// Held until one backend mutation has durably committed or rolled back.
pub struct WritePermit {
    _guard: OwnedRwLockReadGuard<Option<FenceRecord>>,
}

/// Refusals and durable-state failures from the fence boundary.
#[derive(Debug)]
pub enum FenceError {
    Invalid(String),
    NotFenced(String),
    Fenced(FenceReceipt),
    Conflict(String),
    IncarnationMismatch {
        expected: CanonicalId,
        actual: CanonicalId,
    },
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    BlockingTask(String),
}

impl fmt::Display for FenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "invalid fence request: {message}"),
            Self::NotFenced(message) => formatter.write_str(message),
            Self::Fenced(receipt) => write!(
                formatter,
                "tenant {:?} is write-fenced by migration {} at epoch {}",
                receipt.tenant, receipt.migration_id, receipt.fence_epoch
            ),
            Self::Conflict(message) => write!(formatter, "fence conflict: {message}"),
            Self::IncarnationMismatch { expected, actual } => write!(
                formatter,
                "backend incarnation changed: expected {expected}, found {actual}"
            ),
            Self::Sqlite(error) => write!(formatter, "fence journal sqlite: {error}"),
            Self::Io(error) => write!(formatter, "fence journal io: {error}"),
            Self::BlockingTask(message) => write!(formatter, "fence journal task: {message}"),
        }
    }
}

impl std::error::Error for FenceError {}

impl From<rusqlite::Error> for FenceError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<std::io::Error> for FenceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<tokio::task::JoinError> for FenceError {
    fn from(value: tokio::task::JoinError) -> Self {
        Self::BlockingTask(value.to_string())
    }
}

impl From<FenceError> for Status {
    fn from(value: FenceError) -> Self {
        let code = match value {
            FenceError::Invalid(_) => Code::InvalidArgument,
            FenceError::NotFenced(_)
            | FenceError::Fenced(_)
            | FenceError::IncarnationMismatch { .. } => Code::FailedPrecondition,
            FenceError::Conflict(_) => Code::Aborted,
            FenceError::Sqlite(_) | FenceError::Io(_) | FenceError::BlockingTask(_) => {
                Code::Internal
            }
        };
        Status::new(code, value.to_string())
    }
}

/// Bearer-token interceptor for the management surface.
#[derive(Clone)]
pub struct MigrationAuth {
    expected_sha256: Arc<[u8; 32]>,
}

impl MigrationAuth {
    pub fn new(secret: &str) -> Result<Self, FenceError> {
        if secret.len() < 32 {
            return Err(FenceError::Invalid(
                "migration_admin_token must contain at least 32 bytes".to_string(),
            ));
        }
        Ok(Self {
            expected_sha256: Arc::new(Sha256::digest(secret.as_bytes()).into()),
        })
    }

    #[allow(clippy::result_large_err)] // tonic's interceptor contract uses Status by value.
    pub fn check<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let presented = request
            .metadata()
            .get(MIGRATION_AUTH_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|value| !value.is_empty());
        let Some(presented) = presented else {
            return Err(Status::unauthenticated(
                "migration management requires Authorization: Bearer <token>",
            ));
        };
        let found: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        if !constant_time_eq(self.expected_sha256.as_ref(), &found) {
            return Err(Status::unauthenticated(
                "migration management credential was rejected",
            ));
        }
        Ok(())
    }
}

impl tonic::service::Interceptor for MigrationAuth {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        self.check(&request)?;
        Ok(request)
    }
}

fn same_request(record: &FenceRecord, request: &FenceRequest) -> bool {
    record.receipt.tenant == request.tenant
        && record.receipt.migration_id == request.migration_id
        && record.receipt.expected_application_revision == request.expected_application_revision
        && record.receipt.backend_incarnation_id == request.expected_backend_incarnation_id
        && constant_time_eq(
            record.token_sha256.as_bytes(),
            request.token_sha256.as_bytes(),
        )
}

fn load_records(path: &Path) -> Result<(CanonicalId, Vec<FenceRecord>), FenceError> {
    let connection = open_journal(path)?;
    let backend_incarnation_id = load_backend_incarnation(&connection)?;
    let mut statement = connection.prepare(
        "SELECT tenant, migration_id, expected_application_revision, fence_epoch,
                acquired_at, token_sha256, receipt_sha256, receipt_version
           FROM active_tenant_write_fences
          ORDER BY tenant",
    )?;
    let records = statement
        .query_map([], |row| {
            let migration_id: String = row.get(1)?;
            let migration_id = migration_id.parse::<CanonicalId>().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            Ok(FenceRecord {
                receipt: FenceReceipt {
                    tenant: row.get(0)?,
                    migration_id,
                    expected_application_revision: positive_sqlite_u64(row, 2)?,
                    fence_epoch: positive_sqlite_u64(row, 3)?,
                    acquired_at_unix_seconds: row.get(4)?,
                    receipt_sha256: row.get(6)?,
                    backend_incarnation_id,
                },
                token_sha256: row.get(5)?,
                receipt_version: positive_sqlite_u32(row, 7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for record in &records {
        validate_record(record)?;
    }
    Ok((backend_incarnation_id, records))
}

fn persist_acquire(path: &Path, request: &FenceRequest) -> Result<FenceRecord, FenceError> {
    let mut connection = open_journal(path)?;
    let backend_incarnation_id = load_backend_incarnation(&connection)?;
    require_expected_incarnation(request, backend_incarnation_id)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(existing) = active_record(&transaction, &request.tenant, backend_incarnation_id)? {
        if same_request(&existing, request) {
            transaction.commit()?;
            return Ok(existing);
        }
        return Err(FenceError::Conflict(format!(
            "tenant {:?} is already fenced by migration {}",
            request.tenant, existing.receipt.migration_id
        )));
    }
    let acquired_at = unix_seconds();
    transaction.execute(
        "INSERT INTO tenant_write_fence_events
             (tenant, action, migration_id, expected_application_revision, token_sha256,
              occurred_at, backend_incarnation_id)
         VALUES (?1, 'acquire', ?2, ?3, ?4, ?5, ?6)",
        params![
            request.tenant,
            request.migration_id.to_string(),
            as_i64(
                request.expected_application_revision,
                "application revision"
            )?,
            request.token_sha256,
            acquired_at,
            backend_incarnation_id.to_string()
        ],
    )?;
    let epoch = u64::try_from(transaction.last_insert_rowid()).map_err(|_| {
        FenceError::Invalid("fence journal returned a non-positive epoch".to_string())
    })?;
    let receipt_sha256 = receipt_digest_v2(request, epoch, acquired_at);
    transaction.execute(
        "INSERT INTO active_tenant_write_fences
             (tenant, migration_id, expected_application_revision, fence_epoch,
              acquired_at, token_sha256, receipt_sha256, receipt_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 2)",
        params![
            request.tenant,
            request.migration_id.to_string(),
            as_i64(
                request.expected_application_revision,
                "application revision"
            )?,
            as_i64(epoch, "fence epoch")?,
            acquired_at,
            request.token_sha256,
            receipt_sha256
        ],
    )?;
    transaction.commit()?;
    Ok(FenceRecord {
        receipt: FenceReceipt {
            tenant: request.tenant.clone(),
            migration_id: request.migration_id,
            expected_application_revision: request.expected_application_revision,
            fence_epoch: epoch,
            acquired_at_unix_seconds: acquired_at,
            receipt_sha256,
            backend_incarnation_id,
        },
        token_sha256: request.token_sha256.clone(),
        receipt_version: 2,
    })
}

fn persist_release(
    path: &Path,
    request: &FenceRequest,
    expected_epoch: u64,
) -> Result<(), FenceError> {
    let mut connection = open_journal(path)?;
    let backend_incarnation_id = load_backend_incarnation(&connection)?;
    require_expected_incarnation(request, backend_incarnation_id)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing = active_record(&transaction, &request.tenant, backend_incarnation_id)?
        .ok_or_else(|| {
            FenceError::NotFenced(format!(
                "tenant {:?} has no active writer fence",
                request.tenant
            ))
        })?;
    if existing.receipt.fence_epoch != expected_epoch || !same_request(&existing, request) {
        return Err(FenceError::Conflict(format!(
            "tenant {:?}'s durable fence changed before release",
            request.tenant
        )));
    }
    transaction.execute(
        "INSERT INTO tenant_write_fence_events
             (tenant, action, migration_id, expected_application_revision, token_sha256,
              occurred_at, backend_incarnation_id)
         VALUES (?1, 'release', ?2, ?3, ?4, ?5, ?6)",
        params![
            request.tenant,
            request.migration_id.to_string(),
            as_i64(
                request.expected_application_revision,
                "application revision"
            )?,
            request.token_sha256,
            unix_seconds(),
            backend_incarnation_id.to_string()
        ],
    )?;
    let removed = transaction.execute(
        "DELETE FROM active_tenant_write_fences WHERE tenant = ?1 AND fence_epoch = ?2",
        params![request.tenant, as_i64(expected_epoch, "fence epoch")?],
    )?;
    if removed != 1 {
        return Err(FenceError::Conflict(format!(
            "tenant {:?}'s durable fence disappeared before release",
            request.tenant
        )));
    }
    transaction.commit()?;
    Ok(())
}

fn active_record(
    connection: &Connection,
    tenant: &str,
    backend_incarnation_id: CanonicalId,
) -> Result<Option<FenceRecord>, FenceError> {
    let record = connection
        .query_row(
            "SELECT migration_id, expected_application_revision, fence_epoch,
                    acquired_at, token_sha256, receipt_sha256, receipt_version
               FROM active_tenant_write_fences WHERE tenant = ?1",
            [tenant],
            |row| {
                let migration_id: String = row.get(0)?;
                let migration_id = migration_id.parse::<CanonicalId>().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok(FenceRecord {
                    receipt: FenceReceipt {
                        tenant: tenant.to_string(),
                        migration_id,
                        expected_application_revision: positive_sqlite_u64(row, 1)?,
                        fence_epoch: positive_sqlite_u64(row, 2)?,
                        acquired_at_unix_seconds: row.get(3)?,
                        receipt_sha256: row.get(5)?,
                        backend_incarnation_id,
                    },
                    token_sha256: row.get(4)?,
                    receipt_version: positive_sqlite_u32(row, 6)?,
                })
            },
        )
        .optional()?;
    if let Some(record) = &record {
        validate_record(record)?;
    }
    Ok(record)
}

fn open_journal(path: &Path) -> Result<Connection, FenceError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(path)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS backend_metadata (
             singleton              INTEGER PRIMARY KEY CHECK(singleton = 1),
             backend_incarnation_id TEXT NOT NULL UNIQUE,
             created_at             INTEGER NOT NULL
         );
         CREATE TRIGGER IF NOT EXISTS trg_backend_incarnation_immutable_v1
         BEFORE UPDATE OF backend_incarnation_id ON backend_metadata
         WHEN NEW.backend_incarnation_id != OLD.backend_incarnation_id
         BEGIN
             SELECT RAISE(ABORT, 'backend incarnation is immutable');
         END;
         CREATE TRIGGER IF NOT EXISTS trg_backend_incarnation_delete_v1
         BEFORE DELETE ON backend_metadata
         BEGIN
             SELECT RAISE(ABORT, 'backend incarnation is immutable');
         END;
         CREATE TABLE IF NOT EXISTS tenant_write_fence_events (
             epoch                         INTEGER PRIMARY KEY AUTOINCREMENT,
             tenant                        TEXT NOT NULL,
             action                        TEXT NOT NULL CHECK(action IN ('acquire', 'release')),
             migration_id                  TEXT NOT NULL,
             expected_application_revision INTEGER NOT NULL CHECK(expected_application_revision > 0),
             token_sha256                   TEXT NOT NULL CHECK(length(token_sha256) = 64),
             occurred_at                   INTEGER NOT NULL,
             backend_incarnation_id         TEXT
         );
         CREATE TABLE IF NOT EXISTS active_tenant_write_fences (
             tenant                        TEXT PRIMARY KEY,
             migration_id                  TEXT NOT NULL,
             expected_application_revision INTEGER NOT NULL CHECK(expected_application_revision > 0),
             fence_epoch                   INTEGER NOT NULL UNIQUE,
             acquired_at                   INTEGER NOT NULL,
             token_sha256                   TEXT NOT NULL CHECK(length(token_sha256) = 64),
             receipt_sha256                 TEXT NOT NULL CHECK(length(receipt_sha256) = 64),
             receipt_version                INTEGER NOT NULL DEFAULT 1
                                                    CHECK(receipt_version IN (1, 2)),
             FOREIGN KEY(fence_epoch) REFERENCES tenant_write_fence_events(epoch)
         );",
    )?;
    ensure_journal_column(
        &connection,
        "tenant_write_fence_events",
        "backend_incarnation_id",
        "ALTER TABLE tenant_write_fence_events ADD COLUMN backend_incarnation_id TEXT",
    )?;
    ensure_journal_column(
        &connection,
        "active_tenant_write_fences",
        "receipt_version",
        "ALTER TABLE active_tenant_write_fences ADD COLUMN receipt_version INTEGER NOT NULL DEFAULT 1 CHECK(receipt_version IN (1, 2))",
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO backend_metadata
             (singleton, backend_incarnation_id, created_at)
         VALUES (1, ?1, ?2)",
        params![CanonicalId::new_v7().to_string(), unix_seconds()],
    )?;
    let _ = load_backend_incarnation(&connection)?;
    Ok(connection)
}

fn ensure_journal_column(
    connection: &Connection,
    table: &str,
    column: &str,
    migration: &str,
) -> Result<(), FenceError> {
    let columns = connection
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|name| name == column) {
        connection.execute(migration, [])?;
    }
    Ok(())
}

fn load_backend_incarnation(connection: &Connection) -> Result<CanonicalId, FenceError> {
    let value: String = connection.query_row(
        "SELECT backend_incarnation_id FROM backend_metadata WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    value.parse().map_err(|error| {
        FenceError::Invalid(format!(
            "fence journal contains an invalid backend incarnation ID: {error}"
        ))
    })
}

fn require_expected_incarnation(
    request: &FenceRequest,
    actual: CanonicalId,
) -> Result<(), FenceError> {
    if request.expected_backend_incarnation_id != actual {
        return Err(FenceError::IncarnationMismatch {
            expected: request.expected_backend_incarnation_id,
            actual,
        });
    }
    Ok(())
}

fn receipt_digest_v2(request: &FenceRequest, epoch: u64, acquired_at: i64) -> String {
    receipt_digest_parts_v2(
        &request.tenant,
        request.migration_id,
        request.expected_application_revision,
        epoch,
        acquired_at,
        &request.token_sha256,
        request.expected_backend_incarnation_id,
    )
}

fn receipt_digest_parts_v1(
    tenant: &str,
    migration_id: CanonicalId,
    expected_application_revision: u64,
    epoch: u64,
    acquired_at: i64,
    token_sha256: &str,
) -> String {
    let mut hash = InventoryHasher::new("writer-fence-receipt");
    hash.field(tenant.as_bytes());
    hash.field(migration_id.to_string().as_bytes());
    hash.field(&expected_application_revision.to_be_bytes());
    hash.field(&epoch.to_be_bytes());
    hash.field(&acquired_at.to_be_bytes());
    hash.field(token_sha256.as_bytes());
    hash.finish("receipt", 1, 0, 0, 0).sha256
}

fn receipt_digest_parts_v2(
    tenant: &str,
    migration_id: CanonicalId,
    expected_application_revision: u64,
    epoch: u64,
    acquired_at: i64,
    token_sha256: &str,
    backend_incarnation_id: CanonicalId,
) -> String {
    let mut hash = InventoryHasher::new("writer-fence-receipt-v2");
    hash.field(tenant.as_bytes());
    hash.field(migration_id.to_string().as_bytes());
    hash.field(&expected_application_revision.to_be_bytes());
    hash.field(&epoch.to_be_bytes());
    hash.field(&acquired_at.to_be_bytes());
    hash.field(token_sha256.as_bytes());
    hash.field(backend_incarnation_id.to_string().as_bytes());
    hash.finish("receipt", 1, 0, 0, 0).sha256
}

fn validate_record(record: &FenceRecord) -> Result<(), FenceError> {
    let receipt = &record.receipt;
    if receipt.tenant.is_empty() || receipt.tenant.len() > 256 {
        return Err(FenceError::Invalid(
            "fence journal contains an invalid tenant".to_string(),
        ));
    }
    if receipt.expected_application_revision == 0 || receipt.fence_epoch == 0 {
        return Err(FenceError::Invalid(
            "fence journal contains a non-positive revision or epoch".to_string(),
        ));
    }
    if !is_canonical_sha256(&record.token_sha256) || !is_canonical_sha256(&receipt.receipt_sha256) {
        return Err(FenceError::Invalid(
            "fence journal contains a non-canonical digest".to_string(),
        ));
    }
    let expected = match record.receipt_version {
        1 => receipt_digest_parts_v1(
            &receipt.tenant,
            receipt.migration_id,
            receipt.expected_application_revision,
            receipt.fence_epoch,
            receipt.acquired_at_unix_seconds,
            &record.token_sha256,
        ),
        2 => receipt_digest_parts_v2(
            &receipt.tenant,
            receipt.migration_id,
            receipt.expected_application_revision,
            receipt.fence_epoch,
            receipt.acquired_at_unix_seconds,
            &record.token_sha256,
            receipt.backend_incarnation_id,
        ),
        version => {
            return Err(FenceError::Invalid(format!(
                "fence journal contains unsupported receipt version {version}"
            )));
        }
    };
    if !constant_time_eq(expected.as_bytes(), receipt.receipt_sha256.as_bytes()) {
        return Err(FenceError::Invalid(
            "fence journal receipt digest does not match its durable fields".to_string(),
        ));
    }
    Ok(())
}

fn positive_sqlite_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or(rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn positive_sqlite_u32(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u32> {
    let value: i64 = row.get(index)?;
    u32::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or(rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

fn unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

fn as_i64(value: u64, label: &str) -> Result<i64, FenceError> {
    i64::try_from(value)
        .map_err(|_| FenceError::Invalid(format!("{label} exceeds SQLite's integer range")))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut different = left.len() ^ right.len();
    let width = left.len().max(right.len());
    for index in 0..width {
        different |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    different == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const MIGRATION: &str = "01990f44-1000-7000-8000-000000000050";
    const TOKEN: &str = "test-fence-token-00000000000000000001";

    fn request(fences: &TenantWriteFences, tenant: &str) -> FenceRequest {
        FenceRequest::parse(
            tenant,
            MIGRATION,
            7,
            TOKEN,
            &fences.backend_incarnation_id().to_string(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn acquisition_waits_for_an_in_flight_writer_and_then_refuses_new_ones() {
        let directory = TempDir::new().unwrap();
        let fences = TenantWriteFences::open(directory.path().join("fences.sqlite"))
            .await
            .unwrap();
        let permit = fences.permit("shop.production").await.unwrap();
        let acquiring = {
            let fences = fences.clone();
            tokio::spawn(async move {
                let request = request(&fences, "shop.production");
                fences.acquire(request).await
            })
        };
        tokio::task::yield_now().await;
        assert!(!acquiring.is_finished());
        drop(permit);
        let receipt = acquiring.await.unwrap().unwrap();
        assert_eq!(receipt.expected_application_revision, 7);
        assert!(matches!(
            fences.permit("shop.production").await,
            Err(FenceError::Fenced(_))
        ));
        assert!(fences.permit("another.production").await.is_ok());
    }

    #[tokio::test]
    async fn active_fences_survive_reopen_and_exact_retries_are_idempotent() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("fences.sqlite");
        let first = TenantWriteFences::open(&path).await.unwrap();
        let receipt = first
            .acquire(request(&first, "shop.production"))
            .await
            .unwrap();
        let retried = first
            .acquire(request(&first, "shop.production"))
            .await
            .unwrap();
        assert_eq!(retried, receipt);
        drop(first);

        let reopened = TenantWriteFences::open(&path).await.unwrap();
        assert_eq!(reopened.require("shop.production").await.unwrap(), receipt);
        let epoch = reopened
            .release(request(&reopened, "shop.production"))
            .await
            .unwrap();
        assert_eq!(epoch, receipt.fence_epoch);
        assert!(reopened.permit("shop.production").await.is_ok());
    }

    #[tokio::test]
    async fn stale_credentials_cannot_replace_or_release_a_fence() {
        let directory = TempDir::new().unwrap();
        let fences = TenantWriteFences::open(directory.path().join("fences.sqlite"))
            .await
            .unwrap();
        fences
            .acquire(request(&fences, "shop.production"))
            .await
            .unwrap();
        let stale = FenceRequest::parse(
            "shop.production",
            "01990f44-1000-7000-8000-000000000051",
            8,
            "other-fence-token-00000000000000000002",
            &fences.backend_incarnation_id().to_string(),
        )
        .unwrap();
        assert!(matches!(
            fences.acquire(stale.clone()).await,
            Err(FenceError::Conflict(_))
        ));
        assert!(matches!(
            fences.release(stale).await,
            Err(FenceError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn startup_refuses_a_fence_journal_with_a_tampered_receipt() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("fences.sqlite");
        let fences = TenantWriteFences::open(&path).await.unwrap();
        fences
            .acquire(request(&fences, "shop.production"))
            .await
            .unwrap();
        drop(fences);

        let connection = open_journal(&path).unwrap();
        connection
            .execute(
                "UPDATE active_tenant_write_fences SET receipt_sha256 = ?1",
                ["0000000000000000000000000000000000000000000000000000000000000000"],
            )
            .unwrap();
        drop(connection);

        assert!(matches!(
            TenantWriteFences::open(&path).await,
            Err(FenceError::Invalid(message)) if message.contains("receipt digest")
        ));
    }

    #[tokio::test]
    async fn incarnation_is_stable_per_journal_and_different_for_a_new_journal() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("fences.sqlite");
        let first = TenantWriteFences::open(&path).await.unwrap();
        let first_id = first.backend_incarnation_id();
        drop(first);
        assert_eq!(
            TenantWriteFences::open(&path)
                .await
                .unwrap()
                .backend_incarnation_id(),
            first_id
        );

        let replacement = TenantWriteFences::open(directory.path().join("replacement.sqlite"))
            .await
            .unwrap();
        assert_ne!(replacement.backend_incarnation_id(), first_id);

        let stale = FenceRequest::parse(
            "shop.production",
            MIGRATION,
            7,
            TOKEN,
            &replacement.backend_incarnation_id().to_string(),
        )
        .unwrap();
        assert!(matches!(
            TenantWriteFences::open(&path)
                .await
                .unwrap()
                .acquire(stale)
                .await,
            Err(FenceError::IncarnationMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn legacy_active_receipts_survive_the_incarnation_schema_upgrade() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("legacy.sqlite");
        let token_sha256 = sha256_hex(TOKEN.as_bytes());
        let migration_id = MIGRATION.parse::<CanonicalId>().unwrap();
        let acquired_at = 1_788_480_000;
        let receipt_sha256 = receipt_digest_parts_v1(
            "shop.production",
            migration_id,
            7,
            1,
            acquired_at,
            &token_sha256,
        );
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE tenant_write_fence_events (
                     epoch INTEGER PRIMARY KEY AUTOINCREMENT,
                     tenant TEXT NOT NULL,
                     action TEXT NOT NULL,
                     migration_id TEXT NOT NULL,
                     expected_application_revision INTEGER NOT NULL,
                     token_sha256 TEXT NOT NULL,
                     occurred_at INTEGER NOT NULL
                 );
                 CREATE TABLE active_tenant_write_fences (
                     tenant TEXT PRIMARY KEY,
                     migration_id TEXT NOT NULL,
                     expected_application_revision INTEGER NOT NULL,
                     fence_epoch INTEGER NOT NULL UNIQUE,
                     acquired_at INTEGER NOT NULL,
                     token_sha256 TEXT NOT NULL,
                     receipt_sha256 TEXT NOT NULL,
                     FOREIGN KEY(fence_epoch) REFERENCES tenant_write_fence_events(epoch)
                 );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO tenant_write_fence_events
                    (epoch, tenant, action, migration_id, expected_application_revision,
                     token_sha256, occurred_at)
                 VALUES (1, ?1, 'acquire', ?2, 7, ?3, ?4)",
                params!["shop.production", MIGRATION, token_sha256, acquired_at],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO active_tenant_write_fences
                    (tenant, migration_id, expected_application_revision, fence_epoch,
                     acquired_at, token_sha256, receipt_sha256)
                 VALUES (?1, ?2, 7, 1, ?3, ?4, ?5)",
                params![
                    "shop.production",
                    MIGRATION,
                    acquired_at,
                    token_sha256,
                    receipt_sha256
                ],
            )
            .unwrap();
        drop(connection);

        let upgraded = TenantWriteFences::open(&path).await.unwrap();
        let receipt = upgraded.require("shop.production").await.unwrap();
        assert_eq!(receipt.receipt_sha256, receipt_sha256);
        assert_eq!(
            receipt.backend_incarnation_id,
            upgraded.backend_incarnation_id()
        );
        upgraded
            .release(request(&upgraded, "shop.production"))
            .await
            .unwrap();
    }

    #[test]
    fn startup_refuses_an_invalid_durable_incarnation() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("invalid.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE backend_metadata (
                     singleton INTEGER PRIMARY KEY,
                     backend_incarnation_id TEXT NOT NULL UNIQUE,
                     created_at INTEGER NOT NULL
                 );
                 INSERT INTO backend_metadata VALUES (1, 'not-a-uuid', 1);",
            )
            .unwrap();
        drop(connection);
        assert!(matches!(
            TenantWriteFences::open_blocking(path),
            Err(FenceError::Invalid(message)) if message.contains("backend incarnation ID")
        ));
    }

    #[test]
    fn management_auth_is_bearer_only_and_never_accepts_a_short_secret() {
        assert!(MigrationAuth::new("too-short").is_err());
        let auth = MigrationAuth::new("management-token-00000000000000001").unwrap();
        for value in [
            None,
            Some("management-token-00000000000000001"),
            Some("Bearer wrong-token-000000000000000000000"),
        ] {
            let mut request = Request::new(());
            if let Some(value) = value {
                request
                    .metadata_mut()
                    .insert(MIGRATION_AUTH_HEADER, value.parse().unwrap());
            }
            assert_eq!(
                auth.check(&request).unwrap_err().code(),
                Code::Unauthenticated
            );
        }
        let mut request = Request::new(());
        request.metadata_mut().insert(
            MIGRATION_AUTH_HEADER,
            "Bearer management-token-00000000000000001".parse().unwrap(),
        );
        auth.check(&request).unwrap();
    }
}
