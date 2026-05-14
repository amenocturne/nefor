//! PKCE wire-shape tests — keep us honest against RFC 7636 §4.

use base64::Engine;
use sha2::{Digest, Sha256};

use chatgpt_provider::auth::pkce::generate_pkce;

#[test]
fn verifier_length_matches_64_byte_source() {
    // 64 bytes encoded as URL-safe base64 with no padding =
    // ceil(64 / 3) * 4 = 88, minus 2 pad chars stripped = 86 chars.
    let codes = generate_pkce();
    assert_eq!(codes.code_verifier.len(), 86);
}

#[test]
fn challenge_is_43_chars_base64url() {
    // SHA-256 produces 32 bytes → base64url-no-pad = 43 chars.
    let codes = generate_pkce();
    assert_eq!(codes.code_challenge.len(), 43);
    // No padding chars allowed in the challenge — auth.openai.com
    // rejects PKCE pairs with `=` in either field.
    assert!(!codes.code_challenge.contains('='));
    assert!(!codes.code_verifier.contains('='));
}

#[test]
fn challenge_matches_sha256_of_verifier() {
    let codes = generate_pkce();
    let digest = Sha256::digest(codes.code_verifier.as_bytes());
    let recomputed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    assert_eq!(codes.code_challenge, recomputed);
}

#[test]
fn generated_pairs_are_unique() {
    let a = generate_pkce();
    let b = generate_pkce();
    assert_ne!(a.code_verifier, b.code_verifier);
    assert_ne!(a.code_challenge, b.code_challenge);
}
