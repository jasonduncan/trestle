//! Secrets and digests used by login and sessions.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// 256 bits from the operating system, base64url (43 characters).
pub(crate) fn token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("operating system randomness is available");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Whether `value` has the shape [`token`] produces. Checked before any
/// lookup so arbitrary cookie values never reach the database.
pub(crate) fn is_token(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Lowercase hex SHA-256: what the database stores instead of a secret.
pub(crate) fn digest(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// PKCE S256 challenge for `verifier` (RFC 7636 §4.2).
pub(crate) fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_and_well_formed() {
        let a = token();
        let b = token();
        assert_ne!(a, b);
        assert!(is_token(&a));
        assert!(!is_token("short"));
        assert!(!is_token(&format!("{}=", &a[..42])));
        assert_eq!(digest(&a).len(), 64);
    }

    #[test]
    fn pkce_matches_rfc_7636_appendix_b() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
