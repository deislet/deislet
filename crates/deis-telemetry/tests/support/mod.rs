//! What the integration tests stand in for deis-control with: one signing
//! key, the key ring a running service verifies against, and capabilities
//! minted the way the control plane mints them.

#![allow(dead_code)]

use std::time::Duration;

use deis_capability::{
    AUD_TELEMETRY, CapabilityClaims, CapabilityResource, KeyRing, TELEMETRY_INGEST,
    TELEMETRY_PLATFORM_TENANT, TELEMETRY_READ, encode,
};
use ed25519_dalek::SigningKey;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::{Request, Status};

/// Every application the tests push for. The operator capability below may
/// push each of them, in production, so tests about something else need not
/// think about capabilities at all.
pub const TEST_APPS: &[&str] = &[
    "after", "before", "blog", "chatty", "clock", "noisy", "one", "quiet", "shop", "stranger",
    "three", "two",
];

/// The control plane's signing key, as far as these tests are concerned.
pub fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[0x42; 32])
}

/// The key ring a service under test verifies against: exactly the key
/// above, as a JWKS refresher would have installed it.
pub fn keys() -> KeyRing {
    KeyRing::from_keys([signing_key().verifying_key()])
}

/// Where a service under test polls its revocation list from: a stand-in
/// control plane signing lists with the key above. Shared by every test in
/// the binary, so it runs on threads of its own.
pub fn control_endpoint() -> String {
    static CONTROL: std::sync::OnceLock<deis_capability::testing::FakeControl> =
        std::sync::OnceLock::new();
    CONTROL
        .get_or_init(|| deis_capability::testing::FakeControl::start(signing_key()))
        .url()
        .to_string()
}

/// The canonical tenant of `app_id` in `environment`, as deis-runtime stamps
/// it on every record: empty for a record of no application.
pub fn tenant(app_id: &str, environment: &str) -> String {
    if app_id.is_empty() {
        return String::new();
    }
    canonical(&format!("{app_id}.{environment}"))
}

/// What deis-control names in a capability for the environment a test names
/// by its `<app>.<environment>` label: the canonical tenant derived from it.
/// The reserved platform tenant is passed through.
fn canonical(label: &str) -> String {
    if label == TELEMETRY_PLATFORM_TENANT {
        return label.to_string();
    }
    deis_wire::CanonicalTenantId::derived_from_label(label).to_string()
}

/// A telemetry capability for `resources`, signed by the control plane.
/// Tenants are named by label and minted canonical, see [`canonical`].
pub fn token(resources: &[(&str, &str)]) -> String {
    token_as(
        "spiffe://deis.test/deislet/node/test",
        resources,
        Duration::from_secs(900),
    )
}

/// The same, for a chosen subject and lifetime.
pub fn token_as(sub: &str, resources: &[(&str, &str)], lifetime: Duration) -> String {
    let claims = CapabilityClaims::new(
        AUD_TELEMETRY,
        "spiffe://deis.test/deislet/control/main",
        sub,
        1,
        lifetime,
        resources
            .iter()
            .map(|(tenant, binding)| CapabilityResource {
                tenant: canonical(tenant),
                binding: binding.to_string(),
                generation: None,
            })
            .collect(),
    );
    encode(&claims, &signing_key())
}

/// A capability that expired `ago` before now, having lived its full 15
/// minutes.
pub fn expired_token(resources: &[(&str, &str)], ago: Duration) -> String {
    let mut claims = CapabilityClaims::new(
        AUD_TELEMETRY,
        "spiffe://deis.test/deislet/control/main",
        "spiffe://deis.test/human/someone",
        1,
        Duration::from_secs(900),
        resources
            .iter()
            .map(|(tenant, binding)| CapabilityResource {
                tenant: canonical(tenant),
                binding: binding.to_string(),
                generation: None,
            })
            .collect(),
    );
    claims.exp = claims.iat.saturating_sub(ago.as_secs());
    claims.iat = claims.exp.saturating_sub(900);
    encode(&claims, &signing_key())
}

/// Read everything and push for every test application: what the tests that
/// are not about authorisation act with.
pub fn operator_token() -> String {
    let tenants: Vec<String> = TEST_APPS
        .iter()
        .map(|app| format!("{app}.production"))
        .collect();
    let mut resources: Vec<(&str, &str)> = vec![
        (TELEMETRY_PLATFORM_TENANT, TELEMETRY_READ),
        (TELEMETRY_PLATFORM_TENANT, TELEMETRY_INGEST),
    ];
    resources.extend(tenants.iter().map(|t| (t.as_str(), TELEMETRY_INGEST)));
    token(&resources)
}

/// Stamps one capability onto every call of a client.
#[derive(Clone)]
pub struct Bearer(MetadataValue<Ascii>);

impl Bearer {
    pub fn new(token: &str) -> Self {
        Self(
            format!("Bearer {token}")
                .parse()
                .expect("a token is valid metadata"),
        )
    }
}

impl tonic::service::Interceptor for Bearer {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.0.clone());
        Ok(request)
    }
}
