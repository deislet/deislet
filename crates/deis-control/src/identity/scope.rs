//! The scope grammar: what one credential is allowed to do, and to what.
//!
//! A scope is three things joined by two separators, and nothing else:
//!
//! ```text
//! <verb>:<application>/<environment>
//!
//! deploy:myapp/preview     may ship code into one environment of one app
//! read:*/*                 may look at everything and change nothing
//! secrets:myapp/production may write and delete that environment's secrets
//! storage:myapp/production may mint the credential that acts as that tenant
//! admin:*/*                may manage accounts, tokens, nodes and the audit log
//! ```
//!
//! `*` stands for a whole component and never for part of one: there is no
//! `my*` and no `myapp/pre*`. Prefix matching would mean an operator has to
//! read a glob to know what a token can reach, and the first person to write
//! `read:*` meaning "everything" would instead have written a scope that
//! matches nothing.
//!
//! # Why the verbs are these five
//!
//! [`Verb::Deploy`] and [`Verb::Secrets`] are separate because the credential
//! a build pipeline holds should be able to ship code without being able to
//! rotate the database password it ships against. [`Verb::Storage`] is
//! separate from both, and implied by neither, because the thing it mints
//! outlives every check this process makes: see [`Scope::grants`].
//! [`Verb::Admin`] is on a different axis from the other four - it is about
//! the platform rather than about an application - so it is only ever written
//! `admin:*/*`, and the parser refuses anything narrower rather than
//! accepting a scope that would read as "administrator of one app" and mean
//! nothing.
//!
//! Every verb lets its holder *look at* what it acts on, so a scope list
//! never has to repeat itself: `deploy:myapp/preview` implies
//! `read:myapp/preview`. Nothing else is implied in either direction.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Longest scope string this will parse. Far above anything meaningful; it
/// exists so a caller cannot make the parser walk a megabyte.
const MAX_SCOPE_LEN: usize = 256;

/// What a credential may do to whatever its scope names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    /// Look, and change nothing: list applications, versions, environments,
    /// deployments, routes and the *names* of secrets. There is no endpoint
    /// anywhere that returns a secret value to this surface, so `read` can
    /// never reach one.
    Read,
    /// Upload a version and move an environment's pointer - deploy, roll
    /// back, create the environment, change its routes.
    Deploy,
    /// Write and delete an environment's secrets. Separate from `deploy` so
    /// a build pipeline's credential cannot rotate what it deploys against.
    Secrets,
    /// Mint the capability token that talks to deis-store directly as one
    /// tenant. Its own verb, and implied by nothing.
    Storage,
    /// The platform rather than an application: accounts, tokens, the audit
    /// log, node configuration and node commands, and deleting an
    /// application outright.
    Admin,
}

impl Verb {
    pub fn as_str(self) -> &'static str {
        match self {
            Verb::Read => "read",
            Verb::Deploy => "deploy",
            Verb::Secrets => "secrets",
            Verb::Storage => "storage",
            Verb::Admin => "admin",
        }
    }

    /// Every verb, so a role's ceiling and an error message can list them
    /// without a second copy of this set going stale.
    pub const ALL: [Verb; 5] = [
        Verb::Read,
        Verb::Deploy,
        Verb::Secrets,
        Verb::Storage,
        Verb::Admin,
    ];

    fn parse(text: &str) -> Option<Self> {
        Verb::ALL.into_iter().find(|verb| verb.as_str() == text)
    }
}

impl fmt::Display for Verb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One component of a scope's resource: a name, or every name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Component {
    /// `*`. Matches whatever the request names, and is the only thing that
    /// can answer a request about the platform as a whole.
    Every,
    /// One literal name.
    Named(String),
}

impl Component {
    fn parse(text: &str) -> Result<Self, ScopeError> {
        if text == "*" {
            return Ok(Component::Every);
        }
        if text.is_empty() {
            return Err(ScopeError("a scope component may not be empty".to_string()));
        }
        if !text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(ScopeError(format!(
                "{text:?} is not a name this platform can have: application and environment names \
                 are lowercase letters, digits, '-' and '_', and '*' means all of them"
            )));
        }
        Ok(Component::Named(text.to_string()))
    }

    fn is_every(&self) -> bool {
        matches!(self, Component::Every)
    }
}

impl fmt::Display for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Component::Every => f.write_str("*"),
            Component::Named(name) => f.write_str(name),
        }
    }
}

/// Which application or environment a request is about.
///
/// The three cases are genuinely different questions and collapsing any two
/// of them is how a scope check ends up either too tight to be usable or too
/// loose to be a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector<'a> {
    /// This one, by name. A scope answers it by naming the same one or by
    /// naming all of them.
    One(&'a str),
    /// All of them at once - listing every node, reading the platform's
    /// audit log. Only a `*` answers this; a scope naming one application
    /// must not be able to ask a question about all of them.
    Every,
    /// The request is about the application as a whole rather than about one
    /// of its environments: uploading a version puts the code nowhere. Any
    /// environment the holder can reach is enough, because whoever may
    /// deploy into one environment may already upload the bundle it needs.
    Any,
}

impl Selector<'_> {
    fn answered_by(self, component: &Component) -> bool {
        match (self, component) {
            (Selector::Any, _) => true,
            (Selector::Every, component) => component.is_every(),
            (Selector::One(_), Component::Every) => true,
            (Selector::One(name), Component::Named(scoped)) => name == scoped,
        }
    }

    /// How an audit line and a refusal name what was asked for.
    pub fn describe(self) -> String {
        match self {
            Selector::One(name) => name.to_string(),
            Selector::Every => "*".to_string(),
            Selector::Any => "(any)".to_string(),
        }
    }
}

/// What one request needs before it may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Demand<'a> {
    pub verb: Verb,
    pub app: Selector<'a>,
    pub environment: Selector<'a>,
}

impl Demand<'_> {
    /// The scope string that would satisfy this demand exactly, so a refusal
    /// can tell the caller what to ask their operator for instead of leaving
    /// them to guess the grammar.
    pub fn required_scope(&self) -> String {
        format!(
            "{}:{}/{}",
            self.verb,
            self.app.describe(),
            self.environment.describe()
        )
    }
}

impl fmt::Display for Demand<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.required_scope())
    }
}

/// One grant: a verb over one resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Scope {
    pub verb: Verb,
    pub app: Component,
    pub environment: Component,
}

/// Why a scope string is not a scope. Carries the whole sentence, because
/// the person reading it is looking at a CLI error and has no other copy of
/// the grammar in front of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeError(pub String);

impl fmt::Display for ScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScopeError {}

impl Scope {
    /// Parse one scope string.
    ///
    /// Two shapes are refused here rather than at a call site, so there is no
    /// way to hold either of them:
    ///
    /// * `admin` narrower than `*/*`. Everything `admin` covers is about the
    ///   platform, so `admin:myapp/*` would name a thing that does not exist.
    /// * `storage` with a `*` in either half. That verb mints a credential
    ///   which talks to deis-store without passing through this process again
    ///   and never expires, so a blanket grant of it would be a single string
    ///   that hands over every tenant's data for good. Each tenant has to be
    ///   written down.
    pub fn parse(text: &str) -> Result<Self, ScopeError> {
        let text = text.trim();
        if text.len() > MAX_SCOPE_LEN {
            return Err(ScopeError(format!(
                "a scope is at most {MAX_SCOPE_LEN} characters; this one is {}",
                text.len()
            )));
        }
        let Some((verb, resource)) = text.split_once(':') else {
            return Err(ScopeError(format!(
                "{text:?} is not a scope: a scope is written <verb>:<app>/<environment>, for \
                 instance deploy:myapp/preview or read:*/*"
            )));
        };
        let Some(verb) = Verb::parse(verb) else {
            return Err(ScopeError(format!(
                "{verb:?} is not a verb; the verbs are {}",
                Verb::ALL
                    .iter()
                    .map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        let Some((app, environment)) = resource.split_once('/') else {
            return Err(ScopeError(format!(
                "{text:?} names no environment: a scope is written \
                 <verb>:<app>/<environment>, and \"{verb}:{resource}/*\" is what covers every \
                 environment of that application"
            )));
        };
        let scope = Scope {
            verb,
            app: Component::parse(app)?,
            environment: Component::parse(environment)?,
        };

        if verb == Verb::Admin && !(scope.app.is_every() && scope.environment.is_every()) {
            return Err(ScopeError(format!(
                "{text:?} does not exist: admin covers accounts, tokens, the audit log and the \
                 nodes, none of which belong to one application, so the only admin scope is \
                 admin:*/*"
            )));
        }
        if verb == Verb::Storage && (scope.app.is_every() || scope.environment.is_every()) {
            return Err(ScopeError(format!(
                "{text:?} is refused: a storage scope hands out a token that reads and writes one \
                 tenant's data directly against deis-store, without passing through this control \
                 plane again, and never expires. It has to name one application and one \
                 environment, for instance storage:myapp/production"
            )));
        }
        Ok(scope)
    }

    /// Parse a whole scope list as it is stored and as a caller writes it:
    /// scopes separated by whitespace or commas. An empty list is an empty
    /// `Vec`, which is *not* the same as an unscoped credential - see
    /// [`super::Authority`].
    pub fn parse_list(text: &str) -> Result<Vec<Scope>, ScopeError> {
        text.split([' ', ',', '\t', '\n'])
            .filter(|piece| !piece.is_empty())
            .map(Scope::parse)
            .collect()
    }

    /// Render a scope list the way it is stored: one line, space separated,
    /// round-tripping through [`Scope::parse_list`].
    pub fn join(scopes: &[Scope]) -> String {
        scopes
            .iter()
            .map(|scope| scope.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Whether this scope answers that demand.
    ///
    /// The verb rule is the whole of the implication in this model: a verb
    /// answers itself, and every verb answers [`Verb::Read`] on the same
    /// resource, because a credential that may change a thing may obviously
    /// look at it. Nothing else is implied - in particular `admin` does not
    /// answer `deploy`, so an operator who wants to ship code grants
    /// themselves `deploy:*/*` and that grant is visible in `deis token
    /// list`, and `deploy` does not answer `storage`, which is the narrowing
    /// this model exists for.
    pub fn grants(&self, demand: &Demand<'_>) -> bool {
        let verb_matches = self.verb == demand.verb || demand.verb == Verb::Read;
        verb_matches
            && demand.app.answered_by(&self.app)
            && demand.environment.answered_by(&self.environment)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}/{}", self.verb, self.app, self.environment)
    }
}

impl Serialize for Scope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Scope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Scope::parse(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(text: &str) -> Scope {
        Scope::parse(text).unwrap_or_else(|e| panic!("{text} should parse: {e}"))
    }

    #[test]
    fn a_scope_round_trips_through_its_own_text() {
        for text in [
            "read:*/*",
            "deploy:myapp/preview",
            "secrets:myapp/production",
            "storage:myapp/production",
            "admin:*/*",
            "deploy:myapp/*",
        ] {
            assert_eq!(scope(text).to_string(), text);
        }
        let list = Scope::parse_list("deploy:myapp/preview read:*/*").unwrap();
        assert_eq!(Scope::join(&list), "deploy:myapp/preview read:*/*");
        assert_eq!(
            Scope::parse_list("deploy:myapp/preview, read:*/*").unwrap(),
            list,
            "a comma separated list is the same list"
        );
    }

    /// The headline case from the specification: this token may deploy to
    /// one environment of one application and read nothing else.
    #[test]
    fn a_ci_scope_reaches_one_environment_and_no_other() {
        let ci = scope("deploy:myapp/preview");

        assert!(ci.grants(&Demand {
            verb: Verb::Deploy,
            app: Selector::One("myapp"),
            environment: Selector::One("preview"),
        }));
        // The bundle upload is about the application, not about one of its
        // environments, and this holder can deploy into one of them.
        assert!(ci.grants(&Demand {
            verb: Verb::Deploy,
            app: Selector::One("myapp"),
            environment: Selector::Any,
        }));
        // Everything else is refused: the other environment, another
        // application, a stronger verb, and any platform-wide question.
        for refused in [
            Demand {
                verb: Verb::Deploy,
                app: Selector::One("myapp"),
                environment: Selector::One("production"),
            },
            Demand {
                verb: Verb::Read,
                app: Selector::One("otherapp"),
                environment: Selector::One("preview"),
            },
            Demand {
                verb: Verb::Secrets,
                app: Selector::One("myapp"),
                environment: Selector::One("preview"),
            },
            Demand {
                verb: Verb::Storage,
                app: Selector::One("myapp"),
                environment: Selector::One("preview"),
            },
            Demand {
                verb: Verb::Read,
                app: Selector::Every,
                environment: Selector::Every,
            },
        ] {
            assert!(
                !ci.grants(&refused),
                "deploy:myapp/preview must not answer {refused}"
            );
        }
    }

    /// A scope naming one application must not be able to answer a question
    /// about all of them, or `read:myapp/production` would list every node
    /// and every other application's routes.
    #[test]
    fn one_application_cannot_answer_a_platform_question() {
        let narrow = scope("read:myapp/production");
        let wide = scope("read:*/*");
        let platform = Demand {
            verb: Verb::Read,
            app: Selector::Every,
            environment: Selector::Every,
        };
        assert!(!narrow.grants(&platform));
        assert!(wide.grants(&platform));
    }

    /// Every verb lets its holder look at what it acts on; nothing else is
    /// implied, in particular not admin over deploy.
    #[test]
    fn a_verb_implies_reading_what_it_acts_on_and_nothing_more() {
        let secrets = scope("secrets:myapp/production");
        assert!(secrets.grants(&Demand {
            verb: Verb::Read,
            app: Selector::One("myapp"),
            environment: Selector::One("production"),
        }));

        let admin = scope("admin:*/*");
        assert!(
            admin.grants(&Demand {
                verb: Verb::Read,
                app: Selector::One("myapp"),
                environment: Selector::One("production"),
            }),
            "an administrator can look at what they administer"
        );
        assert!(
            !admin.grants(&Demand {
                verb: Verb::Deploy,
                app: Selector::One("myapp"),
                environment: Selector::One("production"),
            }),
            "shipping code is a grant an operator writes down, not one that arrives with admin"
        );
    }

    /// The two shapes that must not exist at all, refused where they are
    /// written rather than where they would be checked.
    #[test]
    fn admin_is_platform_wide_and_storage_is_never_blanket() {
        for refused in ["admin:myapp/*", "admin:*/production", "admin:myapp/preview"] {
            let error = Scope::parse(refused).unwrap_err().to_string();
            assert!(error.contains("admin:*/*"), "{refused}: {error}");
        }
        for refused in ["storage:*/*", "storage:myapp/*", "storage:*/production"] {
            let error = Scope::parse(refused).unwrap_err().to_string();
            assert!(error.contains("never expires"), "{refused}: {error}");
        }
    }

    #[test]
    fn a_malformed_scope_says_what_the_grammar_is() {
        for (text, expected) in [
            ("deploy", "<verb>:<app>/<environment>"),
            ("deploy:myapp", "names no environment"),
            ("wibble:*/*", "is not a verb"),
            ("read:My App/*", "not a name"),
            ("read:/*", "may not be empty"),
        ] {
            let error = Scope::parse(text).unwrap_err().to_string();
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    /// A refusal has to name the scope that would have worked, because the
    /// person reading it is holding a token and cannot see this file.
    #[test]
    fn a_demand_names_the_scope_that_would_answer_it() {
        assert_eq!(
            Demand {
                verb: Verb::Storage,
                app: Selector::One("demo"),
                environment: Selector::One("production"),
            }
            .required_scope(),
            "storage:demo/production"
        );
    }
}
