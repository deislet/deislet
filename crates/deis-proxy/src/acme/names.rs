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
//! When more names want certificates than the ceiling allows, the ceiling is
//! shared rather than taken first-come; see [`desired_names`] for the policy
//! and for why alphabetical truncation was a hole.
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

use std::collections::{BTreeMap, BTreeSet};

use crate::catalog::table::CatalogRoutes;
use crate::config::routes::RouteTable;

/// A name that wanted a certificate and did not fit under the ceiling, with
/// whose it is.
///
/// Returned by name rather than summed, because a count tells an operator
/// that something is uncovered and not what: the name a tenant just added
/// is one answer, the production domain that has quietly stopped renewing
/// is another, and the log line for each pass has to be able to say which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedName {
    pub name: String,
    /// The application it came from, or this instance's own configuration.
    pub owner: String,
}

/// What one pass will and will not hold certificates for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NameSelection {
    /// The names the pass covers, in a stable order.
    pub wanted: Vec<String>,
    /// The names that wanted a certificate and did not fit, in a stable order.
    pub dropped: Vec<DroppedName>,
}

/// The owner label for names out of this instance's own configuration.
pub const OWN_CONFIGURATION: &str = "this instance's own configuration";

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
///
/// # Who gets the ceiling when it is not enough for everybody
///
/// This used to be one alphabetical list, truncated. On a platform where
/// tenants create their own routes that is a hole with a shape: thirty routes
/// named `a001..a030.flood.example` sort before everything and take the whole
/// ceiling, and every real domain on the platform silently drops out of
/// certificate renewal and answers plain HTTP. Nobody has to break in to do
/// it - it is what the ordering does to whoever names things late in the
/// alphabet.
///
/// So the ceiling is shared, in two steps, and the order of the two is the
/// policy:
///
/// 1. **The operator's own file first.** Names from this instance's
///    configuration are the operator's decision and no tenant can add to that
///    file, so they are not in competition with anybody. They take what they
///    need, in order.
/// 2. **Then one name per application, round after round**, applications in a
///    stable order, until the ceiling is reached. Every application gets its
///    first name before any application gets its second, so an application
///    with thirty names cannot displace an application with one. Inside an
///    application the order is alphabetical, which only decides which of
///    *its own* names it gets - a cost it pays itself.
///
/// The ceiling is still a ceiling: on a platform that wants more names than
/// it, somebody goes without, and the log says who. What changed is that the
/// somebody is no longer decided by an attacker's choice of hostname.
pub fn desired_names(
    routes: &RouteTable,
    catalog: &CatalogRoutes,
    include_preview: bool,
    max: usize,
) -> Vec<String> {
    select_names(routes, catalog, include_preview, max).wanted
}

/// [`desired_names`], plus every name that did not fit and whose it was.
///
/// The renewal pass logs the dropped names on every pass, not once: a name
/// pushed out of the ceiling that already holds a certificate keeps being
/// served until that certificate expires, and the day it does is weeks after
/// the one warning at startup scrolled away.
pub fn select_names(
    routes: &RouteTable,
    catalog: &CatalogRoutes,
    include_preview: bool,
    max: usize,
) -> NameSelection {
    // The operator's own file wins twice over: a name they already placed a
    // certificate for is dropped entirely (ordering over it would spend a
    // rate limit on a certificate the handshake never selects, since a
    // configured certificate is what the SNI callback finds first), and what
    // is left takes the ceiling before any tenant's name does.
    let wanted =
        |name: &String| is_certifiable_name(name) && !routes.has_configured_certificate_for(name);

    let configured: BTreeSet<String> = routes.served_names().into_iter().filter(wanted).collect();

    // A set per application, because the same name reaching this from two of
    // an application's routes is the normal case and ordering it twice would
    // be two certificates for one name.
    let mut by_app: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let add = |by_app: &mut BTreeMap<String, BTreeSet<String>>,
               source: BTreeMap<String, BTreeSet<String>>| {
        for (app, hosts) in source {
            let entry = by_app.entry(app).or_default();
            for host in hosts {
                // Already taken by the operator's own file: not this
                // application's to be charged for, and not to be ordered twice.
                if wanted(&host) && !configured.contains(&host) {
                    entry.insert(host);
                }
            }
        }
    };
    add(&mut by_app, catalog.route_hosts_by_app());
    if include_preview {
        add(&mut by_app, catalog.preview_hosts_by_app());
    }

    let mut chosen: BTreeSet<String> = BTreeSet::new();
    let mut dropped: Vec<DroppedName> = Vec::new();

    for name in &configured {
        if chosen.len() < max {
            chosen.insert(name.clone());
        } else {
            dropped.push(DroppedName {
                name: name.clone(),
                owner: OWN_CONFIGURATION.to_string(),
            });
        }
    }

    // One name per application, round after round. Each application keeps its
    // remaining names in order and an index into them; a round that hands out
    // nothing means every application is finished.
    let mut queues: Vec<(String, Vec<String>, usize)> = by_app
        .into_iter()
        .map(|(app, hosts)| (app, hosts.into_iter().collect(), 0usize))
        .collect();
    'rounds: loop {
        let mut handed_out = false;
        for (_, queue, taken) in queues.iter_mut() {
            // A host two applications both route on is one certificate, and
            // whichever of them reaches it first has paid for it. Skipping it
            // here rather than letting the set swallow it is what keeps the
            // second application's turn from being spent on nothing.
            while *taken < queue.len() && chosen.contains(&queue[*taken]) {
                *taken += 1;
            }
            if *taken >= queue.len() {
                continue;
            }
            if chosen.len() >= max {
                break 'rounds;
            }
            chosen.insert(queue[*taken].clone());
            *taken += 1;
            handed_out = true;
        }
        if !handed_out {
            break;
        }
    }

    for (app, queue, taken) in queues {
        for name in &queue[taken.min(queue.len())..] {
            if !chosen.contains(name) {
                dropped.push(DroppedName {
                    name: name.clone(),
                    owner: app.clone(),
                });
            }
        }
    }

    NameSelection {
        wanted: chosen.into_iter().collect(),
        dropped,
    }
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

    /// The hole this policy exists to close. One application creates thirty
    /// routes named so that they sort first; under alphabetical truncation
    /// every other application's single name fell off the end of the ceiling
    /// and answered plain HTTP for as long as the flood stayed.
    #[test]
    fn one_application_cannot_crowd_every_other_one_out_of_the_ceiling() {
        let flood: String = (0..30)
            .map(|n| {
                format!(
                    r#"{{"app_id":"flood","environment":"production","host":"a{n:03}.flood.example","path_prefix":"/","priority":0}}"#
                )
            })
            .chain(["shop", "blog", "bank"].iter().map(|app| {
                format!(
                    r#"{{"app_id":"{app}","environment":"production","host":"{app}.example.com","path_prefix":"/","priority":0}}"#
                )
            }))
            .collect::<Vec<_>>()
            .join(",");
        let catalog = catalog(&format!("[{flood}]"), "[]", None);
        let table = routes(&[], None);

        let chosen = desired_names(&table, &catalog, false, 5);
        assert_eq!(chosen.len(), 5);
        for real in ["bank.example.com", "blog.example.com", "shop.example.com"] {
            assert!(
                chosen.contains(&real.to_string()),
                "{real} was pushed out of the ceiling by one tenant's flood: {chosen:?}"
            );
        }
        assert_eq!(
            chosen
                .iter()
                .filter(|name| name.ends_with(".flood.example"))
                .count(),
            2,
            "the flooding application gets its share of the ceiling and no more: {chosen:?}"
        );
    }

    /// Every name that did not fit is handed back by name, with its owner,
    /// so the pass can say on every pass which names it is not covering.
    /// A count alone let a production domain drop out of renewal behind a
    /// number that looked like a tenant's overflow.
    #[test]
    fn the_names_that_did_not_fit_are_reported_by_name_and_owner() {
        let table = routes(
            &[app(
                "mine",
                &["operator-a.example.com", "operator-b.example.com"],
                &[],
            )],
            None,
        );
        let catalog = catalog(
            r#"[{"app_id":"tenant","environment":"production","host":"aaa.example.com","path_prefix":"/"},
                {"app_id":"tenant","environment":"production","host":"bbb.example.com","path_prefix":"/"}]"#,
            "[]",
            None,
        );
        let selection = select_names(&table, &catalog, false, 1);
        assert_eq!(selection.wanted, vec!["operator-a.example.com".to_string()]);
        assert_eq!(
            selection.dropped,
            vec![
                DroppedName {
                    name: "operator-b.example.com".to_string(),
                    owner: OWN_CONFIGURATION.to_string(),
                },
                DroppedName {
                    name: "aaa.example.com".to_string(),
                    owner: "tenant".to_string(),
                },
                DroppedName {
                    name: "bbb.example.com".to_string(),
                    owner: "tenant".to_string(),
                },
            ]
        );
        assert!(
            select_names(&table, &catalog, false, 20).dropped.is_empty(),
            "a ceiling nobody presses against drops nothing"
        );
    }

    /// The share is a share, not a cap of one: with room to spare, an
    /// application with several names gets all of them.
    #[test]
    fn a_ceiling_nobody_is_pressing_against_hands_out_every_name() {
        let catalog = catalog(
            r#"[{"app_id":"shop","environment":"production","host":"a.example.com","path_prefix":"/"},
                {"app_id":"shop","environment":"production","host":"b.example.com","path_prefix":"/"},
                {"app_id":"blog","environment":"production","host":"c.example.com","path_prefix":"/"}]"#,
            "[]",
            None,
        );
        assert_eq!(
            desired_names(&routes(&[], None), &catalog, false, 20),
            vec![
                "a.example.com".to_string(),
                "b.example.com".to_string(),
                "c.example.com".to_string(),
            ]
        );
    }

    /// Step one of the policy: this instance's own configuration is the
    /// operator's decision and no tenant can add to it, so it is not in
    /// competition with the catalog for the ceiling.
    #[test]
    fn the_operators_own_file_takes_the_ceiling_before_any_tenant_does() {
        let table = routes(&[app("mine", &["operator.example.com"], &[])], None);
        let catalog = catalog(
            r#"[{"app_id":"tenant","environment":"production","host":"aaa.example.com","path_prefix":"/"},
                {"app_id":"tenant","environment":"production","host":"bbb.example.com","path_prefix":"/"}]"#,
            "[]",
            None,
        );
        // Room for one name only, and `aaa.example.com` sorts first.
        assert_eq!(
            desired_names(&table, &catalog, false, 1),
            vec!["operator.example.com".to_string()],
            "a tenant name that sorts earlier must not displace the operator's own"
        );
    }

    /// A host two applications both route on is one certificate, and taking
    /// it must not spend the second application's turn.
    #[test]
    fn a_host_two_applications_share_costs_one_turn_between_them() {
        let catalog = catalog(
            r#"[{"app_id":"one","environment":"production","host":"shared.example.com","path_prefix":"/"},
                {"app_id":"two","environment":"production","host":"shared.example.com","path_prefix":"/api"},
                {"app_id":"two","environment":"production","host":"own.example.com","path_prefix":"/"}]"#,
            "[]",
            None,
        );
        assert_eq!(
            desired_names(&routes(&[], None), &catalog, false, 2),
            vec![
                "own.example.com".to_string(),
                "shared.example.com".to_string()
            ],
            "the shared name is ordered once and `two` still gets its own name"
        );
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
