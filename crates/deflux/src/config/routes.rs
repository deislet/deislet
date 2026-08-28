//! Route table derived from application configurations.
//!
//! Built once per configuration change and swapped in atomically, so the
//! data plane never parses JSON or reads certificate files on the request
//! path.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value as JsonValue;
use tracing::{debug, warn};

use crate::config::Validatable;
use crate::config::apps::AppConfig;
use crate::config::apps::certificate::CertificateConfig;
use crate::tls::LoadedCertificate;

/// Immutable snapshot of host and certificate lookups.
#[derive(Default)]
pub struct RouteTable {
    by_id: HashMap<String, Arc<AppConfig>>,
    by_domain: HashMap<String, Arc<AppConfig>>,
    /// Wildcard domains (`*.example.com`) keyed by suffix (`.example.com`).
    wildcard_domains: Vec<(String, Arc<AppConfig>)>,
    certs_by_name: HashMap<String, Arc<LoadedCertificate>>,
    wildcard_certs: Vec<(String, Arc<LoadedCertificate>)>,
    default_cert: Option<Arc<LoadedCertificate>>,
}

impl RouteTable {
    /// Build a table from raw app configurations. Entries that fail to parse,
    /// validate, or load their certificate are logged and skipped rather than
    /// taking the whole table down.
    pub fn build(
        apps: &HashMap<String, JsonValue>,
        default_certificate: Option<&CertificateConfig>,
    ) -> Self {
        let mut table = Self::default();

        if let Some(cfg) = default_certificate {
            match LoadedCertificate::load(cfg) {
                Ok(cert) => table.default_cert = Some(Arc::new(cert)),
                Err(e) => warn!(
                    cert = %cfg.cert_file,
                    error = %e,
                    "default certificate not loaded; unmatched SNI names will be rejected"
                ),
            }
        }

        for (key, raw) in apps {
            let app: AppConfig = match serde_json::from_value(raw.clone()) {
                Ok(app) => app,
                Err(e) => {
                    warn!(app = %key, error = %e, "app configuration is not valid JSON for AppConfig; skipped");
                    continue;
                }
            };
            if let Err(e) = app.validate() {
                warn!(app = %key, error = %e, "app configuration failed validation; skipped");
                continue;
            }
            if app.id != *key {
                warn!(app = %key, id = %app.id, "app id does not match its key; using the id");
            }
            let app = Arc::new(app);
            table.by_id.insert(app.id.clone(), Arc::clone(&app));

            for domain in &app.domains {
                let domain = normalize_host(domain);
                if let Some(suffix) = domain.strip_prefix("*.") {
                    table
                        .wildcard_domains
                        .push((format!(".{suffix}"), Arc::clone(&app)));
                } else if let Some(previous) =
                    table.by_domain.insert(domain.clone(), Arc::clone(&app))
                {
                    warn!(
                        domain = %domain,
                        previous = %previous.id,
                        current = %app.id,
                        "domain claimed by more than one app; the later one wins"
                    );
                }
            }

            let mut loaded = Vec::new();
            for cert_cfg in &app.certificates {
                match LoadedCertificate::load(cert_cfg) {
                    Ok(cert) => {
                        let cert = Arc::new(cert);
                        for name in &cert.names {
                            table.insert_certificate(name, &cert);
                        }
                        loaded.push(cert);
                    }
                    Err(e) => warn!(
                        app = %app.id,
                        cert = %cert_cfg.cert_file,
                        error = %e,
                        "certificate not loaded; TLS for this app will fall back to the default certificate"
                    ),
                }
            }
            // A certificate whose subject names do not list the app's domains
            // (or list none at all) still has to serve those domains.
            if let Some(first) = loaded.first() {
                for domain in &app.domains {
                    let domain = normalize_host(domain);
                    if !table.has_certificate_named(&domain) {
                        table.insert_certificate(&domain, first);
                    }
                }
            }
        }

        // Longest suffix first so `.a.example.com` beats `.example.com`.
        table
            .wildcard_domains
            .sort_by_key(|(suffix, _)| std::cmp::Reverse(suffix.len()));
        table
            .wildcard_certs
            .sort_by_key(|(suffix, _)| std::cmp::Reverse(suffix.len()));

        debug!(
            apps = table.by_id.len(),
            domains = table.by_domain.len() + table.wildcard_domains.len(),
            certificates = table.certs_by_name.len() + table.wildcard_certs.len(),
            default_certificate = table.default_cert.is_some(),
            "route table built"
        );
        table
    }

    fn insert_certificate(&mut self, name: &str, cert: &Arc<LoadedCertificate>) {
        let name = normalize_host(name);
        if let Some(suffix) = name.strip_prefix("*.") {
            self.wildcard_certs
                .push((format!(".{suffix}"), Arc::clone(cert)));
        } else {
            self.certs_by_name.insert(name, Arc::clone(cert));
        }
    }

    fn has_certificate_named(&self, name: &str) -> bool {
        self.certs_by_name.contains_key(name)
            || self
                .wildcard_certs
                .iter()
                .any(|(suffix, _)| name.ends_with(suffix.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn app_by_id(&self, id: &str) -> Option<&Arc<AppConfig>> {
        self.by_id.get(id)
    }

    /// Look up the app serving a `Host` value. The port, if any, is ignored.
    pub fn app_by_host(&self, host: &str) -> Option<&Arc<AppConfig>> {
        let host = normalize_host(host);
        if let Some(app) = self.by_domain.get(&host) {
            return Some(app);
        }
        self.wildcard_domains
            .iter()
            .find(|(suffix, _)| host.ends_with(suffix.as_str()) && host.len() > suffix.len())
            .map(|(_, app)| app)
    }

    /// Certificate to present for an SNI name; falls back to the default
    /// certificate when nothing matches or no name was sent.
    pub fn certificate_for(&self, server_name: Option<&str>) -> Option<&Arc<LoadedCertificate>> {
        if let Some(name) = server_name {
            let name = normalize_host(name);
            if let Some(cert) = self.certs_by_name.get(&name) {
                return Some(cert);
            }
            if let Some((_, cert)) = self
                .wildcard_certs
                .iter()
                .find(|(suffix, _)| name.ends_with(suffix.as_str()) && name.len() > suffix.len())
            {
                return Some(cert);
            }
        }
        self.default_cert.as_ref()
    }

    /// Whether any certificate can be served at all.
    pub fn has_certificates(&self) -> bool {
        self.default_cert.is_some()
            || !self.certs_by_name.is_empty()
            || !self.wildcard_certs.is_empty()
    }
}

/// Lowercase a host name and strip a trailing port (`example.com:8080`,
/// `[::1]:443`).
pub fn normalize_host(host: &str) -> String {
    let host = host.trim();
    let without_port = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else if host.matches(':').count() == 1 {
        host.split(':').next().unwrap_or(host)
    } else {
        host
    };
    without_port.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn app(id: &str, domains: &[&str], enabled: bool) -> JsonValue {
        json!({
            "id": id,
            "description": "",
            "domains": domains,
            "certificates": [],
            "enabled": enabled,
            "cache": [],
        })
    }

    #[test]
    fn normalizes_hosts() {
        assert_eq!(normalize_host("Example.COM:8080"), "example.com");
        assert_eq!(normalize_host("[::1]:443"), "::1");
        assert_eq!(normalize_host(" api.example.com "), "api.example.com");
    }

    #[test]
    fn routes_by_exact_and_wildcard_domain() {
        let mut apps = HashMap::new();
        apps.insert("shop".to_string(), app("shop", &["shop.example.com"], true));
        apps.insert("any".to_string(), app("any", &["*.example.com"], true));
        let table = RouteTable::build(&apps, None);

        assert_eq!(table.len(), 2);
        assert_eq!(
            table.app_by_host("shop.example.com:443").unwrap().id,
            "shop"
        );
        assert_eq!(table.app_by_host("blog.example.com").unwrap().id, "any");
        assert!(table.app_by_host("example.com").is_none());
        assert!(table.app_by_host("other.net").is_none());
        assert_eq!(table.app_by_id("shop").unwrap().id, "shop");
    }

    #[test]
    fn skips_invalid_entries() {
        let mut apps = HashMap::new();
        apps.insert("ok".to_string(), app("ok", &["ok.example.com"], true));
        apps.insert("bad".to_string(), json!({"id": "bad"}));
        apps.insert("nodomain".to_string(), app("nodomain", &[], true));
        let table = RouteTable::build(&apps, None);
        assert_eq!(table.len(), 1);
        assert!(!table.has_certificates());
        assert!(table.certificate_for(Some("ok.example.com")).is_none());
    }

    #[test]
    fn empty_table_reports_empty() {
        let table = RouteTable::build(&HashMap::new(), None);
        assert!(table.is_empty());
    }
}
