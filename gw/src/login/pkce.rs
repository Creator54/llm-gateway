//! PKCE (RFC 7636) verifier/challenge and OAuth `state` generation.
//!
//! Randomness comes from `/dev/urandom`; the crate carries no RNG dependency.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};

/// Bytes per verifier: opencodex draws 96 and base64url-encodes them.
const VERIFIER_BYTES: usize = 96;
/// Bytes per `state`: 16, rendered as hex like the TypeScript flow.
const STATE_BYTES: usize = 16;

/// A PKCE pair.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// `len` bytes from `/dev/urandom`.
pub fn random_bytes(len: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = vec![0u8; len];
    std::fs::File::open("/dev/urandom")
        .context("/dev/urandom is unavailable")?
        .read_exact(&mut buf)
        .context("reading /dev/urandom")?;
    Ok(buf)
}

/// Unpadded base64url, the encoding OAuth expects for PKCE material.
pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// S256 challenge for a verifier: `base64url(sha256(verifier))`.
pub fn challenge(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

/// Fresh verifier plus its S256 challenge.
pub fn generate() -> Result<Pkce> {
    let verifier = b64url(&random_bytes(VERIFIER_BYTES)?);
    Ok(Pkce {
        challenge: challenge(&verifier),
        verifier,
    })
}

/// A CSRF `state` value: 16 random bytes as lowercase hex.
pub fn state() -> Result<String> {
    Ok(random_bytes(STATE_BYTES)?
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 appendix B.
    #[test]
    fn challenge_matches_the_rfc_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(challenge(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn verifier_is_urlsafe_and_long_enough() {
        let pkce = generate().unwrap();
        assert_eq!(pkce.verifier.len(), 128);
        assert!(pkce
            .verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(pkce.challenge.len(), 43);
        assert_ne!(pkce.verifier, generate().unwrap().verifier);
    }

    #[test]
    fn state_is_32_hex_chars() {
        let state = state().unwrap();
        assert_eq!(state.len(), 32);
        assert!(state.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(state, super::state().unwrap());
    }
}
