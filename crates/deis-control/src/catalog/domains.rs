//! Who may claim which hostname.
//!
//! A hostname is the one namespace on this platform that is **global**. An
//! application id belongs to whoever created the application, an environment
//! name belongs to its application, and a secret name belongs to its
//! environment: each of them is a name inside something the scope grammar can
//! already spell. `shop.example.com` is not. It belongs to whoever registered
//! the domain, which is a fact about the world outside this database, and
//! `deploy:<app>/<env>` has no way to say it.
//!
//! That gap was a hole with a name. `POST
//! /api/apps/<app>/environments/<env>/routes` is authorised against the
//! application and environment **in the URL** - the caller's own - while the
//! host being claimed is in the request body and was checked for nothing but
//! its shape. A credential scoped to `deploy:attacker/production`, refused
//! `GET /api/apps/victim` with 403, could post `{"host":
//! "victim.example.com"}` to its own environment's route list, be answered
//! 201, and take delivery of every request to that name: cookies,
//! `Authorization` headers, POST bodies. The edge would then order a
//! certificate for it.
//!
//! # What this module is
//!
//! A registry the installation writes down: *this pattern may be claimed by
//! that application*. It is consulted by [`super::store::CatalogStore`] on
//! every route write, in the same database call that inserts the row, and it
//! **fails closed** - a name nobody has been granted may be claimed by
//! nobody, whatever their scopes and whatever their role. An owner who wants
//! a name creates the grant first; that is one extra call, and it is the call
//! that leaves the record of who was given what.
//!
//! # The three shapes, and why `*.` means the same thing everywhere
//!
//! A route claims one of three things ([`HostClaim`]):
//!
//! * `Exact("shop.example.com")` - that one name;
//! * `Under("example.com")`, written `*.example.com` - every name *strictly*
//!   under the zone, which is exactly what the edge matches
//!   (`deis_proxy::catalog::table`: `host.len() > suffix.len() &&
//!   host.ends_with(".example.com")`), so `example.com` itself is **not**
//!   included;
//! * `EveryName` - a route with no host at all, which answers for every name
//!   no more specific route claims. It is the widest claim there is.
//!
//! A grant ([`GrantPattern`]) is written in the same spelling and read the
//! same way, so `*.example.com` never means one thing in a grant and another
//! in a route. Granting the zone therefore does not grant the apex; the apex
//! is its own name and gets its own row. That asymmetry is deliberate: an
//! operator who writes `*.example.com` meaning "the subdomains" gets the
//! subdomains, and an application cannot quietly acquire `example.com`
//! because somebody was generous about what is under it.
//!
//! # Most specific wins, and it has to name you
//!
//! Grants overlap on purpose - a platform grants a team its zone and then
//! delegates one name inside it to somebody else. So the rule is not "any
//! covering grant will do": among every grant that covers the claim, only the
//! **most specific** ones are consulted, and one of those has to name this
//! application and environment. `*.example.com` to `team-a` and
//! `b.example.com` to `team-b` means `team-a` cannot claim `b.example.com`,
//! which is what delegation has to mean if it means anything.
//!
//! Two grants can share a pattern (the primary key is the pattern *and* its
//! grantee), because two applications legitimately share a zone and because
//! an installation upgrading into this rule has existing routes to account
//! for. Grants at equal specificity are peers: either may claim, and the
//! `(host, path_prefix)` uniqueness in [`super::store`] decides which one
//! actually holds a given address.
//!
//! # What a delegation does not do
//!
//! A `Name` grant inside somebody else's `Zone` constrains the **exact**
//! claim and nothing more. The zone holder may still create the wildcard
//! route `*.example.com`: that claim is `Under(example.com)`, a `Name` grant
//! is not a zone and so is never in the set covering it
//! ([`GrantPattern::covers`]), and the zone grant is. At the edge an exact
//! host beats a wildcard (`deis_proxy::catalog::table`), so until `team-b`
//! creates its exact route for `b.example.com`, requests to that name reach
//! `team-a`'s wildcard route. This is the precedence the edge has always
//! had; the registry decides who may *write* a route, not which existing
//! route answers. A delegate that wants its name answered only by itself
//! creates its route first, and `GET /api/routing-table` shows who answers
//! for what in the meantime. Changing the resolution order instead would
//! change what every existing wildcard route serves, which is not this
//! module's call to make.

use serde::Serialize;

use super::CatalogError;

/// Longest hostname DNS carries, counting the dots (RFC 1035).
pub const MAX_HOST_LEN: usize = 253;
/// Longest single label between dots.
pub const MAX_LABEL_LEN: usize = 63;

/// The spelling that means "any name at all", in a grant.
pub const ANY_NAME: &str = "*";
/// The spelling that means "the route that names no host, and nothing else",
/// in a grant. A catch-all route answers only for names no more specific
/// route claims, so permission to hold one is a *narrow* thing -- and it
/// needs its own spelling precisely because the only other pattern covering
/// it is [`ANY_NAME`], which also lets its holder claim every hostname on
/// the platform. Those are different statements, and an installation adopted
/// from the pre-registry world holds this one, not that one. The
/// parentheses keep it out of the hostname grammar: no DNS name can ever
/// collide with it.
pub const NO_HOST_PATTERN: &str = "(no-host)";
/// The spelling that means "any environment of this application", in a
/// grant's environment column. No environment name can be this: an
/// environment name is lowercase letters, digits, `-` and `_`
/// ([`super::model::validate_environment_name`]).
pub const EVERY_ENVIRONMENT: &str = "*";

/// What a route claims out of the global hostname namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostClaim {
    /// A route that names no host. It answers for every name no more
    /// specific route claims, so it is the widest claim on the platform.
    EveryName,
    /// One name, exactly.
    Exact(String),
    /// Every name strictly under this zone - `*.example.com` covering
    /// `a.example.com` and `a.b.example.com`, and not `example.com`.
    Under(String),
}

impl HostClaim {
    /// Read the `host` field of a route request.
    ///
    /// `None` - the field omitted or null - is [`HostClaim::EveryName`],
    /// which is what the catalog has always meant by a null host and what
    /// makes the widest claim on this platform go through the same gate as
    /// the narrowest.
    ///
    /// Not trimmed, unlike [`GrantPattern::parse`]. A grant is an
    /// administrator typing a name into a registry and the stored row is
    /// canonical either way; a claim is a tenant naming an address, and
    /// trimming it would be a rewrite - one string checked, another string
    /// matched. `" example.com"` is refused, with a message saying what to
    /// write.
    pub fn parse(host: Option<&str>) -> Result<Self, CatalogError> {
        let Some(host) = host else {
            return Ok(HostClaim::EveryName);
        };
        match host.strip_prefix("*.") {
            Some(zone) => Ok(HostClaim::Under(canonical_name(zone, host)?)),
            None => Ok(HostClaim::Exact(canonical_name(host, host)?)),
        }
    }

    /// How this claim is stored in the `routes` table and served to the
    /// edge. `None` for [`HostClaim::EveryName`], which is the null host.
    ///
    /// This is the *canonical* spelling and it is the only one that is ever
    /// written, which is what keeps the control plane and the edge looking at
    /// one string. `deis_proxy::config::routes::normalize_host` lowercases
    /// the stored host and strips a port before matching, so a row saying
    /// `VICTIM.example.com:8443` used to be checked as one name and matched
    /// as another. [`canonical_name`] refuses both spellings rather than
    /// rewriting them, so there is one representation and no rewrite to get
    /// wrong.
    pub fn stored(&self) -> Option<String> {
        match self {
            HostClaim::EveryName => None,
            HostClaim::Exact(name) => Some(name.clone()),
            HostClaim::Under(zone) => Some(format!("*.{zone}")),
        }
    }

    /// How the claim reads in a refusal, a log line and an audit row.
    pub fn describe(&self) -> String {
        match self {
            HostClaim::EveryName => "every host (a route with no host)".to_string(),
            HostClaim::Exact(name) => name.clone(),
            HostClaim::Under(zone) => format!("*.{zone}"),
        }
    }

    /// The grant an operator would have to write to allow this claim, so a
    /// refusal can say what to do about itself.
    pub fn grant_needed(&self) -> String {
        match self {
            HostClaim::EveryName => NO_HOST_PATTERN.to_string(),
            HostClaim::Exact(name) => name.clone(),
            HostClaim::Under(zone) => format!("*.{zone}"),
        }
    }
}

/// What one grant allows to be claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantPattern {
    /// `*` - every name, including the route that names no host. This is the
    /// old unrestricted behaviour, written down as one row so that an
    /// installation which wants it has said so and an audit can see who has
    /// it.
    Anything,
    /// One name, exactly.
    Name(String),
    /// Every name strictly under a zone, read exactly as a wildcard route
    /// is: `*.example.com` does not include `example.com`.
    Zone(String),
    /// The route that names no host, and nothing else. See
    /// [`NO_HOST_PATTERN`].
    NoHost,
}

impl GrantPattern {
    pub fn parse(text: &str) -> Result<Self, CatalogError> {
        let text = text.trim();
        if text == ANY_NAME {
            return Ok(GrantPattern::Anything);
        }
        if text == NO_HOST_PATTERN {
            return Ok(GrantPattern::NoHost);
        }
        match text.strip_prefix("*.") {
            Some(zone) => Ok(GrantPattern::Zone(canonical_name(zone, text)?)),
            None => Ok(GrantPattern::Name(canonical_name(text, text)?)),
        }
    }

    /// The canonical spelling, which is what the `pattern` column holds.
    pub fn as_text(&self) -> String {
        match self {
            GrantPattern::Anything => ANY_NAME.to_string(),
            GrantPattern::Name(name) => name.clone(),
            GrantPattern::Zone(zone) => format!("*.{zone}"),
            GrantPattern::NoHost => NO_HOST_PATTERN.to_string(),
        }
    }

    /// Whether every name this claim can answer for is a name this grant
    /// allows.
    ///
    /// Set containment, deliberately, rather than a string comparison: a
    /// claim is allowed exactly when it cannot reach a name the grant does
    /// not already cover.
    pub fn covers(&self, claim: &HostClaim) -> bool {
        match (self, claim) {
            (GrantPattern::Anything, _) => true,
            // The catch-all claim is covered by exactly two patterns: the
            // widest grant there is, and the one that says only this. A
            // name or a zone never covers it, because a route with no host
            // answers for names its holder was never granted.
            (GrantPattern::NoHost, HostClaim::EveryName) => true,
            (GrantPattern::NoHost, _) => false,
            (_, HostClaim::EveryName) => false,

            (GrantPattern::Name(granted), HostClaim::Exact(name)) => granted == name,
            // A name is not a zone: holding `example.com` does not let you
            // claim `*.example.com`, which answers for names you were never
            // given.
            (GrantPattern::Name(_), HostClaim::Under(_)) => false,

            (GrantPattern::Zone(zone), HostClaim::Exact(name)) => is_strictly_under(name, zone),
            // `*.a.example.com` is inside `*.example.com`; so is
            // `*.example.com` itself, which claims exactly what the grant
            // allows.
            (GrantPattern::Zone(zone), HostClaim::Under(claimed)) => {
                claimed == zone || is_strictly_under(claimed, zone)
            }
        }
    }

    /// How narrow this grant is. Only the narrowest grants covering a claim
    /// get a say in it; see the module header.
    ///
    /// An exact name outranks every zone, and a longer zone outranks a
    /// shorter one, so a delegation inside a zone is never overruled by the
    /// zone it sits in.
    pub fn specificity(&self) -> (u8, usize) {
        match self {
            GrantPattern::Anything => (0, 0),
            GrantPattern::Zone(zone) => (1, zone.len()),
            GrantPattern::Name(name) => (2, name.len()),
            // Covers a single claim, the way an exact name does. It never
            // competes with a name or a zone -- no hostname claim matches it
            // -- so the only ranking that matters is that it outranks
            // [`GrantPattern::Anything`] on the catch-all claim.
            GrantPattern::NoHost => (2, 0),
        }
    }
}

/// Whether `name` sits strictly beneath `zone`, which is how a wildcard is
/// matched at the edge and therefore how it is read here.
fn is_strictly_under(name: &str, zone: &str) -> bool {
    name.len() > zone.len() + 1
        && name.ends_with(zone)
        && name.as_bytes()[name.len() - zone.len() - 1] == b'.'
}

/// One row of the registry.
#[derive(Debug, Clone, Serialize)]
pub struct DomainGrant {
    /// Canonical pattern text: `example.com`, `*.example.com`, `(no-host)`
    /// or `*`.
    pub pattern: String,
    /// The application allowed to claim inside it.
    pub app_id: String,
    /// The environment, or [`EVERY_ENVIRONMENT`] for all of them.
    pub environment: String,
    pub created_at: i64,
    /// Free text an operator leaves for the next operator. Never read by
    /// anything here.
    pub note: Option<String>,
}

impl DomainGrant {
    /// The parsed pattern. A row whose text does not parse is not silently
    /// ignored - see [`grant_allows`], which refuses the claim instead.
    pub fn parsed(&self) -> Result<GrantPattern, CatalogError> {
        GrantPattern::parse(&self.pattern)
    }

    fn names(&self, app_id: &str, environment: &str) -> bool {
        self.app_id == app_id
            && (self.environment == EVERY_ENVIRONMENT || self.environment == environment)
    }
}

/// Why a claim was refused, with everything a refusal message needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRefusal {
    /// The claim, as an operator wrote it.
    pub claim: String,
    pub app_id: String,
    pub environment: String,
    /// The pattern that would have to be granted, when nothing covers the
    /// claim at all.
    pub grant_needed: String,
    /// Who does hold the narrowest covering grant, when somebody does. This
    /// is the delegation case, and naming the holder is what turns "no" into
    /// something an operator can act on.
    pub held_by: Vec<String>,
    /// The pattern those holders hold.
    pub held_pattern: Option<String>,
}

impl std::fmt::Display for ClaimRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.held_pattern, self.held_by.is_empty()) {
            (Some(pattern), false) => write!(
                f,
                "{}/{} may not claim {}: a hostname is global on this platform and the closest \
                 grant covering that name is {pattern}, held by {}. Ask an owner to delegate the \
                 name (`POST /api/domains` with pattern {}, app_id {}, environment {}), or claim \
                 a name you were granted. No route was created",
                self.app_id,
                self.environment,
                self.claim,
                self.held_by.join(", "),
                self.grant_needed,
                self.app_id,
                self.environment,
            ),
            _ => write!(
                f,
                "{}/{} may not claim {}: a hostname is global on this platform, so it has to be \
                 granted before anybody can route it, and nothing on this installation grants \
                 that name to anyone. An owner grants it with `POST /api/domains` (pattern {}, \
                 app_id {}, environment {}). No route was created",
                self.app_id,
                self.environment,
                self.claim,
                self.grant_needed,
                self.app_id,
                self.environment,
            ),
        }
    }
}

/// The one rule. Whether this application and environment may claim this
/// host, given every grant the installation has written down.
///
/// Fails closed three ways, each of which has been a hole somewhere:
///
/// * no covering grant at all - refused, whoever is asking;
/// * a covering grant that names somebody else and is narrower than any
///   grant of ours - refused, which is what makes delegation binding;
/// * a grant row whose pattern does not parse - refused rather than skipped,
///   because a row this cannot read is a row whose meaning is unknown, and
///   an unreadable grant that silently disappeared would widen the platform
///   exactly when somebody had fat-fingered a registry entry.
pub fn grant_allows(
    grants: &[DomainGrant],
    claim: &HostClaim,
    app_id: &str,
    environment: &str,
) -> Result<(), Box<ClaimRefusal>> {
    let mut best: Option<(u8, usize)> = None;
    let mut covering: Vec<(&DomainGrant, GrantPattern)> = Vec::new();

    for grant in grants {
        let Ok(pattern) = grant.parsed() else {
            // Named, not skipped: an operator has to be able to find it.
            tracing::error!(
                pattern = %grant.pattern,
                app_id = %grant.app_id,
                environment = %grant.environment,
                "A domain grant row does not parse, so what it permits cannot be established. It \
                 grants nothing until it is corrected or deleted, which means route claims that \
                 depended on it are refused rather than waved through"
            );
            continue;
        };
        if !pattern.covers(claim) {
            continue;
        }
        let specificity = pattern.specificity();
        if best.is_none_or(|current| specificity > current) {
            best = Some(specificity);
        }
        covering.push((grant, pattern));
    }

    let Some(best) = best else {
        return Err(Box::new(ClaimRefusal {
            claim: claim.describe(),
            app_id: app_id.to_string(),
            environment: environment.to_string(),
            grant_needed: claim.grant_needed(),
            held_by: Vec::new(),
            held_pattern: None,
        }));
    };

    let narrowest: Vec<&(&DomainGrant, GrantPattern)> = covering
        .iter()
        .filter(|(_, pattern)| pattern.specificity() == best)
        .collect();

    if narrowest
        .iter()
        .any(|(grant, _)| grant.names(app_id, environment))
    {
        return Ok(());
    }

    Err(Box::new(ClaimRefusal {
        claim: claim.describe(),
        app_id: app_id.to_string(),
        environment: environment.to_string(),
        grant_needed: claim.grant_needed(),
        held_by: narrowest
            .iter()
            .map(|(grant, _)| format!("{}/{}", grant.app_id, grant.environment))
            .collect(),
        held_pattern: narrowest.first().map(|(_, pattern)| pattern.as_text()),
    }))
}

/// The single spelling of a hostname this control plane will store.
///
/// Not a normaliser. It **refuses** everything that is not already canonical
/// instead of rewriting it, and the difference matters: a rewrite is a second
/// representation somewhere, and this repository has shipped the bug where a
/// check reads one spelling and the thing being guarded uses another. The
/// edge lowercases the stored host and strips a port before it matches
/// (`deis_proxy::config::routes::normalize_host`), so `Shop.Example.COM:8443`
/// stored here would be checked as itself and matched as `shop.example.com`.
/// Refusing it, with a message saying what to write instead, leaves exactly
/// one string for the grant, the row and the match.
///
/// `whole` is the spelling the caller sent, so the message can quote it back
/// including any `*.` this has already stripped.
fn canonical_name(name: &str, whole: &str) -> Result<String, CatalogError> {
    if name.is_empty() {
        return Err(CatalogError::invalid(format!(
            "route host {whole:?} is not a hostname; omit the host entirely to match every domain"
        )));
    }
    if whole.len() > MAX_HOST_LEN {
        return Err(CatalogError::invalid(format!(
            "route host {whole:?} is longer than {MAX_HOST_LEN} characters"
        )));
    }
    if name.contains(':') {
        return Err(CatalogError::invalid(format!(
            "route host {whole:?} carries a port. A port is never part of the match - the edge \
             strips it from the request and from this row before comparing them - so storing one \
             means checking a name that is not the name traffic arrives on. Write the host alone"
        )));
    }
    if name.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(CatalogError::invalid(format!(
            "route host {whole:?} has capital letters. Host names are matched in lower case (the \
             edge lower-cases both sides), so an upper-case row would be checked as one name and \
             matched as another. Write {:?}",
            whole.to_ascii_lowercase()
        )));
    }
    for label in name.split('.') {
        if label.is_empty() {
            return Err(CatalogError::invalid(format!(
                "route host {whole:?} has an empty label: no leading or trailing dot, and no \
                 '..'. A trailing dot is a different string from the name without one, and only \
                 one of them would ever match"
            )));
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(CatalogError::invalid(format!(
                "route host {whole:?} has a label longer than {MAX_LABEL_LEN} characters"
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(CatalogError::invalid(format!(
                "route host {whole:?} has a label that starts or ends with '-', which DNS does \
                 not carry"
            )));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(CatalogError::invalid(format!(
                "route host {whole:?} may only contain lower-case letters, digits, '-' and '.', \
                 with an optional leading '*.' for a wildcard"
            )));
        }
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(pattern: &str, app: &str, environment: &str) -> DomainGrant {
        DomainGrant {
            pattern: pattern.to_string(),
            app_id: app.to_string(),
            environment: environment.to_string(),
            created_at: 0,
            note: None,
        }
    }

    fn claim(host: &str) -> HostClaim {
        HostClaim::parse(Some(host)).expect("a claim the tests mean to be valid")
    }

    /// The reproduction, in one test: a credential that may deploy
    /// `attacker/production` posts the victim's hostname to its own
    /// environment's route list.
    #[test]
    fn a_tenant_cannot_claim_a_name_granted_to_another() {
        let grants = [grant("victim.example.com", "victim", "production")];
        let refusal = grant_allows(
            &grants,
            &claim("victim.example.com"),
            "attacker",
            "production",
        )
        .expect_err("the attacker holds no grant for the victim's name");
        assert_eq!(refusal.held_by, vec!["victim/production".to_string()]);
        assert!(
            refusal.to_string().contains("victim.example.com"),
            "{refusal}"
        );

        grant_allows(
            &grants,
            &claim("victim.example.com"),
            "victim",
            "production",
        )
        .expect("the application the name was granted to may claim it");
    }

    /// The half that makes this worth having: with no registry at all,
    /// nobody claims anything.
    #[test]
    fn a_name_nobody_was_granted_is_claimable_by_nobody() {
        let refusal = grant_allows(&[], &claim("anything.example.com"), "any", "production")
            .expect_err("an empty registry grants nothing");
        assert!(refusal.held_by.is_empty());
        assert_eq!(refusal.grant_needed, "anything.example.com");

        // Including the widest claim of all, which is the one a caller
        // reaches for when a specific name is refused.
        assert!(grant_allows(&[], &HostClaim::EveryName, "any", "production").is_err());
    }

    /// The catch-all is the widest claim on the platform - it answers for
    /// every name no other route claims - so only the widest grant covers it.
    #[test]
    fn the_route_with_no_host_needs_a_grant_that_says_exactly_that() {
        for pattern in ["example.com", "*.example.com"] {
            assert!(
                grant_allows(
                    &[grant(pattern, "app", "production")],
                    &HostClaim::EveryName,
                    "app",
                    "production"
                )
                .is_err(),
                "{pattern} must not let an application answer for every host"
            );
        }
        for pattern in [NO_HOST_PATTERN, "*"] {
            grant_allows(
                &[grant(pattern, "app", "production")],
                &HostClaim::EveryName,
                "app",
                "production",
            )
            .unwrap_or_else(|refusal| {
                panic!("{pattern} covers the route that names no host: {refusal}")
            });
        }
    }

    /// Holding the fallback is not holding the namespace.
    ///
    /// This is the row adoption writes for a pre-registry catch-all route,
    /// and it is the difference between "this tenant may keep its fallback"
    /// and "this tenant may claim any hostname on the platform". The first
    /// migration wrote `*` here, and on any upgraded installation where any
    /// tenant held a catch-all - the ordinary way to serve an app on the
    /// default host - the hostname hijack the registry exists to stop was
    /// back, for every name its victim had not already routed.
    #[test]
    fn the_no_host_grant_covers_the_fallback_and_no_hostname_at_all() {
        let grants = [grant(NO_HOST_PATTERN, "adopted", "production")];
        grant_allows(&grants, &HostClaim::EveryName, "adopted", "production")
            .expect("the adopted tenant may keep, and re-create, its catch-all");
        for claim in [
            HostClaim::Exact("victim.example.com".to_string()),
            HostClaim::Under("example.com".to_string()),
        ] {
            assert!(
                grant_allows(&grants, &claim, "adopted", "production").is_err(),
                "the fallback grant let its holder claim {}",
                claim.describe()
            );
        }
    }

    /// A zone means what it means at the edge: the names under it, not the
    /// name itself.
    #[test]
    fn a_zone_grant_covers_what_a_wildcard_route_matches_and_no_more() {
        let grants = [grant("*.example.com", "app", "production")];
        for allowed in [
            "a.example.com",
            "a.b.example.com",
            "*.example.com",
            "*.b.example.com",
        ] {
            grant_allows(&grants, &claim(allowed), "app", "production")
                .unwrap_or_else(|e| panic!("{allowed} is inside the zone: {e}"));
        }
        for refused in ["example.com", "notexample.com", "example.com.evil.test"] {
            assert!(
                grant_allows(&grants, &claim(refused), "app", "production").is_err(),
                "{refused} is not inside *.example.com"
            );
        }
    }

    /// Holding one name is not holding the zone beneath it. Otherwise
    /// granting `example.com` would quietly hand over every subdomain.
    #[test]
    fn a_name_grant_is_not_a_zone_grant() {
        let grants = [grant("example.com", "app", "production")];
        grant_allows(&grants, &claim("example.com"), "app", "production").expect("its own name");
        assert!(grant_allows(&grants, &claim("*.example.com"), "app", "production").is_err());
        assert!(grant_allows(&grants, &claim("a.example.com"), "app", "production").is_err());
    }

    /// Delegation has to bind the zone's holder, or it is not delegation.
    #[test]
    fn the_narrowest_grant_decides_even_against_the_zone_it_sits_in() {
        let grants = [
            grant("*.example.com", "team-a", EVERY_ENVIRONMENT),
            grant("b.example.com", "team-b", EVERY_ENVIRONMENT),
        ];
        grant_allows(&grants, &claim("a.example.com"), "team-a", "production")
            .expect("team-a holds the zone");
        grant_allows(&grants, &claim("b.example.com"), "team-b", "production")
            .expect("team-b was delegated this one name");
        let refusal = grant_allows(&grants, &claim("b.example.com"), "team-a", "production")
            .expect_err("the delegation outranks the zone");
        assert_eq!(refusal.held_by, vec!["team-b/*".to_string()]);
        assert_eq!(refusal.held_pattern.as_deref(), Some("b.example.com"));
    }

    /// `*` does not become a way around a delegation either: it is the least
    /// specific pattern there is.
    #[test]
    fn the_widest_grant_is_the_least_specific_one() {
        let grants = [
            grant("*", "platform", EVERY_ENVIRONMENT),
            grant("shop.example.com", "shop", "production"),
        ];
        grant_allows(&grants, &claim("shop.example.com"), "shop", "production")
            .expect("the exact grant names it");
        assert!(
            grant_allows(
                &grants,
                &claim("shop.example.com"),
                "platform",
                "production"
            )
            .is_err(),
            "holding `*` must not overrule a name somebody was given"
        );
        grant_allows(
            &grants,
            &claim("other.example.com"),
            "platform",
            "production",
        )
        .expect("`*` still covers everything nobody else was given");
    }

    /// Two applications legitimately share a zone, and an installation
    /// upgrading into this rule has existing routes that need it.
    #[test]
    fn grants_at_equal_specificity_are_peers() {
        let grants = [
            grant("shared.example.com", "one", "production"),
            grant("shared.example.com", "two", "production"),
        ];
        for app in ["one", "two"] {
            grant_allows(&grants, &claim("shared.example.com"), app, "production")
                .unwrap_or_else(|e| panic!("{app} shares this name: {e}"));
        }
        assert!(
            grant_allows(&grants, &claim("shared.example.com"), "three", "production").is_err()
        );
    }

    #[test]
    fn an_environment_wildcard_covers_every_environment_and_a_name_covers_one() {
        let wide = [grant("app.example.com", "app", EVERY_ENVIRONMENT)];
        for environment in ["production", "pr-42"] {
            grant_allows(&wide, &claim("app.example.com"), "app", environment)
                .unwrap_or_else(|e| panic!("{environment}: {e}"));
        }

        let narrow = [grant("app.example.com", "app", "production")];
        grant_allows(&narrow, &claim("app.example.com"), "app", "production").expect("named");
        assert!(
            grant_allows(&narrow, &claim("app.example.com"), "app", "pr-42").is_err(),
            "a grant naming one environment must not cover another"
        );
    }

    /// A row this cannot read grants nothing. Skipping it would have been
    /// the friendly choice and the wrong one: the claim then falls through to
    /// whatever else happens to cover it.
    #[test]
    fn a_grant_row_that_does_not_parse_grants_nothing() {
        let grants = [grant("NOT A PATTERN", "app", "production")];
        assert!(grant_allows(&grants, &claim("not.a.pattern"), "app", "production").is_err());
    }

    /// One spelling, or none. Every rejection here is a string the edge would
    /// have matched differently from the way this control plane checked it.
    #[test]
    fn only_the_spelling_the_edge_matches_is_stored() {
        assert_eq!(
            HostClaim::parse(Some("shop.example.com")).unwrap().stored(),
            Some("shop.example.com".to_string())
        );
        assert_eq!(
            HostClaim::parse(Some("*.example.com")).unwrap().stored(),
            Some("*.example.com".to_string())
        );
        assert_eq!(HostClaim::parse(None).unwrap().stored(), None);

        // Each of these is a name the edge normalises to something else.
        for refused in [
            "Shop.Example.com",
            "shop.example.com:8443",
            "shop.example.com.",
            ".shop.example.com",
            "shop..example.com",
            "shop example.com",
            "-shop.example.com",
            "shop-.example.com",
            "under_score.example.com",
            "",
            "*.",
            "*",
        ] {
            assert!(
                HostClaim::parse(Some(refused)).is_err(),
                "{refused:?} must not be storable: the edge would match a different string"
            );
        }
        assert!(HostClaim::parse(Some(&format!("{}.com", "a".repeat(64)))).is_err());
        assert!(HostClaim::parse(Some(&"a.".repeat(200))).is_err());
    }

    /// A grant is written in the same spelling as a claim, and `*` is the one
    /// extra word it knows.
    #[test]
    fn a_grant_pattern_is_a_claim_spelling_plus_the_word_for_everything() {
        assert_eq!(GrantPattern::parse("*").unwrap(), GrantPattern::Anything);
        assert_eq!(
            GrantPattern::parse("example.com").unwrap().as_text(),
            "example.com"
        );
        assert_eq!(
            GrantPattern::parse("*.example.com").unwrap().as_text(),
            "*.example.com"
        );
        for refused in ["Example.com", "example.com:443", "", "*.*", "a b"] {
            assert!(
                GrantPattern::parse(refused).is_err(),
                "{refused:?} must not be a grant pattern"
            );
        }
    }

    /// The suffix test is on label boundaries, not on letters. Without that,
    /// a grant for `*.example.com` would cover `notexample.com`.
    #[test]
    fn a_zone_matches_whole_labels() {
        assert!(is_strictly_under("a.example.com", "example.com"));
        assert!(!is_strictly_under("notexample.com", "example.com"));
        assert!(!is_strictly_under("example.com", "example.com"));
        assert!(!is_strictly_under("com", "example.com"));
    }
}
