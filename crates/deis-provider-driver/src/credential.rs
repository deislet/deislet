use std::{fmt, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deis_wire::CanonicalId;
use zeroize::Zeroizing;

use crate::DriverExecutionContext;

const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;

/// Provider-management credential bytes owned by the driver security
/// boundary. The allocation is zeroized on drop and never implements
/// serialization, cloning, equality, `Display`, or an exposing `Debug`.
pub struct CredentialMaterial(Zeroizing<Vec<u8>>);

impl CredentialMaterial {
    pub fn new(value: Vec<u8>) -> Result<Self, CredentialMaterialError> {
        let value = Zeroizing::new(value);
        if value.is_empty() || value.len() > MAX_CREDENTIAL_BYTES {
            return Err(CredentialMaterialError);
        }
        Ok(Self(value))
    }

    /// Reveal bytes only inside provider driver code which is about to use
    /// the credential through an allowed transport or an official SDK/API.
    #[must_use]
    pub fn expose_secret(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl fmt::Debug for CredentialMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialMaterial([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CredentialMaterialError;

impl fmt::Display for CredentialMaterialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("credential must be non-empty and no larger than 64 KiB")
    }
}

impl std::error::Error for CredentialMaterialError {}

/// A short-lived, operation-scoped authorization plus zeroizing material.
/// A lease is bound to the exact operation owner and claim epoch that asked
/// for it; those fence values are deliberately metadata, never credentials.
pub struct ProviderCredentialLease {
    lease_id: CanonicalId,
    binding_id: CanonicalId,
    binding_revision: u64,
    operation_id: CanonicalId,
    owner_id: CanonicalId,
    claim_epoch: u64,
    expires_at: DateTime<Utc>,
    material: CredentialMaterial,
}

impl ProviderCredentialLease {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        lease_id: CanonicalId,
        binding_id: CanonicalId,
        binding_revision: u64,
        operation_id: CanonicalId,
        owner_id: CanonicalId,
        claim_epoch: u64,
        expires_at: DateTime<Utc>,
        material: CredentialMaterial,
    ) -> Self {
        Self {
            lease_id,
            binding_id,
            binding_revision,
            operation_id,
            owner_id,
            claim_epoch,
            expires_at,
            material,
        }
    }

    #[must_use]
    pub const fn lease_id(&self) -> CanonicalId {
        self.lease_id
    }

    #[must_use]
    pub const fn binding_id(&self) -> CanonicalId {
        self.binding_id
    }

    #[must_use]
    pub const fn binding_revision(&self) -> u64 {
        self.binding_revision
    }

    #[must_use]
    pub const fn operation_id(&self) -> CanonicalId {
        self.operation_id
    }

    #[must_use]
    pub const fn owner_id(&self) -> CanonicalId {
        self.owner_id
    }

    #[must_use]
    pub const fn claim_epoch(&self) -> u64 {
        self.claim_epoch
    }

    #[must_use]
    pub const fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    #[must_use]
    pub const fn material(&self) -> &CredentialMaterial {
        &self.material
    }
}

impl fmt::Debug for ProviderCredentialLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCredentialLease")
            .field("lease_id", &self.lease_id)
            .field("binding_id", &self.binding_id)
            .field("binding_revision", &self.binding_revision)
            .field("operation_id", &self.operation_id)
            .field("owner_id", &self.owner_id)
            .field("claim_epoch", &self.claim_epoch)
            .field("expires_at", &self.expires_at)
            .field("material", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialLeaseDisposition {
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialBrokerErrorKind {
    Unavailable,
    Unauthorized,
    Revoked,
    Integrity,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialBrokerError {
    kind: CredentialBrokerErrorKind,
    message: String,
}

impl CredentialBrokerError {
    #[must_use]
    pub fn new(kind: CredentialBrokerErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> CredentialBrokerErrorKind {
        self.kind
    }
}

impl fmt::Display for CredentialBrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CredentialBrokerError {}

/// Credential access is mediated independently of the serializable driver
/// protocol. Implementations must fence acquisition against the supplied
/// execution context and cap the lease to the durable operation lease.
#[async_trait]
pub trait ProviderCredentialBroker: Send + Sync {
    async fn acquire(
        &self,
        context: &DriverExecutionContext<'_>,
        requested_duration: Duration,
    ) -> Result<ProviderCredentialLease, CredentialBrokerError>;

    async fn release(
        &self,
        lease: ProviderCredentialLease,
        disposition: CredentialLeaseDisposition,
    ) -> Result<(), CredentialBrokerError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_debug_output_is_redacted() {
        let secret = b"credential-that-must-not-appear";
        let material = CredentialMaterial::new(secret.to_vec()).unwrap();
        let debug = format!("{material:?}");
        assert!(!debug.contains(std::str::from_utf8(secret).unwrap()));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn credential_material_is_bounded() {
        assert!(CredentialMaterial::new(Vec::new()).is_err());
        assert!(CredentialMaterial::new(vec![0; MAX_CREDENTIAL_BYTES]).is_ok());
        assert!(CredentialMaterial::new(vec![0; MAX_CREDENTIAL_BYTES + 1]).is_err());
    }
}
