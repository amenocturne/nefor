use base64::Engine;
use rand::RngCore;
use sha2::Digest;
use sha2::Sha256;

#[derive(Debug, Clone)]
pub struct PkceCodes {
    pub code_verifier: String,
    pub code_challenge: String,
}

/// Build a PKCE verifier/challenge pair per RFC 7636 §4.
///
/// Verifier: 64 random bytes, base64url-encoded without padding (~86
/// chars). Challenge (S256): base64url-no-pad of SHA-256 over the
/// verifier *string bytes* — matching codex's wire behavior so the auth
/// service accepts it identically.
pub fn generate_pkce() -> PkceCodes {
    let mut bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut bytes);
    let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    PkceCodes {
        code_verifier,
        code_challenge,
    }
}

/// 32 random bytes, base64url-no-pad — the OAuth `state` parameter.
pub fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
