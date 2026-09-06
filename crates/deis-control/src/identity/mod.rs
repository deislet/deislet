//! Who is calling, and what they are allowed to do.
//!
//! Before this module the whole authorisation model of the human API was one
//! list of strings: `api_auth_enabled` plus `api_keys`. Every key could
//! deploy any application, rotate any secret, mint a storage credential for
//! any tenant and delete anything, and nothing recorded which key did it. On
//! a cluster hosting more than one team's applications that is "everyone with
//! the key is root", and the only way to limit anybody was to give them
//! nothing.
//!
//! What replaces it, in four pieces:
//!
//! * an **account** - a named thing an audit line can blame, with a
//!   [`Role`] that caps what any of its credentials may ever do;
//! * a **token** - one credential belonging to one account, carrying its own
//!   [`scope`] list, stored so that reading the database hands out nothing
//!   (see [`token`]);
//! * a **scope** - `<verb>:<app>/<environment>`, checked against every
//!   request on the human API (see [`scope`]);
//! * the **audit log** next door in [`crate::audit`], which records who did
//!   what to which application, from where, and how it ended.
//!
//! # The two credentials this does not touch
//!
//! `node_preshared_keys` still guards the endpoints that serve runtime nodes,
//! unchanged and still not subject to `api_auth_enabled`: that channel hands
//! out decrypted secrets and answers to machines, not to people, so it has no
//! account and no scope. `storage_auth_secret` still signs the capability
//! tokens that talk to deis-store. Three credentials, three channels, and
//! this module is only about the third one - the people.

pub mod scope;
pub mod store;
pub mod token;

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub use scope::{Demand, Scope, ScopeError, Selector, Verb};
pub use store::{Account, ApiToken, IdentityError, IdentityStore, TokenOrigin};

/// What an account is allowed to hold.
///
/// A role is a ceiling, not a grant: it says which verbs this account's
/// tokens may carry, and it is re-checked on every request rather than only
/// when a token is minted. That is what makes a demotion mean something -
/// moving an account from `owner` to `viewer` reduces every token it already
/// holds to reading, at the next request, without anybody having to hunt
/// those tokens down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The platform. Accounts, tokens, the audit log, node configuration and
    /// node commands, plus everything a maintainer can do. The only role that
    /// may hold [`Verb::Admin`].
    Owner,
    /// Everything about applications: read, deploy, secrets, and minting a
    /// tenant's storage token. Nothing about accounts, tokens or nodes.
    Maintainer,
    /// Ships code and nothing else. The role a build pipeline's account has:
    /// it can upload a version and move a pointer, and it cannot read back or
    /// rotate the secrets the code it ships runs against.
    Deployer,
    /// Looks, changes nothing.
    Viewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Maintainer => "maintainer",
            Role::Deployer => "deployer",
            Role::Viewer => "viewer",
        }
    }

    pub const ALL: [Role; 4] = [Role::Owner, Role::Maintainer, Role::Deployer, Role::Viewer];

    pub fn parse(text: &str) -> Option<Self> {
        Role::ALL.into_iter().find(|role| role.as_str() == text)
    }

    /// The names, for an error message that has to offer them.
    pub fn names() -> String {
        Role::ALL
            .iter()
            .map(|role| role.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Whether a token of this account may carry - and use - this verb.
    pub fn permits(self, verb: Verb) -> bool {
        match self {
            Role::Owner => true,
            Role::Maintainer => verb != Verb::Admin,
            Role::Deployer => matches!(verb, Verb::Read | Verb::Deploy),
            Role::Viewer => verb == Verb::Read,
        }
    }

    /// The scopes a token gets when its creator names none: everything this
    /// role can hold, over every application.
    ///
    /// [`Verb::Storage`] is deliberately absent even for an owner. A storage
    /// scope has to name one tenant (see [`Scope::parse`]), so there is no
    /// wildcard form of it to hand out here, and inventing one for every
    /// existing application would mint a credential that silently stops
    /// covering the next application somebody creates.
    pub fn default_scopes(self) -> Vec<Scope> {
        Verb::ALL
            .iter()
            .filter(|verb| **verb != Verb::Storage && self.permits(**verb))
            .map(|verb| Scope {
                verb: *verb,
                app: scope::Component::Every,
                environment: scope::Component::Every,
            })
            .collect()
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How much a credential may do, before its account's role is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authority {
    /// No scope list at all: whatever the account's role permits, over every
    /// application. This is what an `api_keys` entry from the configuration
    /// file is, and what the bootstrap credential is - the old flat key,
    /// preserved exactly, so an upgrade does not break a deploy pipeline.
    ///
    /// It is also the thing an operator is meant to retire, which is why
    /// every place that lists credentials says out loud which ones are
    /// unscoped.
    Unscoped,
    /// A written-down list. Empty means the token can do nothing at all,
    /// which is a usable state (a token parked before its scopes are
    /// decided) and is not the same as `Unscoped`.
    Scoped(Vec<Scope>),
}

impl Authority {
    pub fn is_unscoped(&self) -> bool {
        matches!(self, Authority::Unscoped)
    }

    /// How this authority is written down, for a list and for a log line.
    pub fn describe(&self) -> String {
        match self {
            Authority::Unscoped => "unscoped".to_string(),
            Authority::Scoped(scopes) if scopes.is_empty() => "none".to_string(),
            Authority::Scoped(scopes) => Scope::join(scopes),
        }
    }
}

/// What kind of thing is calling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// A token minted by this control plane, belonging to an account.
    Account,
    /// An `api_keys` entry from the configuration file. It has a placeholder
    /// account so the audit log has something to name, and no scopes.
    ConfigKey,
    /// Nobody: `api_auth_enabled` is off, so no credential is required, and
    /// this is what a caller who presents none is - directly connected from
    /// the machine the control plane runs on, which is as far as that goes (see
    /// [`crate::api::authz::authorize`]). Recorded in the audit log all the
    /// same: a deploy nobody can be blamed for is still a deploy that
    /// happened.
    Anonymous,
}

impl PrincipalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PrincipalKind::Account => "account",
            PrincipalKind::ConfigKey => "config_key",
            PrincipalKind::Anonymous => "anonymous",
        }
    }
}

/// The caller of one request, resolved once by the authorisation middleware
/// and then carried in the request's extensions.
#[derive(Debug, Clone)]
pub struct Principal {
    pub kind: PrincipalKind,
    /// The account this credential belongs to. `None` only for
    /// [`PrincipalKind::Anonymous`].
    pub account: Option<String>,
    pub role: Option<Role>,
    /// The public half of the token, or the fingerprint of a configured key.
    /// Never the credential itself.
    pub credential_id: Option<String>,
    pub authority: Authority,
}

impl Principal {
    /// The caller nobody authenticated, because nothing was required.
    pub fn anonymous() -> Self {
        Self {
            kind: PrincipalKind::Anonymous,
            account: None,
            role: None,
            credential_id: None,
            authority: Authority::Unscoped,
        }
    }

    /// How a log line and an audit row name this caller. Short, stable, and
    /// carrying no part of any credential.
    pub fn label(&self) -> String {
        match (&self.account, &self.credential_id) {
            (Some(account), Some(credential)) => format!("{account} (token {credential})"),
            (Some(account), None) => account.clone(),
            (None, Some(credential)) => format!("key {credential}"),
            (None, None) => "anonymous".to_string(),
        }
    }

    /// Whether this caller may do what the request asks.
    ///
    /// Two gates, both of which have to open. The role is the account's
    /// ceiling and is applied even to an unscoped credential, so demoting an
    /// account narrows what its existing tokens can do at the very next
    /// request. The scope list is the token's own narrowing, and an unscoped
    /// credential simply has none - which is the old flat key, unchanged.
    pub fn permits(&self, demand: &Demand<'_>) -> Result<(), Denial> {
        if let Some(role) = self.role
            && !role.permits(demand.verb)
        {
            return Err(Denial::Role {
                role,
                verb: demand.verb,
            });
        }
        match &self.authority {
            Authority::Unscoped => Ok(()),
            Authority::Scoped(scopes) => {
                if scopes.iter().any(|scope| scope.grants(demand)) {
                    Ok(())
                } else {
                    Err(Denial::Scope {
                        required: demand.required_scope(),
                        held: Scope::join(scopes),
                    })
                }
            }
        }
    }
}

/// Why a caller who *is* authenticated may not do this.
///
/// Separate from "we do not know who you are" because they are different
/// answers to the caller (403 against 401) and different lines in the audit
/// log: one is a person whose credential works and does not reach far enough,
/// the other is a credential that does not work at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denial {
    /// The account's role does not cover this verb, whatever its tokens say.
    Role { role: Role, verb: Verb },
    /// The token's scope list does not cover this resource.
    Scope { required: String, held: String },
}

impl fmt::Display for Denial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Denial::Role { role, verb } => write!(
                f,
                "this account's role is {role}, which cannot {verb} anything; an owner has to \
                 change the role before any token of this account can"
            ),
            Denial::Scope { required, held } if held.is_empty() => write!(
                f,
                "this token carries no scopes at all, so it can do nothing; it would need \
                 {required}"
            ),
            Denial::Scope { required, held } => write!(
                f,
                "this token carries {held}, which does not cover {required}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scoped(text: &str) -> Principal {
        Principal {
            kind: PrincipalKind::Account,
            account: Some("ci".to_string()),
            role: Some(Role::Deployer),
            credential_id: Some("0011223344556677".to_string()),
            authority: Authority::Scoped(Scope::parse_list(text).unwrap()),
        }
    }

    /// The old flat key, preserved: no scopes means everything, exactly as
    /// `api_keys` always behaved.
    #[test]
    fn an_unscoped_credential_is_the_old_flat_key() {
        let key = Principal {
            kind: PrincipalKind::ConfigKey,
            account: Some("config-key".to_string()),
            role: Some(Role::Owner),
            credential_id: Some("a1b2c3d4".to_string()),
            authority: Authority::Unscoped,
        };
        for verb in Verb::ALL {
            assert!(
                key.permits(&Demand {
                    verb,
                    app: Selector::One("anything"),
                    environment: Selector::One("anywhere"),
                })
                .is_ok(),
                "an api_keys entry must still be able to {verb}"
            );
        }
        assert!(key.authority.is_unscoped());
        assert_eq!(key.label(), "config-key (token a1b2c3d4)");
    }

    /// The role is a ceiling over the token, not a default under it: a
    /// deployer holding a secrets scope still cannot write a secret, so
    /// demoting an account takes effect without revoking anything.
    #[test]
    fn the_role_caps_a_token_that_carries_more() {
        let over_granted = scoped("deploy:myapp/preview secrets:myapp/preview");
        let write_secret = Demand {
            verb: Verb::Secrets,
            app: Selector::One("myapp"),
            environment: Selector::One("preview"),
        };
        assert_eq!(
            over_granted.permits(&write_secret),
            Err(Denial::Role {
                role: Role::Deployer,
                verb: Verb::Secrets
            })
        );
        assert!(
            over_granted
                .permits(&Demand {
                    verb: Verb::Deploy,
                    ..write_secret
                })
                .is_ok()
        );
    }

    /// A refusal has to name the scope that would have worked and the ones
    /// the caller holds; without both, the person on the other end has to
    /// guess which of the two halves is wrong.
    #[test]
    fn a_scope_refusal_names_what_was_needed_and_what_was_held() {
        let ci = scoped("deploy:myapp/preview");
        let denial = ci
            .permits(&Demand {
                verb: Verb::Deploy,
                app: Selector::One("myapp"),
                environment: Selector::One("production"),
            })
            .unwrap_err();
        let message = denial.to_string();
        assert!(message.contains("deploy:myapp/production"), "{message}");
        assert!(message.contains("deploy:myapp/preview"), "{message}");
    }

    /// A role's default scopes have to be exactly what that role can hold,
    /// or a token minted with no `--scope` would be refused by its own
    /// ceiling on its first call.
    #[test]
    fn the_default_scopes_of_a_role_are_within_that_role() {
        for role in Role::ALL {
            let scopes = role.default_scopes();
            assert!(
                !scopes.is_empty(),
                "{role} would get a token that can do nothing"
            );
            for scope in &scopes {
                assert!(
                    role.permits(scope.verb),
                    "{role}'s default scopes include {scope}, which {role} may not hold"
                );
                assert_ne!(
                    scope.verb,
                    Verb::Storage,
                    "a storage scope has to name one tenant, so it is never a default"
                );
            }
        }
        assert_eq!(Scope::join(&Role::Viewer.default_scopes()), "read:*/*");
        assert_eq!(
            Scope::join(&Role::Owner.default_scopes()),
            "read:*/* deploy:*/* secrets:*/* admin:*/*"
        );
    }

    /// An empty scope list is a token that can do nothing. It must not be
    /// mistaken for an unscoped one, which is a token that can do
    /// everything - the two are one `Vec::is_empty()` apart.
    #[test]
    fn an_empty_scope_list_is_not_an_unscoped_credential() {
        let parked = scoped("");
        assert!(!parked.authority.is_unscoped());
        assert_eq!(parked.authority.describe(), "none");
        assert!(
            parked
                .permits(&Demand {
                    verb: Verb::Read,
                    app: Selector::One("myapp"),
                    environment: Selector::One("preview"),
                })
                .is_err()
        );
    }
}
