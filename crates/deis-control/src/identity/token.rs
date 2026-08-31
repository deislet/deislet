//! How a token is shaped, minted, stored and checked.
//!
//! One token string, two halves:
//!
//! ```text
//! deis_pat_<id>_<secret>
//!          ^^^^ ^^^^^^^^
//!          |    32 random bytes as hex. Never stored, never logged, shown
//!          |    once at creation and nowhere else.
//!          8 random bytes as hex. Stored in the clear, printed in every
//!          audit line and in `deis token list`, and useless on its own.
//! ```
//!
//! # Why the database cannot hand out a working credential
//!
//! Only the id and `SHA-256(secret)` are written down. Reading every row of
//! `api_tokens` gives an attacker the hashes and nothing that authenticates:
//! recovering a secret means inverting SHA-256 over a 256-bit uniformly
//! random pre-image, which no amount of database access helps with.
//!
//! That is also why this is a plain digest rather than argon2 or bcrypt. A
//! slow hash exists to make guessing a *human-chosen* password expensive.
//! There is nothing here a person chose: the secret is 32 bytes straight from
//! the system CSPRNG, so the guessing attack a slow hash defends against does
//! not exist, and paying for it would mean one deliberately slow hash on
//! every API call this control plane serves.
//!
//! The id is what makes that affordable. It is the primary key, so verifying
//! a token is one indexed lookup and one comparison - not a scan that hashes
//! every row.

use rand::RngCore;
use sha2::{Digest, Sha256};

/// What every minted token string starts with. It exists so a leaked
/// credential is recognisable for what it is - in a log, in a paste, in a
/// repository scan - rather than looking like any other 80 hex characters.
pub const TOKEN_PREFIX: &str = "deis_pat_";

/// Bytes of randomness in the public half. Eight is plenty: it only has to
/// be unique, and a collision is caught by the primary key.
const ID_BYTES: usize = 8;

/// Bytes of randomness in the secret half. This is the whole strength of the
/// credential, so it is a full 256 bits.
const SECRET_BYTES: usize = 32;

/// A freshly minted token: the part that is stored and the part that is
/// shown once.
///
/// The two are returned together and immediately split by the caller. There
/// is deliberately no field holding the whole string on any type that is
/// stored, serialized into a list response, or logged.
pub struct MintedToken {
    /// The public half. Goes in the row, in `deis token list` and in every
    /// audit line that names this credential.
    pub id: String,
    /// `SHA-256(secret)` as hex. Goes in the row.
    pub secret_hash: String,
    /// The whole token string. This is the only moment it exists anywhere;
    /// the creation response carries it and nothing keeps a copy.
    pub secret: String,
}

/// Mint one token.
pub fn mint() -> MintedToken {
    let mut rng = rand::thread_rng();
    let mut id_bytes = [0u8; ID_BYTES];
    let mut secret_bytes = [0u8; SECRET_BYTES];
    rng.fill_bytes(&mut id_bytes);
    rng.fill_bytes(&mut secret_bytes);

    let id = hex::encode(id_bytes);
    let secret_half = hex::encode(secret_bytes);
    MintedToken {
        secret_hash: hash_secret(&secret_half),
        secret: format!("{TOKEN_PREFIX}{id}_{secret_half}"),
        id,
    }
}

/// The stored form of a token's secret half.
pub fn hash_secret(secret_half: &str) -> String {
    hex::encode(Sha256::digest(secret_half.as_bytes()))
}

/// A short, stable, non-reversible name for a credential this platform did
/// not mint - an `api_keys` entry from the configuration file.
///
/// Four bytes of SHA-256: enough to tell two configured keys apart and to
/// match an audit line against a key an operator still holds, useless to
/// anyone reading the log for a way in. The same reduction the audit line
/// used before there were accounts, kept so old lines and new ones name the
/// same key the same way.
pub fn fingerprint(credential: &str) -> String {
    hex::encode(&Sha256::digest(credential.as_bytes())[..4])
}

/// The two halves of a presented token string, once it has been recognised
/// as one.
pub struct PresentedToken<'a> {
    pub id: &'a str,
    pub secret_half: &'a str,
}

/// Split a presented credential into its halves, or answer `None` because it
/// is not one of this platform's tokens at all.
///
/// `None` is not a failure: an `api_keys` entry from the configuration file
/// is a perfectly good credential and takes the other path in
/// [`super::store::IdentityStore::resolve`]. What this function must never do
/// is accept a malformed string as a token with an empty secret, which is why
/// both halves are length-checked here rather than at the comparison.
pub fn split(presented: &str) -> Option<PresentedToken<'_>> {
    let rest = presented.strip_prefix(TOKEN_PREFIX)?;
    let (id, secret_half) = rest.split_once('_')?;
    if id.len() != ID_BYTES * 2 || secret_half.len() != SECRET_BYTES * 2 {
        return None;
    }
    if !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(PresentedToken { id, secret_half })
}

/// Whether a presented secret half is the one behind a stored hash.
///
/// Compared in time that does not depend on how many leading bytes matched.
/// Both sides are hex digests of a fixed length, so the only thing a timing
/// difference could leak is the digest - but the digest is what an attacker
/// who has the database already has, and spelling the comparison this way
/// costs nothing and removes the question.
pub fn secret_matches(stored_hash: &str, presented_secret_half: &str) -> bool {
    crate::api::middleware::secret_eq(stored_hash, &hash_secret(presented_secret_half))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the rest of the system relies on: recognisable prefix, an
    /// id that is stored, a secret that is not, and a string that splits
    /// back into exactly what was minted.
    #[test]
    fn a_minted_token_splits_back_into_the_halves_that_were_stored() {
        let minted = mint();
        assert!(minted.secret.starts_with(TOKEN_PREFIX));

        let presented = split(&minted.secret).expect("a minted token must be recognised as one");
        assert_eq!(presented.id, minted.id);
        assert!(secret_matches(&minted.secret_hash, presented.secret_half));

        // The stored half is not the credential.
        assert!(
            !minted.secret.contains(&minted.secret_hash),
            "the hash that is stored must not appear inside the token that is handed out"
        );
        assert_ne!(minted.secret_hash, presented.secret_half);
    }

    /// Two mints share nothing. A collision in the id would be caught by the
    /// primary key; a collision in the secret would be a credential that
    /// authenticates as somebody else.
    #[test]
    fn two_tokens_share_neither_half() {
        let a = mint();
        let b = mint();
        assert_ne!(a.id, b.id);
        assert_ne!(a.secret, b.secret);
        assert_ne!(a.secret_hash, b.secret_hash);
    }

    /// A credential that is not one of ours has to be recognised as such
    /// rather than parsed into a token with an empty or short secret, which
    /// is what would let a caller present `deis_pat_x_` and be compared
    /// against nothing.
    #[test]
    fn a_credential_that_is_not_a_token_is_not_parsed_as_one() {
        for text in [
            "",
            "some-api-key-from-the-config-file",
            "deis_pat_",
            "deis_pat_0011223344556677",
            "deis_pat_0011223344556677_",
            "deis_pat_0011223344556677_tooshort",
            "deis_pat_nothexadecimal0_0000000000000000000000000000000000000000000000000000000000000000",
        ] {
            assert!(split(text).is_none(), "{text:?} was parsed as a token");
        }
    }

    /// The wrong secret with the right id is refused; so is the right secret
    /// against another token's hash.
    #[test]
    fn only_the_secret_that_was_minted_matches_its_hash() {
        let a = mint();
        let b = mint();
        let a_half = split(&a.secret).unwrap().secret_half;
        let b_half = split(&b.secret).unwrap().secret_half;

        assert!(secret_matches(&a.secret_hash, a_half));
        assert!(!secret_matches(&a.secret_hash, b_half));
        assert!(!secret_matches(&a.secret_hash, ""));
    }

    /// The fingerprint of a configured key is short, stable and not the key.
    #[test]
    fn a_configured_key_is_named_by_a_fingerprint_that_is_not_the_key() {
        let key = "an-api-keys-entry";
        let fp = fingerprint(key);
        assert_eq!(fp.len(), 8);
        assert_eq!(fp, fingerprint(key));
        assert_ne!(fp, fingerprint("another-api-keys-entry"));
        assert!(!key.contains(&fp));
    }
}
