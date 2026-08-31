//! Which names this instance obtains certificates for.
//!
//! Derived, never listed. The names come from the two tables that already
//! decide what this proxy answers to - the applications configured on this
//! machine, and the catalog the control plane publishes - so a domain added
//! through the control plane gets a certificate without anybody editing a
//! file here. A second list would be a list to forget to update, and the
//! symptom of forgetting would be a handshake failure on a name that routes
//! perfectly well.
//!
//! Three kinds of name are left out, each for a reason worth stating:
//!
//! * a wildcard (`*.example.com`) cannot be had over `http-01` at all -
//!   RFC 8555 section 7.1.6 allows wildcards only through `dns-01` - so
//!   asking for one would fail every time;
//! * a name an operator has already given a certificate file for stays
//!   theirs; the point of the file is that they chose it;
//! * anything that cannot be a public host name - an address literal, a
//!   single label, a name with a character DNS does not carry - because no
//!   authority will issue for it and every attempt spends a rate limit.

use std::collections::BTreeSet;

use tracing::warn;

use crate::catalog::table::CatalogRoutes;
use crate::config::routes::RouteTable;

/// Longest a DNS name can be, counting the dots (RFC 1035).
const MAX_NAME_LEN: usize = 253;
/// Longest one label between dots can be.
const MAX_LABEL_LEN: usize = 63;

/// Whether a name is one a public authority could plausibly issue for.
///
/// Deliberately not a list of which top-level domains exist: the authority
/// is the judge of what it will issue, and a client that second-guessed it
/// would refuse names that work. This only rules out what is not a host name
/// at all, and what `http-01` structurally cannot do.
pub fn is_certifiable_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN || name.starts_with("*.") {
        return false;
    }
    let labels: Vec<&str> = name.split('.').collect();
    // A single label is either a machine on the local network or something
    // like `localhost`; there is no registered domain for an authority to
    // check against.
    if labels.len() < 2 {
        return false;
    }
    if labels.iter().any(|label| {
        label.is_empty()
            || label.len() > MAX_LABEL_LEN
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }) {
        return false;
    }
    // An IPv4 literal parses as four numeric labels and is not a name; a
    // certificate for one is possible in principle and not over `http-01`
    // through a public authority, so it is left out rather than tried.
    if labels
        .last()
        .is_some_and(|last| last.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    true
}

/// What the renewal pass will try to hold certificates for, in a stable
/// order.
///
/// `include_preview` decides whether the automatic per-environment addresses
/// under `routing.preview_zone` are included; see `AcmeConfig` for why that
/// is off by default. `max` is the ceiling that keeps a route table which
/// suddenly grew from turning into an order per name against the authority's
/// weekly limit.
pub fn desired_names(
    routes: &RouteTable,
    catalog: &CatalogRoutes,
    include_preview: bool,
    max: usize,
) -> Vec<String> {
    // A set, because the same name reaching this from both tables is the
    // normal case - the control plane publishes a route for a domain an
    // application here also lists - and ordering it twice would be two
    // certificates for one name.
    let mut wanted: BTreeSet<String> = BTreeSet::new();

    for name in routes.served_names() {
        wanted.insert(name);
    }
    for name in catalog.route_hosts() {
        wanted.insert(name);
    }
    if include_preview {
        for name in catalog.preview_hosts() {
            wanted.insert(name);
        }
    }

    let mut chosen: Vec<String> = wanted
        .into_iter()
        .filter(|name| is_certifiable_name(name))
        // The operator's own file wins. Ordering a certificate for a name
        // they already placed one for would spend a rate limit on a
        // certificate that is never selected, since a configured
        // certificate is what the SNI callback finds first.
        .filter(|name| !routes.has_configured_certificate_for(name))
        .collect();

    if chosen.len() > max {
        let dropped = chosen.len() - max;
        warn!(
            wanted = chosen.len(),
            max,
            dropped,
            first_dropped = %chosen[max],
            "more names want automatic certificates than tls.acme.max_names allows, so the last \
             ones in alphabetical order get none and will only answer plain HTTP. The ceiling is \
             there because a certificate authority counts new certificates per registered domain \
             per week and refuses every name under the domain once that is spent, production \
             included. Raise max_names deliberately, or give the extra names one certificate \
             placed by hand"
        );
        chosen.truncate(max);
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use serde_json::{Value as JsonValue, json};

    use crate::catalog::table::PreviewZone;
    use crate::catalog::wire::RoutingTableDocument;
    use crate::config::CertificateConfig;

    fn app(id: &str, domains: &[&str], certificates: &[CertificateConfig]) -> JsonValue {
        json!({
            "id": id,
            "description": "",
            "domains": domains,
            "certificates": certificates,
            "enabled": true,
            "cache": [],
        })
    }

    fn routes(apps: &[JsonValue], default: Option<&CertificateConfig>) -> RouteTable {
        let map: HashMap<String, JsonValue> = apps
            .iter()
            .map(|app| (app["id"].as_str().unwrap().to_string(), app.clone()))
            .collect();
        RouteTable::build(&map, default, &[])
    }

    fn catalog(routes: &str, environments: &str, zone: Option<&str>) -> CatalogRoutes {
        let document: RoutingTableDocument = serde_json::from_str(&format!(
            r#"{{"version":"v1","generated_at":1,"routes":{routes},"environments":{environments}}}"#
        ))
        .unwrap();
        document.validate().unwrap();
        let zone = zone.and_then(PreviewZone::new);
        CatalogRoutes::build(&document, zone.as_ref())
    }

    fn empty_catalog() -> CatalogRoutes {
        catalog("[]", "[]", None)
    }

    #[test]
    fn the_names_come_from_the_applications_this_instance_serves() {
        // The property the whole design rests on: nobody writes a list of
        // names anywhere, so nobody can forget to update one.
        let table = routes(
            &[
                app("shop", &["shop.example.com", "www.shop.example.com"], &[]),
                app("blog", &["blog.example.com"], &[]),
            ],
            None,
        );
        assert_eq!(
            desired_names(&table, &empty_catalog(), false, 20),
            vec![
                "blog.example.com".to_string(),
                "shop.example.com".to_string(),
                "www.shop.example.com".to_string(),
            ]
        );
    }

    #[test]
    fn a_route_the_control_plane_published_is_certified_without_touching_this_machine() {
        // 4.8 put the routing table on the front door; this is the half that
        // makes a domain added there work over HTTPS as well as over HTTP.
        let table = routes(&[], None);
        let catalog = catalog(
            r#"[{"app_id":"shop","environment":"production","host":"added-in-the-dashboard.example.com","path_prefix":"/"}]"#,
            "[]",
            None,
        );
        assert_eq!(
            desired_names(&table, &catalog, false, 20),
            vec!["added-in-the-dashboard.example.com".to_string()]
        );
    }

    #[test]
    fn a_name_in_both_tables_is_ordered_once() {
        // The ordinary case on a configured instance - the control plane
        // publishes a route for a domain the application here also lists -
        // and two orders for one name would be two certificates against the
        // authority's weekly allowance.
        let table = routes(&[app("shop", &["shop.example.com"], &[])], None);
        let catalog = catalog(
            r#"[{"app_id":"shop","environment":"production","host":"shop.example.com","path_prefix":"/"}]"#,
            "[]",
            None,
        );
        assert_eq!(
            desired_names(&table, &catalog, false, 20),
            vec!["shop.example.com".to_string()]
        );
    }

    #[test]
    fn the_automatic_preview_addresses_are_left_out_until_they_are_asked_for() {
        // Off by default because each one spends a certificate out of the
        // fifty a registered domain gets per week, and preview environments
        // come and go with branches. One wildcard covers them all, and a
        // wildcard needs dns-01.
        let table = routes(&[], None);
        let catalog = catalog(
            "[]",
            r#"[{"app_id":"shop","environment":"production"},{"app_id":"shop","environment":"pr-1"}]"#,
            Some("preview.example.com"),
        );
        assert!(desired_names(&table, &catalog, false, 20).is_empty());

        let mut with_previews = desired_names(&table, &catalog, true, 20);
        with_previews.sort();
        assert_eq!(
            with_previews,
            vec![
                // Every environment gets `<env>--<app>`, and the default one
                // also answers at the bare `<app>`, so switching this on
                // costs one certificate per environment plus one.
                "pr-1--shop.preview.example.com".to_string(),
                "production--shop.preview.example.com".to_string(),
                "shop.preview.example.com".to_string(),
            ]
        );
    }

    #[test]
    fn a_name_an_operator_placed_a_certificate_for_is_left_alone() {
        // Their file is a decision. Ordering over it would spend a rate
        // limit on a certificate the handshake never selects, because a
        // configured certificate is what the route table finds first.
        let certificate = CertificateConfig {
            cert_file: "/nonexistent/theirs.pem".to_string(),
            key_file: "/nonexistent/theirs.key".to_string(),
        };
        let table = routes(
            &[
                app(
                    "shop",
                    &["shop.example.com"],
                    std::slice::from_ref(&certificate),
                ),
                app("blog", &["blog.example.com"], &[]),
            ],
            None,
        );
        // The file does not exist, so nothing loaded - and the name is still
        // theirs. Obtaining one because their certificate failed to load
        // would silently take over a name they are in the middle of fixing.
        assert_eq!(
            desired_names(&table, &empty_catalog(), false, 20),
            vec!["blog.example.com".to_string()]
        );
    }

    #[test]
    fn nothing_that_http_01_cannot_certify_is_ever_submitted() {
        let table = routes(
            &[
                app("wild", &["*.example.com"], &[]),
                app("literal", &["10.0.0.1"], &[]),
                app("good", &["ok.example.com"], &[]),
            ],
            None,
        );
        assert_eq!(
            desired_names(&table, &empty_catalog(), false, 20),
            vec!["ok.example.com".to_string()],
            "a wildcard needs dns-01 and an address literal is not a name an authority issues for"
        );
    }

    #[test]
    fn the_ceiling_bounds_what_a_runaway_table_can_order() {
        // A control plane fault or a preview zone that ran away must not
        // become an order per name: exhausting the authority's weekly limit
        // stops issuance for every name under the domain, production
        // included, and there is no way to get it back early.
        let apps: Vec<JsonValue> = (0..30)
            .map(|n| {
                let id = format!("app{n:02}");
                app(&id, &[&format!("host{n:02}.example.com")], &[])
            })
            .collect();
        let table = routes(&apps, None);
        let chosen = desired_names(&table, &empty_catalog(), false, 5);
        assert_eq!(chosen.len(), 5);
        assert_eq!(chosen[0], "host00.example.com");
        assert_eq!(chosen[4], "host04.example.com");
    }

    #[test]
    fn what_a_public_authority_could_issue_for_is_accepted() {
        for good in [
            "example.com",
            "shop.example.com",
            "a-b.example.co.uk",
            "pr-42--shop.preview.example.com",
            "xn--80ak6aa92e.com",
        ] {
            assert!(is_certifiable_name(good), "{good} should be certifiable");
        }
    }

    #[test]
    fn a_wildcard_is_refused_because_http_01_cannot_produce_one() {
        // Not a policy choice: RFC 8555 section 7.1.6 only allows a wildcard
        // identifier to be validated by dns-01, so an order for one would be
        // refused by every authority. Asking anyway would turn one
        // misconfigured zone into a permanent stream of failures.
        assert!(!is_certifiable_name("*.example.com"));
        assert!(!is_certifiable_name("*"));
    }

    #[test]
    fn what_is_not_a_host_name_is_refused() {
        for bad in [
            "",
            "localhost",
            "127.0.0.1",
            "192.168.1.10",
            "example..com",
            "-lead.example.com",
            "trail-.example.com",
            "Example.COM",
            "under_score.example.com",
            "space name.example.com",
            "[::1]",
        ] {
            assert!(!is_certifiable_name(bad), "{bad} should not be certifiable");
        }
        assert!(!is_certifiable_name(&format!("{}.com", "a".repeat(64))));
        assert!(!is_certifiable_name(&"a.".repeat(200)));
    }
}
