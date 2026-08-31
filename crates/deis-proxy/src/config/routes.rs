//! Route table derived from application configurations.
//!
//! Built once per configuration change and swapped in atomically, so the
//! data plane never parses JSON or reads certificate files on the request
//! path.

use std::collections::{BTreeSet, HashMap, HashSet};
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
    /// Apps for which at least one of their own certificates loaded.
    tls_apps: HashSet<String>,
    /// Exact names covered by a certificate an operator placed and named in
    /// the configuration, and the suffixes of the wildcards among them.
    ///
    /// Kept apart from `certs_by_name` because automatic certificates land
    /// in that map too, and the question these answer is not "is there a
    /// certificate" but "did somebody choose one" - which is what decides
    /// whether ACME should leave the name alone.
    configured_names: HashSet<String>,
    configured_wildcards: Vec<String>,
    /// Every exact host an application configured here claims, for the
    /// renewal task to certify.
    served_names: BTreeSet<String>,
}

impl RouteTable {
    /// Build a table from raw app configurations. Entries that fail to parse,
    /// validate, or load their certificate are logged and skipped rather than
    /// taking the whole table down.
    ///
    /// `automatic_certificates` are the pairs the ACME store holds, keyed by
    /// the name they were obtained for. They are loaded here rather than
    /// through a second lookup in the handshake callback so that one map
    /// decides which certificate serves a name, and so that a restart picks
    /// up what is already on disk with no extra step.
    pub fn build(
        apps: &HashMap<String, JsonValue>,
        default_certificate: Option<&CertificateConfig>,
        automatic_certificates: &[(String, CertificateConfig)],
    ) -> Self {
        let mut table = Self::default();

        if let Some(cfg) = default_certificate {
            match LoadedCertificate::load(cfg) {
                Ok(cert) => {
                    // The default certificate is a certificate somebody
                    // chose, so the names it attests to are theirs and the
                    // renewal task leaves them alone. A wildcard here is how
                    // one file covers a whole preview zone.
                    for name in &cert.names {
                        table.remember_configured(name);
                    }
                    table.default_cert = Some(Arc::new(cert));
                }
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
                if !domain.starts_with("*.") {
                    table.served_names.insert(domain.clone());
                }
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

            // Naming a certificate for an application is the decision, not
            // whether the file happened to load. An application whose
            // certificate is missing for a minute - being rotated, or a
            // typo in the path - must not have its domains quietly taken
            // over by an automatic certificate while the operator is fixing
            // it; the name stays theirs and answers plain HTTP until it
            // loads, which is what this proxy did before any of this existed.
            if !app.certificates.is_empty() {
                for domain in &app.domains {
                    table.remember_configured(&normalize_host(domain));
                }
            }

            let mut loaded = Vec::new();
            for cert_cfg in &app.certificates {
                match LoadedCertificate::load(cert_cfg) {
                    Ok(cert) => {
                        let cert = Arc::new(cert);
                        for name in &cert.names {
                            table.insert_certificate(name, &cert);
                            table.remember_configured(name);
                        }
                        loaded.push(cert);
                    }
                    Err(e) if table.default_cert.is_some() => warn!(
                        app = %app.id,
                        cert = %cert_cfg.cert_file,
                        error = %e,
                        "certificate not loaded; TLS for this app will fall back to the default certificate"
                    ),
                    Err(e) => warn!(
                        app = %app.id,
                        cert = %cert_cfg.cert_file,
                        error = %e,
                        "certificate not loaded and no default certificate: HTTPS for this app \
                         will fail and plain HTTP will be served without redirect"
                    ),
                }
            }
            // A certificate whose subject names do not list the app's domains
            // (or list none at all) still has to serve those domains.
            if let Some(first) = loaded.first() {
                table.tls_apps.insert(app.id.clone());
                for domain in &app.domains {
                    let domain = normalize_host(domain);
                    if !table.has_certificate_named(&domain) {
                        table.insert_certificate(&domain, first);
                    }
                }
            }
        }

        // Automatic certificates come last and never displace a configured
        // one: a file an operator placed is a decision, and this map is what
        // the handshake reads.
        for (name, config) in automatic_certificates {
            let name = normalize_host(name);
            if table.covered_by_configured_certificate(&name) {
                debug!(
                    name = %name,
                    "an automatic certificate exists for a name a configured certificate already \
                     covers; the configured one is served"
                );
                continue;
            }
            match LoadedCertificate::load(config) {
                Ok(cert) => {
                    let cert = Arc::new(cert);
                    // Registered under the name it was obtained for as well
                    // as under its own subject names: the two are the same
                    // today, and a certificate whose subject names drifted
                    // must still serve the name it is filed under.
                    table.insert_certificate(&name, &cert);
                    for subject in &cert.names {
                        if !table.has_certificate_named(subject) {
                            table.insert_certificate(subject, &cert);
                        }
                    }
                }
                Err(e) => warn!(
                    name = %name,
                    cert = %config.cert_file,
                    error = %e,
                    "an automatic certificate on disk could not be loaded; this name will not \
                     serve HTTPS and the renewal pass will order it again"
                ),
            }
        }

        // An application whose domains are covered by an automatic
        // certificate terminates TLS just as much as one that named a file,
        // and the HTTPS redirect is decided from this set. Without this pass
        // an ACME certificate would be served on the TLS port while plain
        // HTTP for the same application was never redirected to it.
        let ids: Vec<String> = table.by_id.keys().cloned().collect();
        for id in ids {
            if table.tls_apps.contains(&id) {
                continue;
            }
            let covered = table
                .by_id
                .get(&id)
                .map(|app| {
                    app.domains
                        .iter()
                        .any(|domain| table.has_certificate_named(&normalize_host(domain)))
                })
                .unwrap_or(false);
            if covered {
                table.tls_apps.insert(id);
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

    /// Note that a certificate an operator placed attests to this name.
    fn remember_configured(&mut self, name: &str) {
        let name = normalize_host(name);
        match name.strip_prefix("*.") {
            Some(suffix) => self.configured_wildcards.push(format!(".{suffix}")),
            None => {
                self.configured_names.insert(name);
            }
        }
    }

    fn covered_by_configured_certificate(&self, name: &str) -> bool {
        let name = normalize_host(name);
        self.configured_names.contains(&name)
            || self
                .configured_wildcards
                .iter()
                .any(|suffix| name.len() > suffix.len() && name.ends_with(suffix.as_str()))
    }

    /// Whether a certificate an operator chose already covers this name, so
    /// the renewal task should not obtain one of its own for it.
    pub fn has_configured_certificate_for(&self, name: &str) -> bool {
        self.covered_by_configured_certificate(name)
    }

    /// Every exact host the applications configured here claim.
    ///
    /// The renewal task's half of "a certificate follows from what is
    /// actually being served": these names come from the application list,
    /// not from a list beside it.
    pub fn served_names(&self) -> Vec<String> {
        self.served_names.iter().cloned().collect()
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

    /// Whether a TLS handshake for this app can succeed: one of its own
    /// certificates loaded, or it has certificates configured and the default
    /// certificate stands in for the ones that failed to load. Redirecting to
    /// HTTPS on any other basis would send clients to a port that rejects them.
    pub fn https_available(&self, app: &AppConfig) -> bool {
        self.tls_apps.contains(&app.id)
            || (!app.certificates.is_empty() && self.default_cert.is_some())
    }

    /// Whether a TLS handshake for this host name can succeed, without an
    /// application configuration to ask about.
    ///
    /// This is the question for an application only the control plane's
    /// catalog knows: it has no entry here and therefore no certificates of
    /// its own, so what decides is whether *some* certificate would be
    /// served for the name - a wildcard covering a whole preview zone, or
    /// `tls.default_certificate`. The test is exactly what the SNI callback
    /// will do at handshake time, so the redirect cannot point at a port
    /// that would then reject the client.
    pub fn https_available_for_host(&self, host: &str) -> bool {
        self.certificate_for(Some(host)).is_some()
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
        let table = RouteTable::build(&apps, None, &[]);

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
        let table = RouteTable::build(&apps, None, &[]);
        assert_eq!(table.len(), 1);
        assert!(!table.has_certificates());
        assert!(table.certificate_for(Some("ok.example.com")).is_none());
    }

    #[test]
    fn empty_table_reports_empty() {
        let table = RouteTable::build(&HashMap::new(), None, &[]);
        assert!(table.is_empty());
    }

    fn app_with_missing_certificate(id: &str, domain: &str) -> JsonValue {
        json!({
            "id": id,
            "description": "",
            "domains": [domain],
            "certificates": [
                { "cert_file": "/nonexistent/deis-proxy-test.pem", "key_file": "/nonexistent/deis-proxy-test.key" }
            ],
            "enabled": true,
            "cache": [],
        })
    }

    #[test]
    fn unloadable_certificate_keeps_app_routable_without_https() {
        let mut apps = HashMap::new();
        apps.insert(
            "shop".to_string(),
            app_with_missing_certificate("shop", "shop.example.com"),
        );
        let table = RouteTable::build(&apps, None, &[]);
        let shop = table.app_by_host("shop.example.com").unwrap();
        assert!(!table.https_available(shop));
        assert!(table.certificate_for(Some("shop.example.com")).is_none());
    }

    #[test]
    fn default_certificate_makes_https_available() {
        let cert = CertificateConfig {
            cert_file: "/nonexistent/default.pem".to_string(),
            key_file: "/nonexistent/default.key".to_string(),
        };
        let mut apps = HashMap::new();
        apps.insert(
            "shop".to_string(),
            app_with_missing_certificate("shop", "shop.example.com"),
        );
        apps.insert(
            "plain".to_string(),
            app("plain", &["plain.example.com"], true),
        );
        let mut table = RouteTable::build(&apps, Some(&cert), &[]);
        // The default certificate file does not exist either; stand one in.
        assert!(table.default_cert.is_none());
        table.default_cert = Some(Arc::new(test_certificate()));

        let shop = table.app_by_host("shop.example.com").unwrap();
        assert!(table.https_available(shop));
        let plain = table.app_by_host("plain.example.com").unwrap();
        assert!(
            !table.https_available(plain),
            "an app without certificates is not redirected just because a default exists"
        );
    }

    /// A throwaway self-signed certificate written to disk, so the route
    /// table loads it the way it loads a real one.
    fn write_certificate(dir: &std::path::Path, name: &str) -> CertificateConfig {
        use openssl::asn1::Asn1Time;
        use openssl::hash::MessageDigest;
        use openssl::pkey::PKey;
        use openssl::x509::extension::SubjectAlternativeName;
        use openssl::x509::{X509, X509NameBuilder};

        let key = PKey::generate_ed25519().unwrap();
        let mut subject = X509NameBuilder::new().unwrap();
        subject.append_entry_by_text("CN", name).unwrap();
        let subject = subject.build();

        let mut builder = X509::builder().unwrap();
        builder.set_version(2).unwrap();
        builder.set_subject_name(&subject).unwrap();
        builder.set_issuer_name(&subject).unwrap();
        builder.set_pubkey(&key).unwrap();
        builder
            .set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        builder
            .set_not_after(&Asn1Time::days_from_now(90).unwrap())
            .unwrap();
        let context = builder.x509v3_context(None, None);
        let san = SubjectAlternativeName::new()
            .dns(name)
            .build(&context)
            .unwrap();
        let _ = context;
        builder.append_extension(san).unwrap();
        builder.sign(&key, MessageDigest::null()).unwrap();
        let certificate = builder.build();

        let cert_file = dir.join(format!("{name}.pem"));
        let key_file = dir.join(format!("{name}.key"));
        std::fs::write(&cert_file, certificate.to_pem().unwrap()).unwrap();
        std::fs::write(&key_file, key.private_key_to_pem_pkcs8().unwrap()).unwrap();
        CertificateConfig {
            cert_file: cert_file.to_string_lossy().into_owned(),
            key_file: key_file.to_string_lossy().into_owned(),
        }
    }

    #[test]
    fn an_automatic_certificate_is_served_and_turns_the_https_redirect_on() {
        // The whole point of loading the ACME store here rather than through
        // a second lookup in the handshake callback: one map decides which
        // certificate serves a name, and `https_available` - which is what
        // decides whether plain HTTP is redirected - is computed from it.
        // Without the second half, a name would answer HTTPS perfectly well
        // while every plain request stayed on port 80.
        let dir = tempfile::tempdir().unwrap();
        let automatic = write_certificate(dir.path(), "shop.example.com");

        let mut apps = HashMap::new();
        apps.insert("shop".to_string(), app("shop", &["shop.example.com"], true));
        let table = RouteTable::build(&apps, None, &[("shop.example.com".to_string(), automatic)]);

        assert!(table.has_certificates());
        assert_eq!(
            table
                .certificate_for(Some("shop.example.com"))
                .unwrap()
                .names,
            vec!["shop.example.com".to_string()]
        );
        let shop = table.app_by_host("shop.example.com").unwrap();
        assert!(
            table.https_available(shop),
            "an application whose certificate was obtained rather than placed still terminates \
             TLS, so plain HTTP for it has to be redirected"
        );
    }

    #[test]
    fn a_certificate_an_operator_placed_wins_over_an_automatic_one() {
        // Their file is a decision, and this map is what the handshake
        // reads. The name is also excluded from what the renewal task asks
        // for, so the two never race for the same name.
        let dir = tempfile::tempdir().unwrap();
        let theirs = write_certificate(dir.path(), "chosen.example.com");
        let automatic = write_certificate(dir.path(), "chosen.example.com");

        let mut apps = HashMap::new();
        apps.insert(
            "shop".to_string(),
            json!({
                "id": "shop",
                "description": "",
                "domains": ["chosen.example.com"],
                "certificates": [theirs],
                "enabled": true,
                "cache": [],
            }),
        );
        let table = RouteTable::build(
            &apps,
            None,
            &[("chosen.example.com".to_string(), automatic.clone())],
        );

        assert!(table.has_configured_certificate_for("chosen.example.com"));
        let served = table.certificate_for(Some("chosen.example.com")).unwrap();
        let theirs_pem = std::fs::read(dir.path().join("chosen.example.com.pem")).unwrap();
        assert_eq!(
            served.leaf.to_pem().unwrap(),
            openssl::x509::X509::stack_from_pem(&theirs_pem).unwrap()[0]
                .to_pem()
                .unwrap(),
            "the certificate the operator named has to be the one presented"
        );
        // And the automatic file is untouched on disk: nothing deletes it,
        // so removing the configured certificate falls back to it.
        assert!(std::path::Path::new(&automatic.cert_file).is_file());
    }

    #[test]
    fn the_names_offered_to_the_renewal_task_are_the_hosts_that_are_served() {
        let mut apps = HashMap::new();
        apps.insert(
            "shop".to_string(),
            app("shop", &["Shop.Example.COM", "*.example.com"], true),
        );
        let table = RouteTable::build(&apps, None, &[]);
        assert_eq!(
            table.served_names(),
            vec!["shop.example.com".to_string()],
            "names are normalised, and a wildcard is not a name http-01 can certify"
        );
    }

    #[test]
    fn a_wildcard_default_certificate_covers_the_names_under_it() {
        // The intended way to serve a preview zone: one wildcard placed by
        // hand, and the renewal task leaves every name under it alone rather
        // than ordering one certificate per environment.
        let dir = tempfile::tempdir().unwrap();
        let wildcard = write_certificate(dir.path(), "*.preview.example.com");
        let table = RouteTable::build(&HashMap::new(), Some(&wildcard), &[]);

        assert!(table.has_configured_certificate_for("pr-1--shop.preview.example.com"));
        assert!(!table.has_configured_certificate_for("shop.example.com"));
        assert!(table.https_available_for_host("pr-1--shop.preview.example.com"));
    }

    /// A throwaway self-signed certificate, generated in memory.
    fn test_certificate() -> LoadedCertificate {
        use openssl::asn1::Asn1Time;
        use openssl::hash::MessageDigest;
        use openssl::pkey::PKey;
        use openssl::x509::{X509, X509NameBuilder};

        let key = PKey::generate_ed25519().unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "default.example.com")
            .unwrap();
        let name = name.build();
        let mut builder = X509::builder().unwrap();
        builder.set_version(2).unwrap();
        builder.set_subject_name(&name).unwrap();
        builder.set_issuer_name(&name).unwrap();
        builder.set_pubkey(&key).unwrap();
        builder
            .set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        builder
            .set_not_after(&Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        // Ed25519 signatures embed their own digest.
        builder.sign(&key, MessageDigest::null()).unwrap();
        LoadedCertificate {
            leaf: builder.build(),
            chain: Vec::new(),
            key,
            names: vec!["default.example.com".to_string()],
        }
    }
}
