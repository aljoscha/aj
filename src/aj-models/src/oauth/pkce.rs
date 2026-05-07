//! PKCE (Proof Key for Code Exchange) utilities for OAuth 2.0.
//!
//! Implements RFC 7636: a `code_verifier` is a cryptographically random
//! string, and the `code_challenge` is its SHA-256 hash, both encoded as
//! base64url-without-padding. Authorization servers compare the hash of
//! the verifier (sent at token-exchange time) against the challenge they
//! received with the initial authorization request, proving the same
//! client owns both ends of the flow without ever transmitting the
//! verifier in the redirect.
//!
//! Used by both the Anthropic and OpenAI OAuth flows (`docs/models-spec.md`
//! §9.3 and §9.4).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRngCore;
use sha2::{Digest, Sha256};

/// A PKCE verifier/challenge pair.
///
/// Hold onto [`PkcePair::verifier`] for the duration of the OAuth flow:
/// the authorization request carries [`PkcePair::challenge`], and the
/// later token-exchange request must echo back the matching verifier.
#[derive(Debug, Clone)]
pub struct PkcePair {
    /// Random base64url-encoded value sent with the token-exchange
    /// request as `code_verifier`.
    pub verifier: String,
    /// SHA-256 hash of [`PkcePair::verifier`], base64url-encoded.
    /// Sent with the authorization request as `code_challenge` (with
    /// `code_challenge_method=S256`).
    pub challenge: String,
}

/// Generate a fresh PKCE verifier/challenge pair using the OS CSPRNG.
///
/// Verifier length is 43 ASCII characters — the result of base64url-
/// encoding 32 random bytes — which sits well within RFC 7636's
/// 43–128 char range while keeping URLs short.
///
/// Returns an error if the operating system CSPRNG fails. We propagate
/// that rather than panic so the caller can present a meaningful login
/// failure (e.g. on locked-down systems with no entropy).
pub fn generate_pkce() -> Result<PkcePair, PkceError> {
    let mut verifier_bytes = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut verifier_bytes)
        .map_err(|err| PkceError::Random(err.to_string()))?;
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);

    let challenge = challenge_for(&verifier);

    Ok(PkcePair {
        verifier,
        challenge,
    })
}

/// Compute the S256 challenge for a given verifier string.
///
/// Exposed separately so callers that already hold a verifier (e.g.
/// resuming a flow from saved state) can derive the challenge without
/// going through [`generate_pkce`].
pub fn challenge_for(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    URL_SAFE_NO_PAD.encode(digest)
}

/// Errors that can occur while generating PKCE parameters.
#[derive(Debug, thiserror::Error)]
pub enum PkceError {
    /// The operating system's secure random source failed.
    #[error("failed to read system CSPRNG: {0}")]
    Random(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verifier must be base64url-without-padding of 32 random bytes,
    /// which is exactly 43 ASCII characters. This locks the format so we
    /// catch accidental encoding regressions.
    #[test]
    fn verifier_is_43_chars_base64url() {
        let pair = generate_pkce().expect("rng should succeed");
        assert_eq!(pair.verifier.len(), 43, "verifier length");
        assert!(
            pair.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "verifier must be base64url alphabet, got {:?}",
            pair.verifier
        );
        assert!(
            !pair.verifier.contains('='),
            "verifier must be unpadded, got {:?}",
            pair.verifier
        );
    }

    /// The challenge must be base64url-without-padding of a SHA-256 digest:
    /// 32 bytes → 43 characters in this encoding.
    #[test]
    fn challenge_is_43_chars_base64url() {
        let pair = generate_pkce().expect("rng should succeed");
        assert_eq!(pair.challenge.len(), 43, "challenge length");
        assert!(
            pair.challenge
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "challenge must be base64url alphabet, got {:?}",
            pair.challenge
        );
    }

    /// Successive calls must produce distinct verifiers, otherwise PKCE
    /// provides no replay protection.
    #[test]
    fn successive_pairs_differ() {
        let a = generate_pkce().expect("rng should succeed");
        let b = generate_pkce().expect("rng should succeed");
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }

    /// Pinned RFC 7636 Appendix B test vector: the canonical verifier
    /// `dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk` hashes to the
    /// canonical challenge `E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM`.
    /// This is the contract test for `challenge_for`.
    #[test]
    fn matches_rfc_test_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(challenge_for(verifier), expected);
    }

    /// `challenge_for` must agree with the challenge returned alongside
    /// the verifier from `generate_pkce`. This guards against drift
    /// between the helper and the generator.
    #[test]
    fn challenge_for_agrees_with_generator() {
        let pair = generate_pkce().expect("rng should succeed");
        assert_eq!(challenge_for(&pair.verifier), pair.challenge);
    }
}
