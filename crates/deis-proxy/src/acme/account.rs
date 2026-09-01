//! The ACME account key: the identity every request to the authority is
//! signed with, and the identity a certificate is issued to.
//!
//! One key, kept for the life of the installation. Generating a fresh one on
//! each start would register a fresh account each time, and Let's Encrypt
//! counts new registrations per IP address per three hours - an instance
//! that restarted a few times an hour would lock itself out of issuing
//! anything at all, with nothing in its own logs to explain why.
//!
//! `ES256` (ECDSA over P-256 with SHA-256) rather than RSA: every ACME
//! authority is required to accept it, the key is 32 bytes rather than
//! 2048 bits, and generating one does not stall the startup path for the
//! best part of a second.

use openssl::bn::BigNumContext;
use openssl::ec::{EcGroup, EcKey, PointConversionForm};
use openssl::ecdsa::EcdsaSig;
use openssl::hash::{MessageDigest, hash};
use openssl::nid::Nid;
use openssl::pkey::Private;
use serde_json::{Value as JsonValue, json};

use super::error::AcmeError;
use super::jose::base64url;

/// Bytes in one P-256 coordinate, and therefore in each half of an ES256
/// signature. Both are fixed-width big-endian and must be padded to it: a
/// coordinate that happened to start with a zero byte would otherwise be
/// serialised one byte short, and the authority would reject the signature
/// roughly one time in 256 - the kind of failure that passes every test and
/// then happens in production a fortnight later.
const P256_COORDINATE_BYTES: usize = 32;

/// The account key pair.
pub struct AccountKey {
    key: EcKey<Private>,
}

impl AccountKey {
    pub fn generate() -> Result<Self, AcmeError> {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)
            .map_err(|e| AcmeError::Crypto(format!("selecting the P-256 group: {e}")))?;
        let key = EcKey::generate(&group)
            .map_err(|e| AcmeError::Crypto(format!("generating the account key: {e}")))?;
        Self::from_ec_key(key)
    }

    /// Read a key written by [`AccountKey::to_pem`].
    pub fn from_pem(pem: &[u8]) -> Result<Self, AcmeError> {
        let key = EcKey::private_key_from_pem(pem).map_err(|e| {
            AcmeError::Crypto(format!(
                "reading the stored account key: {e}. It is the identity the authority knows this \
                 installation by; deleting it and letting a new one be generated is the recovery, \
                 at the cost of a fresh registration"
            ))
        })?;
        Self::from_ec_key(key)
    }

    fn from_ec_key(key: EcKey<Private>) -> Result<Self, AcmeError> {
        Ok(Self { key })
    }

    pub fn to_pem(&self) -> Result<Vec<u8>, AcmeError> {
        self.key
            .private_key_to_pem()
            .map_err(|e| AcmeError::Crypto(format!("serialising the account key: {e}")))
    }

    /// The public key as a JWK, as it goes into a `newAccount` request.
    pub fn jwk_value(&self) -> JsonValue {
        let (x, y) = self.coordinates();
        json!({
            "crv": "P-256",
            "kty": "EC",
            "x": base64url(&x),
            "y": base64url(&y),
        })
    }

    /// The account key thumbprint (RFC 7638), which is the second half of
    /// every `http-01` key authorization.
    ///
    /// The JSON is built by hand rather than serialised, because the
    /// thumbprint is the hash of an exactly specified string: the required
    /// members only, lexicographic by name, no whitespace. A serialiser that
    /// reordered or pretty-printed would produce a different hash, the
    /// authority would fetch the challenge and find the wrong answer, and
    /// the only symptom would be every validation failing.
    pub fn thumbprint(&self) -> Result<String, AcmeError> {
        let (x, y) = self.coordinates();
        let canonical = format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
            base64url(&x),
            base64url(&y)
        );
        let digest = hash(MessageDigest::sha256(), canonical.as_bytes())
            .map_err(|e| AcmeError::Crypto(format!("hashing the account key thumbprint: {e}")))?;
        Ok(base64url(&digest))
    }

    /// What a challenge file has to contain: the token, a dot, and the
    /// thumbprint (RFC 8555 section 8.1).
    pub fn key_authorization(&self, token: &str) -> Result<String, AcmeError> {
        Ok(format!("{token}.{}", self.thumbprint()?))
    }

    /// Sign the JWS signing input, returning the 64 raw bytes `ES256` wants
    /// rather than the DER structure OpenSSL produces.
    pub fn sign(&self, signing_input: &[u8]) -> Result<Vec<u8>, AcmeError> {
        let digest = hash(MessageDigest::sha256(), signing_input)
            .map_err(|e| AcmeError::Crypto(format!("hashing the signing input: {e}")))?;
        let signature = EcdsaSig::sign(&digest, &self.key)
            .map_err(|e| AcmeError::Crypto(format!("signing with the account key: {e}")))?;
        let r = signature
            .r()
            .to_vec_padded(P256_COORDINATE_BYTES as i32)
            .map_err(|e| AcmeError::Crypto(format!("encoding the signature r value: {e}")))?;
        let s = signature
            .s()
            .to_vec_padded(P256_COORDINATE_BYTES as i32)
            .map_err(|e| AcmeError::Crypto(format!("encoding the signature s value: {e}")))?;
        let mut raw = r;
        raw.extend_from_slice(&s);
        Ok(raw)
    }

    /// The public point split into its two fixed-width coordinates.
    ///
    /// An unreadable key would mean a JWK that names no key, which the
    /// authority refuses; there is nothing useful to do about it here, and
    /// the empty coordinates make the refusal happen at the authority with
    /// its own message rather than turning every caller into an error path.
    fn coordinates(&self) -> (Vec<u8>, Vec<u8>) {
        let mut context = match BigNumContext::new() {
            Ok(context) => context,
            Err(_) => return (Vec::new(), Vec::new()),
        };
        let bytes = match self.key.public_key().to_bytes(
            self.key.group(),
            PointConversionForm::UNCOMPRESSED,
            &mut context,
        ) {
            Ok(bytes) => bytes,
            Err(_) => return (Vec::new(), Vec::new()),
        };
        // Uncompressed form is 0x04 then X then Y, both fixed width.
        if bytes.len() != 1 + 2 * P256_COORDINATE_BYTES {
            return (Vec::new(), Vec::new());
        }
        (
            bytes[1..1 + P256_COORDINATE_BYTES].to_vec(),
            bytes[1 + P256_COORDINATE_BYTES..].to_vec(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed P-256 key, so the thumbprint below is a value rather than
    /// whatever this run happened to generate. Generated once for this test
    /// and used nowhere else.
    const FIXED_KEY_PEM: &[u8] = b"-----BEGIN EC PRIVATE KEY-----\n\
MHcCAQEEIHoRaGaZEjBLKYXdW7F2fY6yLygA4qkQFKujc8VE6UUvoAoGCCqGSM49\n\
AwEHoUQDQgAEJXn/UuNyL/brKebI4yTsC+98WHdumH0y0bB5k9fyqBs7ZoeW1wZ+\n\
bsQYFKFc9dTQfotPIKmUq1inaC4klQ7RQQ==\n\
-----END EC PRIVATE KEY-----\n";

    /// The thumbprint of [`FIXED_KEY_PEM`], computed away from this code
    /// from the public point in the key's DER encoding, so the assertion
    /// below compares against an outside answer rather than against another
    /// run of the same function.
    const FIXED_KEY_THUMBPRINT: &str = "h80DhGig146TX2_0VmmCwrI2rVxrJFrL9c6hQl5fHfU";

    #[test]
    fn a_stored_key_survives_the_round_trip() {
        // The whole point of keeping the key: a process that read it back
        // differently would be a new account, and a new account every
        // restart is how an installation runs into the authority's
        // registration limit without a single error of its own.
        let key = AccountKey::generate().unwrap();
        let pem = key.to_pem().unwrap();
        let reloaded = AccountKey::from_pem(&pem).unwrap();
        assert_eq!(key.thumbprint().unwrap(), reloaded.thumbprint().unwrap());
        assert_eq!(key.jwk_value(), reloaded.jwk_value());
    }

    #[test]
    fn the_jwk_holds_two_full_width_coordinates() {
        let key = AccountKey::generate().unwrap();
        let jwk = key.jwk_value();
        assert_eq!(jwk["kty"], "EC");
        assert_eq!(jwk["crv"], "P-256");
        // 32 bytes base64url with no padding is always 43 characters. A
        // coordinate that was not left-padded would sometimes be 42, and the
        // authority would refuse that signature about once in 256 runs.
        assert_eq!(jwk["x"].as_str().unwrap().len(), 43);
        assert_eq!(jwk["y"].as_str().unwrap().len(), 43);
    }

    #[test]
    fn the_thumbprint_of_a_known_key_is_a_known_value() {
        // Pinned rather than compared against another run of the same code:
        // the thumbprint is the second half of every key authorization, so a
        // change in how the canonical JSON is built would silently make every
        // challenge answer wrong, and a self-consistent test would still pass.
        let key = AccountKey::from_pem(FIXED_KEY_PEM).unwrap();
        assert_eq!(key.thumbprint().unwrap(), FIXED_KEY_THUMBPRINT);
        assert_eq!(
            key.jwk_value()["x"].as_str().unwrap(),
            "JXn_UuNyL_brKebI4yTsC-98WHdumH0y0bB5k9fyqBs"
        );
        assert_eq!(
            key.key_authorization("some-token").unwrap(),
            format!("some-token.{}", key.thumbprint().unwrap())
        );
    }

    #[test]
    fn a_signature_is_the_sixty_four_raw_bytes_es256_asks_for() {
        // OpenSSL hands back a DER SEQUENCE; JWS wants r and s concatenated.
        // Passing the DER through is the classic mistake here, and it fails
        // only at the authority.
        //
        // The check is that the halves are the scalars, verified against the
        // key that produced them, not that the first byte is not the DER
        // SEQUENCE tag: a raw signature legitimately begins with 0x30 about
        // one time in 256, and this test used to fail that often for a
        // correct implementation. A flaky test that guards a security
        // property is worse than none, because the next person reads a red
        // run as noise.
        let key = AccountKey::generate().unwrap();
        let public = EcKey::from_public_key(
            &EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap(),
            key.key.public_key(),
        )
        .unwrap();

        // Every signature is fixed width. DER is variable and, for P-256,
        // longer than this - so the length alone already rules it out, for
        // every key rather than for most of them.
        for _ in 0..64 {
            let signature = key.sign(b"anything").unwrap();
            assert_eq!(signature.len(), P256_COORDINATE_BYTES * 2);
        }

        let signature = key.sign(b"anything").unwrap();
        let (r, s) = signature.split_at(P256_COORDINATE_BYTES);
        let rebuilt = EcdsaSig::from_private_components(
            openssl::bn::BigNum::from_slice(r).unwrap(),
            openssl::bn::BigNum::from_slice(s).unwrap(),
        )
        .unwrap();
        let digest = hash(MessageDigest::sha256(), b"anything").unwrap();
        assert!(
            rebuilt.verify(&digest, &public).unwrap(),
            "the two halves are not the r and s the key signed with, so what is on the wire is \
             not an ES256 signature"
        );
    }
}
