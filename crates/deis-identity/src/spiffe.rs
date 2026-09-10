//! `spiffe://<trust domain>/<path>` - the one name a machine answers to.
//!
//! # Why this refuses spellings instead of normalising them
//!
//! Two spellings that mean the same principal are an alias, and an alias is a
//! way to be authorised as somebody else. A table keyed by
//! `spiffe://deis.internal/node/a` is not consulted at all when the caller
//! arrives as `spiffe://deis.internal/node/./a`, and a table that normalises
//! before it looks up has to normalise the same way in every place that reads
//! it or the two halves quietly disagree about who is who.
//!
//! So nothing here normalises. Exactly one spelling of an identity is
//! accepted, everything else is refused at parse time with a reason, and
//! comparison afterwards is a byte comparison. That makes the parse the only
//! place the rule lives, and it makes "these two are the same principal" a
//! statement nobody has to qualify.
//!
//! The path is case-sensitive and the trust domain is not, which is what
//! SPIFFE specifies. There is no aliasing in that: an upper-case letter in the
//! trust domain is refused rather than folded, so no two accepted spellings
//! ever differ only by case.

use std::fmt;

/// The longest identity this accepts, in bytes; the limit SPIFFE states.
///
/// Not a performance guard. It is what stops an identity from being a place to
/// carry a payload into an audit record, a log line or a routing table.
const MAX_LEN: usize = 2048;

/// The longest trust domain this accepts, in bytes; also from SPIFFE.
const MAX_TRUST_DOMAIN_LEN: usize = 255;

/// The scheme, lower-case, with its separator. An identity that opens with any
/// other capitalisation is refused rather than folded - see the module note.
const SCHEME: &str = "spiffe://";

/// A SPIFFE identity that has been checked, and so can be compared by bytes.
///
/// There is no way to build one of these without going through [`parse`], so a
/// value of this type is a promise that the rules in this module held.
///
/// [`parse`]: SpiffeId::parse
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpiffeId {
    text: String,
    /// Where the trust domain ends and the path begins, as a byte offset into
    /// `text`. The path always starts with `/`, so this is also the offset of
    /// that slash.
    path_at: usize,
}

impl SpiffeId {
    /// Read an identity, or say which rule it broke.
    pub fn parse(text: &str) -> Result<Self, SpiffeIdError> {
        if text.len() > MAX_LEN {
            return Err(SpiffeIdError::TooLong {
                length: text.len(),
                limit: MAX_LEN,
            });
        }
        let Some(rest) = text.strip_prefix(SCHEME) else {
            return Err(SpiffeIdError::NotSpiffe {
                text: text.to_string(),
            });
        };

        let path_at = SCHEME.len()
            + match rest.find('/') {
                Some(offset) => offset,
                None => {
                    return Err(SpiffeIdError::NoPath {
                        text: text.to_string(),
                    });
                }
            };
        let trust_domain = &text[SCHEME.len()..path_at];
        let path = &text[path_at..];

        check_trust_domain(trust_domain)?;
        check_path(path)?;

        Ok(Self {
            text: text.to_string(),
            path_at,
        })
    }

    /// The whole identity, exactly as it was written.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The trust domain: everything between `spiffe://` and the first `/`.
    ///
    /// Two identities in different trust domains are two strangers, however
    /// alike their paths look.
    pub fn trust_domain(&self) -> &str {
        &self.text[SCHEME.len()..self.path_at]
    }

    /// The path, leading `/` included.
    pub fn path(&self) -> &str {
        &self.text[self.path_at..]
    }

    /// Whether both identities were issued by the same trust domain.
    pub fn shares_trust_domain_with(&self, other: &Self) -> bool {
        self.trust_domain() == other.trust_domain()
    }
}

impl fmt::Display for SpiffeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::str::FromStr for SpiffeId {
    type Err = SpiffeIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// Trust domain characters, per SPIFFE: lower-case letters, digits, and the
/// three punctuation marks below. An upper-case letter, a `%`, a `:` or an `@`
/// is refused here rather than anywhere later.
fn check_trust_domain(trust_domain: &str) -> Result<(), SpiffeIdError> {
    if trust_domain.is_empty() {
        return Err(SpiffeIdError::EmptyTrustDomain);
    }
    if trust_domain.len() > MAX_TRUST_DOMAIN_LEN {
        return Err(SpiffeIdError::TrustDomainTooLong {
            length: trust_domain.len(),
            limit: MAX_TRUST_DOMAIN_LEN,
        });
    }
    for character in trust_domain.chars() {
        let allowed = character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '-' | '_');
        if !allowed {
            return Err(SpiffeIdError::TrustDomainCharacter {
                character,
                trust_domain: trust_domain.to_string(),
            });
        }
    }
    Ok(())
}

/// Path characters, per SPIFFE: letters of either case, digits, and the same
/// three marks. Empty segments and the two relative segments are refused,
/// because `/node//a`, `/node/a/` and `/node/b/../a` would otherwise be three
/// more ways to write one principal.
fn check_path(path: &str) -> Result<(), SpiffeIdError> {
    debug_assert!(path.starts_with('/'));
    let mut segments = 0usize;
    for segment in path[1..].split('/') {
        segments += 1;
        if segment.is_empty() {
            return Err(SpiffeIdError::EmptySegment {
                path: path.to_string(),
            });
        }
        if segment == "." || segment == ".." {
            return Err(SpiffeIdError::RelativeSegment {
                segment: segment.to_string(),
                path: path.to_string(),
            });
        }
        for character in segment.chars() {
            let allowed = character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_');
            if !allowed {
                return Err(SpiffeIdError::PathCharacter {
                    character,
                    path: path.to_string(),
                });
            }
        }
    }
    debug_assert!(segments > 0, "a path starting with / has one segment");
    let _ = segments;
    Ok(())
}

/// Why an identity was not accepted.
///
/// Every variant names the text it refused. A machine that cannot authenticate
/// is an outage, and an operator reading the log needs to see which of the two
/// ends wrote the identity wrong, not that "authentication failed".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpiffeIdError {
    #[error("a SPIFFE identity may be at most {limit} bytes, and this one is {length}")]
    TooLong { length: usize, limit: usize },

    #[error(
        "a SPIFFE identity starts with the lower-case scheme spiffe://, and this one does not: {text:?}"
    )]
    NotSpiffe { text: String },

    #[error(
        "a SPIFFE identity names a workload with a path after the trust domain, and this one is a bare trust domain: {text:?}"
    )]
    NoPath { text: String },

    #[error(
        "a SPIFFE identity has a trust domain between spiffe:// and the first /, and this one is empty"
    )]
    EmptyTrustDomain,

    #[error("a SPIFFE trust domain may be at most {limit} bytes, and this one is {length}")]
    TrustDomainTooLong { length: usize, limit: usize },

    #[error(
        "a SPIFFE trust domain holds lower-case letters, digits, '.', '-' and '_', and {trust_domain:?} holds {character:?}"
    )]
    TrustDomainCharacter {
        character: char,
        trust_domain: String,
    },

    #[error(
        "a SPIFFE path holds letters, digits, '.', '-' and '_' between its slashes, and {path:?} holds {character:?}"
    )]
    PathCharacter { character: char, path: String },

    #[error(
        "a SPIFFE path has no empty segment, so it neither ends in / nor repeats one, and {path:?} does"
    )]
    EmptySegment { path: String },

    #[error(
        "a SPIFFE path spells the principal it names outright, so {segment:?} is not a segment it may hold: {path:?}"
    )]
    RelativeSegment { segment: String, path: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_identity_keeps_its_two_halves() {
        let id = SpiffeId::parse("spiffe://deis.internal/machine/runtime/01").expect("valid");
        assert_eq!(id.trust_domain(), "deis.internal");
        assert_eq!(id.path(), "/machine/runtime/01");
        assert_eq!(id.as_str(), "spiffe://deis.internal/machine/runtime/01");
        assert_eq!(id.to_string(), "spiffe://deis.internal/machine/runtime/01");
    }

    /// The whole point of the module: no two accepted spellings name one
    /// principal. Each of these is a spelling somebody could reasonably expect
    /// to work, and each is refused rather than folded into the first one.
    #[test]
    fn every_other_spelling_of_one_principal_is_refused() {
        let refused = [
            "SPIFFE://deis.internal/machine/a",
            "spiffe://DEIS.INTERNAL/machine/a",
            "spiffe://deis.internal/machine//a",
            "spiffe://deis.internal/machine/a/",
            "spiffe://deis.internal/machine/./a",
            "spiffe://deis.internal/machine/b/../a",
            "spiffe://deis.internal/machine%2Fa",
            "spiffe://deis.internal:8443/machine/a",
            "spiffe://user@deis.internal/machine/a",
            "spiffe://deis.internal/machine/a?role=node",
            "spiffe://deis.internal/machine/a#node",
        ];
        for text in refused {
            assert!(
                SpiffeId::parse(text).is_err(),
                "{text} was accepted, so it is a second spelling of one principal"
            );
        }
    }

    #[test]
    fn a_bare_trust_domain_does_not_name_a_workload() {
        assert_eq!(
            SpiffeId::parse("spiffe://deis.internal"),
            Err(SpiffeIdError::NoPath {
                text: "spiffe://deis.internal".to_string()
            })
        );
    }

    #[test]
    fn an_empty_trust_domain_is_refused() {
        assert_eq!(
            SpiffeId::parse("spiffe:///machine/a"),
            Err(SpiffeIdError::EmptyTrustDomain)
        );
    }

    #[test]
    fn the_limits_are_stated_in_the_error_rather_than_implied() {
        let long = format!("spiffe://deis.internal/{}", "a".repeat(MAX_LEN));
        match SpiffeId::parse(&long) {
            Err(SpiffeIdError::TooLong { length, limit }) => {
                assert_eq!(limit, MAX_LEN);
                assert_eq!(length, long.len());
            }
            other => panic!("expected a length refusal, got {other:?}"),
        }
    }

    #[test]
    fn two_trust_domains_are_two_strangers() {
        let ours = SpiffeId::parse("spiffe://deis.internal/machine/a").expect("valid");
        let theirs = SpiffeId::parse("spiffe://other.internal/machine/a").expect("valid");
        assert_ne!(ours, theirs);
        assert!(!ours.shares_trust_domain_with(&theirs));

        let sibling = SpiffeId::parse("spiffe://deis.internal/machine/b").expect("valid");
        assert!(ours.shares_trust_domain_with(&sibling));
    }

    #[test]
    fn a_path_keeps_its_case_because_that_makes_no_alias() {
        let upper = SpiffeId::parse("spiffe://deis.internal/machine/A").expect("valid");
        let lower = SpiffeId::parse("spiffe://deis.internal/machine/a").expect("valid");
        assert_ne!(upper, lower);
        assert_eq!(upper.path(), "/machine/A");
    }
}
