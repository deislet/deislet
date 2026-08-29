//! Percent-decoding of request paths.
//!
//! A browser escapes anything in a URL path that is not a plain ASCII path
//! character, so a file called `report 2026.pdf` is asked for as
//! `/report%202026.pdf`. Everything the bundle records - an asset's path, a
//! route pattern's literal segments - is the unescaped text, because that is
//! what the file tree holds. Comparing the two forms without decoding first
//! never matches, and the failure is silent: the file is in the archive and
//! listed in the manifest, and the request 404s anyway with nothing logged.
//!
//! Both readers of a request path need the same answer, so the decoder lives
//! here rather than once per reader.

/// Decode the `%XX` escapes in a request path.
///
/// Only percent-escapes are decoded. `+` is left alone: it means a space in a
/// query string, never in a path, and turning it into one would make
/// `/a+b.txt` unable to name the file actually called `a+b.txt`.
///
/// Bytes that do not form valid UTF-8 once decoded become the Unicode
/// replacement character rather than failing the call - a malformed escape is
/// a request that will not match anything, not an error worth propagating.
///
/// Note for callers that also reject `..`: decode *first*, then look for it.
/// The other order lets `%2e%2e` through.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some(byte) = decode_escape(bytes, index) {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The byte a `%XX` escape starting at `index` stands for, or `None` when what
/// is there is not a complete escape - a bare `%`, or one followed by anything
/// other than two hex digits.
fn decode_escape(bytes: &[u8], index: usize) -> Option<u8> {
    if *bytes.get(index)? != b'%' {
        return None;
    }
    let high = hex_digit(*bytes.get(index + 1)?)?;
    let low = hex_digit(*bytes.get(index + 2)?)?;
    Some(high * 16 + low)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_space_comes_back_as_a_space() {
        assert_eq!(percent_decode("/my%20file.txt"), "/my file.txt");
    }

    #[test]
    fn multi_byte_utf8_survives_a_round_trip() {
        assert_eq!(
            percent_decode("/caf%C3%A9/men%C3%BC.txt"),
            "/caf\u{e9}/men\u{fc}.txt"
        );
    }

    #[test]
    fn a_plus_stays_a_plus() {
        // In a path it is an ordinary character; decoding it to a space would
        // make the file `a+b.txt` unaddressable.
        assert_eq!(percent_decode("/a+b.txt"), "/a+b.txt");
    }

    #[test]
    fn a_malformed_escape_is_left_as_written() {
        assert_eq!(percent_decode("/100%off"), "/100%off");
        assert_eq!(percent_decode("/trailing%2"), "/trailing%2");
        assert_eq!(percent_decode("/%zz"), "/%zz");
    }

    #[test]
    fn an_escaped_dot_dot_decodes_to_a_dot_dot() {
        // Which is why the traversal check has to run after this, not before.
        assert_eq!(percent_decode("/%2e%2e/etc"), "/../etc");
        assert_eq!(percent_decode("/a%2F..%2Fb"), "/a/../b");
    }

    #[test]
    fn invalid_utf8_does_not_fail_the_call() {
        assert_eq!(percent_decode("/%FF"), "/\u{fffd}");
    }
}
