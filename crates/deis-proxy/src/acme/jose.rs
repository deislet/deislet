//! The signing envelope ACME wraps every request in.
//!
//! RFC 8555 carries each request as a JSON Web Signature in flattened JSON
//! serialisation (RFC 7515 section 7.2.1), signed with the account key. The
//! protected header always names the request URL and a nonce the authority
//! handed out, which is what stops a captured request from being replayed
//! against a different resource or a second time.
//!
//! Only what that needs is implemented here: unpadded base64url, and the
//! `ES256` envelope. There is no general JOSE library in this process and
//! this is not the start of one.

use serde_json::json;

use super::account::AccountKey;
use super::error::AcmeError;

/// Unpadded base64url (RFC 4648 section 5), which is the only encoding that
/// appears anywhere in ACME.
///
/// Written out rather than pulled in: it is fifteen lines, and the
/// alternative is a dependency on the request path of a process that
/// terminates TLS for every tenant.
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        // Four characters carry three bytes; a short final chunk emits only
        // the characters its bytes actually reach into, and no padding,
        // which is what "unpadded" means here.
        let count = chunk.len() + 1;
        for i in 0..count {
            let index = (triple >> (18 - 6 * i)) & 0x3f;
            out.push(ALPHABET[index as usize] as char);
        }
    }
    out
}

/// Which key identity the protected header carries.
///
/// The distinction is not cosmetic: RFC 8555 section 6.2 requires the
/// account key itself in `newAccount` (there is no account URL yet to point
/// at) and requires the account URL on every later request. A request that
/// carried the wrong one is refused by the authority with `malformed`.
pub enum KeyId<'a> {
    /// The full public key, for `newAccount` only.
    Jwk,
    /// The account URL the authority returned when it registered the key.
    Kid(&'a str),
}

/// Build the flattened JWS for one ACME request.
///
/// `payload` is the request body; an empty string is the "POST-as-GET" form
/// RFC 8555 section 6.3 uses to read a resource, which is a signed request
/// with no body rather than a `GET`, so that reading an order is
/// authenticated the same way as creating one.
pub fn sign_request(
    key: &AccountKey,
    key_id: KeyId<'_>,
    url: &str,
    nonce: &str,
    payload: &str,
) -> Result<String, AcmeError> {
    let mut header = json!({
        "alg": "ES256",
        "nonce": nonce,
        "url": url,
    });
    match key_id {
        KeyId::Jwk => header["jwk"] = key.jwk_value(),
        KeyId::Kid(kid) => header["kid"] = json!(kid),
    }

    let protected = base64url(header.to_string().as_bytes());
    let payload = base64url(payload.as_bytes());
    let signing_input = format!("{protected}.{payload}");
    let signature = base64url(&key.sign(signing_input.as_bytes())?);

    Ok(json!({
        "protected": protected,
        "payload": payload,
        "signature": signature,
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inverse of [`base64url`], for reading back what the tests just
    /// encoded. Only the tests need it: nothing an authority sends this
    /// client back is base64url, so a decoder in the module itself would be
    /// code with no caller on the request path.
    fn decode_base64url(text: &str) -> Vec<u8> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut bits = 0u32;
        let mut held = 0u32;
        let mut out = Vec::new();
        for byte in text.bytes() {
            let value = ALPHABET
                .iter()
                .position(|c| *c == byte)
                .unwrap_or_else(|| panic!("{text:?} is not base64url"))
                as u32;
            bits = (bits << 6) | value;
            held += 6;
            if held >= 8 {
                held -= 8;
                out.push((bits >> held) as u8);
            }
        }
        out
    }

    #[test]
    fn base64url_matches_the_rfc4648_vectors() {
        // RFC 4648 section 10, with the two characters that differ from
        // standard base64 and with the padding taken off - which is exactly
        // where a hand-written encoder goes wrong.
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_two_characters_that_are_not_standard_base64_are_produced() {
        // 0xfb 0xff encodes to the last two alphabet entries. Standard
        // base64 would answer "+/8", and an authority given that instead of
        // "-_8" refuses the request as malformed - a failure that would only
        // ever show up against a real CA.
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert!(!base64url(&[0xfb, 0xff, 0xfe]).contains('+'));
        assert!(!base64url(&[0xfb, 0xff, 0xfe]).contains('/'));
        assert!(!base64url(&[0xff]).contains('='));
    }

    #[test]
    fn a_signed_request_carries_the_url_and_the_nonce_it_was_given() {
        // The two fields that make a captured request useless anywhere else:
        // an authority checks both, and a client that left either out would
        // work in a test harness that did not.
        let key = AccountKey::generate().unwrap();
        let jws = sign_request(
            &key,
            KeyId::Kid("https://ca.test/acct/1"),
            "https://ca.test/order",
            "nonce-1",
            "{}",
        )
        .unwrap();
        let jws: serde_json::Value = serde_json::from_str(&jws).unwrap();

        let protected = jws["protected"].as_str().unwrap();
        let header: serde_json::Value =
            serde_json::from_slice(&decode_base64url(protected)).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["url"], "https://ca.test/order");
        assert_eq!(header["nonce"], "nonce-1");
        assert_eq!(header["kid"], "https://ca.test/acct/1");
        assert!(
            header.get("jwk").is_none(),
            "a request that names an account URL must not also carry the key; \
             RFC 8555 section 6.2 allows exactly one of the two"
        );
        assert!(jws["signature"].as_str().unwrap().len() > 80);
    }

    #[test]
    fn registration_carries_the_key_itself_because_there_is_no_account_yet() {
        let key = AccountKey::generate().unwrap();
        let jws = sign_request(
            &key,
            KeyId::Jwk,
            "https://ca.test/new-account",
            "nonce-2",
            "{}",
        )
        .unwrap();
        let jws: serde_json::Value = serde_json::from_str(&jws).unwrap();
        let header: serde_json::Value =
            serde_json::from_slice(&decode_base64url(jws["protected"].as_str().unwrap())).unwrap();
        assert_eq!(header["jwk"]["kty"], "EC");
        assert_eq!(header["jwk"]["crv"], "P-256");
        assert!(header.get("kid").is_none());
    }
}
