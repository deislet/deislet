//! Cloudflare-side names for one application environment's Worker and
//! resources.
//!
//! One Cloudflare account can host many deislet applications and
//! environments, and Cloudflare resource names are account-global (a KV
//! namespace title, a D1 database name, an R2 bucket, a queue, a Worker). A
//! resource named after its logical binding alone - `HOT` - would be shared by
//! every application that declares `HOT`, silently mixing their data. So every
//! name the driver creates is prefixed with a **scope** derived from the
//! application id and the environment id:
//!
//! ```text
//! scope      = first 12 hex of SHA-256("deis-cloudflare-scope/v1\n<app id>\n<env id>")
//! worker     = deis-<scope>
//! resource   = deis-<scope>-<kind>-<logical>     kind: kv | d1 | r2 | q
//! ```
//!
//! The ids, not display names, feed the scope, so renaming an application or
//! a project never orphans its resources, and two environments of one
//! application never share one. The `<logical>` part is the binding name
//! lower-cased with `_` turned into `-` (Cloudflare R2 and Queues accept only
//! lower-case letters, digits and `-`, at most 63 characters); a name too
//! long for that budget keeps a readable prefix plus a hash of the full name.
//! Two logical names that map to the same Cloudflare name are refused rather
//! than merged.

use deis_build_core::BindingKind;
use deis_wire::CanonicalId;
use sha2::{Digest, Sha256};

/// Every Worker and resource name the driver creates starts with this.
pub const NAME_PREFIX: &str = "deis-";
/// Cloudflare's limit for R2 bucket, queue and Worker names.
pub const MAX_NAME_LEN: usize = 63;
const SCOPE_HEX_LEN: usize = 12;

/// The deployment scope of one application environment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeploymentScope {
    scope: String,
}

impl DeploymentScope {
    pub fn new(application_id: &CanonicalId, environment_id: &CanonicalId) -> Self {
        let digest = Sha256::digest(
            format!("deis-cloudflare-scope/v1\n{application_id}\n{environment_id}").as_bytes(),
        );
        Self {
            scope: hex::encode(digest)[..SCOPE_HEX_LEN].to_owned(),
        }
    }

    /// The 12-hex scope token.
    pub fn token(&self) -> &str {
        &self.scope
    }

    /// The Worker script name for this environment.
    pub fn worker_name(&self) -> String {
        format!("{NAME_PREFIX}{}", self.scope)
    }

    /// The prefix every resource of this environment carries.
    pub fn resource_prefix(&self) -> String {
        format!("{NAME_PREFIX}{}-", self.scope)
    }

    /// The Cloudflare name of one logical resource.
    pub fn resource_name(&self, kind: BindingKind, logical: &str) -> String {
        let prefix = format!("{}{}-", self.resource_prefix(), kind_token(kind));
        let budget = MAX_NAME_LEN - prefix.len();
        let mut slug = slug(logical);
        if slug.len() > budget {
            let hash = &hex::encode(Sha256::digest(logical.as_bytes()))[..8];
            let keep = budget - hash.len() - 1;
            slug.truncate(keep);
            let slug = slug.trim_end_matches('-');
            return format!("{prefix}{slug}-{hash}");
        }
        if slug.is_empty() {
            slug = hex::encode(Sha256::digest(logical.as_bytes()))[..8].to_owned();
        }
        format!("{prefix}{slug}")
    }

    /// Whether `name` belongs to this scope.
    pub fn owns(&self, name: &str) -> bool {
        name.starts_with(&self.resource_prefix())
    }
}

/// The short kind token in a resource name.
pub fn kind_token(kind: BindingKind) -> &'static str {
    match kind {
        BindingKind::Kv => "kv",
        BindingKind::Database => "d1",
        BindingKind::Blob => "r2",
        BindingKind::Queue => "q",
    }
}

fn slug(logical: &str) -> String {
    let mut out = String::with_capacity(logical.len());
    for c in logical.chars() {
        let mapped = if c.is_ascii_alphanumeric() {
            c.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' && out.ends_with('-') {
            continue;
        }
        out.push(mapped);
    }
    out.trim_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(text: &str) -> CanonicalId {
        text.parse().unwrap()
    }

    fn scope(app: &str, env: &str) -> DeploymentScope {
        DeploymentScope::new(&id(app), &id(env))
    }

    const APP_A: &str = "01890a5d-ac96-774b-bcce-b302099a8057";
    const APP_B: &str = "01890a5d-ac96-774b-bcce-b302099a8058";
    const ENV_1: &str = "01890a5d-ac96-774b-bcce-b302099a8059";
    const ENV_2: &str = "01890a5d-ac96-774b-bcce-b302099a805a";

    #[test]
    fn different_apps_and_environments_never_share_a_name() {
        let names = [
            scope(APP_A, ENV_1).resource_name(BindingKind::Kv, "HOT"),
            scope(APP_B, ENV_1).resource_name(BindingKind::Kv, "HOT"),
            scope(APP_A, ENV_2).resource_name(BindingKind::Kv, "HOT"),
        ];
        assert_ne!(names[0], names[1]);
        assert_ne!(names[0], names[2]);
        assert_ne!(names[1], names[2]);
        assert_ne!(
            scope(APP_A, ENV_1).worker_name(),
            scope(APP_A, ENV_2).worker_name()
        );
    }

    #[test]
    fn names_are_stable_and_cloudflare_safe() {
        let s = scope(APP_A, ENV_1);
        assert_eq!(s, scope(APP_A, ENV_1));
        let name = s.resource_name(BindingKind::Blob, "USER_FILES");
        assert!(name.starts_with(&s.resource_prefix()));
        assert!(name.ends_with("-r2-user-files"), "{name}");
        assert!(s.owns(&name));
        assert!(!scope(APP_B, ENV_1).owns(&name));
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
    }

    #[test]
    fn long_names_fit_the_limit_and_stay_distinct() {
        let s = scope(APP_A, ENV_1);
        let long_a = "A".repeat(80);
        let long_b = format!("{}B", "A".repeat(79));
        let a = s.resource_name(BindingKind::Queue, &long_a);
        let b = s.resource_name(BindingKind::Queue, &long_b);
        assert!(a.len() <= MAX_NAME_LEN, "{a}");
        assert!(b.len() <= MAX_NAME_LEN, "{b}");
        assert_ne!(a, b);
    }
}
