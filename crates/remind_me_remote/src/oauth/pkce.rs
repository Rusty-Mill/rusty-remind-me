//! PKCE (RFC 7636) S256 verification.
//!
//! No new crypto/base64 dependency: the workspace's existing `sha256`
//! dependency already gives a hex digest, and base64url is small enough to
//! hand-roll for one 32-byte encode rather than pull in a whole `base64`
//! crate for it — the same "don't add a dependency for one call site"
//! reasoning `remind_me_core::remote`'s token-generation doc already
//! records for this workspace.

use remind_me_core::webhook::constant_time_eq;

const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Decode a lowercase hex string (as produced by `sha256::digest`) into raw
/// bytes. `None` on malformed input — not expected in practice, since the
/// only caller feeds it `sha256::digest`'s own output, but this avoids a
/// panic if that ever stops being true.
fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let chars: Vec<char> = hex.chars().collect();
    chars
        .chunks(2)
        .map(|pair| {
            let hi = pair[0].to_digit(16)?;
            let lo = pair[1].to_digit(16)?;
            Some(((hi << 4) | lo) as u8)
        })
        .collect()
}

/// Base64url-encode (RFC 4648 §5), no padding — what RFC 7636's
/// `code_challenge` is: `BASE64URL-ENCODE(SHA256(code_verifier))`.
fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64URL_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL_ALPHABET[(n & 0x3f) as usize] as char);
        }
    }
    out
}

/// Compute RFC 7636's S256 `code_challenge` for a given `code_verifier`.
pub fn code_challenge_s256(code_verifier: &str) -> String {
    let hex = sha256::digest(code_verifier);
    let bytes = hex_decode(&hex).unwrap_or_default();
    base64url_encode(&bytes)
}

/// Verify a presented `code_verifier` against the `code_challenge` recorded
/// at `/authorize` time. Constant-time: both are secret-adjacent (a
/// verifier that leaks via timing is exactly the proof-of-possession PKCE
/// exists to protect), matching this crate's `constant_time_eq` convention
/// (`auth.rs`'s `secret_gate`) rather than a plain `==`.
pub fn verify_pkce(code_verifier: &str, code_challenge: &str) -> bool {
    constant_time_eq(
        code_challenge_s256(code_verifier).as_bytes(),
        code_challenge.as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_challenge_matches_the_rfc_7636_appendix_b_test_vector() {
        // https://datatracker.ietf.org/doc/html/rfc7636#appendix-B
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge_s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verify_pkce_accepts_the_matching_verifier_and_rejects_a_wrong_one() {
        let challenge = code_challenge_s256("correct-horse-battery-staple");
        assert!(verify_pkce("correct-horse-battery-staple", &challenge));
        assert!(!verify_pkce("wrong-verifier", &challenge));
    }

    #[test]
    fn base64url_encoding_never_emits_padding_or_standard_base64_characters() {
        let encoded = code_challenge_s256("any-verifier-value-at-all");
        assert!(!encoded.contains('='));
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
    }
}
