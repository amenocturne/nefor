//! Auth state for openai-provider.
//!
//! The plugin owns its current bearer token and a state machine that
//! external auth plugins can drive via `<prefix>.auth.set` /
//! `<prefix>.logout_requested` events. State transitions emit
//! `<prefix>.auth.status` so chat (and any observer) can render the
//! provider's current auth posture.
//!
//! `openai-provider` itself has **no built-in OAuth flow** — it's a thin
//! HTTP client. `<prefix>.login_requested` therefore transitions to
//! `Error` with a message that points the user at an external auth
//! plugin. The error sticks until something pushes a token via
//! `<prefix>.auth.set`.

use tokio::sync::Mutex;

/// Where the current token came from. Drives logout semantics: tokens
/// pushed by an auth plugin can be cleared, but env-supplied tokens
/// can't be revoked at runtime (the env doesn't change under us).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// `OPENAI_PROVIDER_API_KEY` was set at startup.
    Env,
    /// An external auth plugin pushed a token via `<prefix>.auth.set`.
    AuthSet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    Connected,
    LoginRequired,
    Error(String),
}

impl AuthState {
    /// Wire string for `<prefix>.auth.status { state: … }`.
    pub fn wire_str(&self) -> &'static str {
        match self {
            AuthState::Connected => "connected",
            AuthState::LoginRequired => "login_required",
            AuthState::Error(_) => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSnapshot {
    pub token: Option<String>,
    pub state: AuthState,
    pub source: Option<TokenSource>,
}

pub struct AuthStore {
    inner: Mutex<Inner>,
}

#[derive(Debug, Clone)]
struct Inner {
    token: Option<String>,
    state: AuthState,
    source: Option<TokenSource>,
}

impl AuthStore {
    /// Initialize from the env-supplied API key. `None` → `LoginRequired`,
    /// `Some(_)` → `Connected` with `TokenSource::Env`.
    pub fn from_env_key(key: Option<String>) -> Self {
        let (state, source) = match &key {
            Some(_) => (AuthState::Connected, Some(TokenSource::Env)),
            None => (AuthState::LoginRequired, None),
        };
        Self {
            inner: Mutex::new(Inner {
                token: key,
                state,
                source,
            }),
        }
    }

    pub async fn snapshot(&self) -> AuthSnapshot {
        let g = self.inner.lock().await;
        AuthSnapshot {
            token: g.token.clone(),
            state: g.state.clone(),
            source: g.source,
        }
    }

    pub async fn token(&self) -> Option<String> {
        self.inner.lock().await.token.clone()
    }

    /// Apply `<prefix>.auth.set` — adopt the new token, transition to
    /// Connected, mark source as AuthSet.
    pub async fn apply_auth_set(&self, token: String) -> AuthSnapshot {
        let mut g = self.inner.lock().await;
        g.token = Some(token);
        g.state = AuthState::Connected;
        g.source = Some(TokenSource::AuthSet);
        AuthSnapshot {
            token: g.token.clone(),
            state: g.state.clone(),
            source: g.source,
        }
    }

    /// Apply `<prefix>.logout_requested`.
    ///
    /// Returns `(new_state, message_for_status)`:
    /// - If token came from `auth.set`: clear token, transition to
    ///   LoginRequired, no message.
    /// - If token came from env (or there's no token): refuse — emit an
    ///   error status without changing the stored state.
    pub async fn apply_logout(&self) -> LogoutOutcome {
        let mut g = self.inner.lock().await;
        match g.source {
            Some(TokenSource::AuthSet) => {
                g.token = None;
                g.state = AuthState::LoginRequired;
                g.source = None;
                LogoutOutcome::Cleared
            }
            Some(TokenSource::Env) => LogoutOutcome::RefusedEnv,
            None => LogoutOutcome::RefusedEnv,
        }
    }

    /// Apply `<prefix>.login_requested`. openai-provider has no built-in
    /// flow, so this transitions to Error.
    pub async fn apply_login_requested(&self, message: String) -> AuthSnapshot {
        let mut g = self.inner.lock().await;
        g.state = AuthState::Error(message);
        AuthSnapshot {
            token: g.token.clone(),
            state: g.state.clone(),
            source: g.source,
        }
    }

    /// Mark auth as failed (e.g. HTTP 401 mid-request).
    pub async fn mark_auth_error(&self, message: String) -> AuthSnapshot {
        let mut g = self.inner.lock().await;
        g.state = AuthState::Error(message);
        AuthSnapshot {
            token: g.token.clone(),
            state: g.state.clone(),
            source: g.source,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogoutOutcome {
    /// Token was cleared; state is now LoginRequired.
    Cleared,
    /// Logout refused because the token came from env (or no token at
    /// all). State is unchanged.
    RefusedEnv,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn from_env_key_some_yields_connected_env() {
        let s = AuthStore::from_env_key(Some("k".into()));
        let snap = s.snapshot().await;
        assert_eq!(snap.state, AuthState::Connected);
        assert_eq!(snap.source, Some(TokenSource::Env));
        assert_eq!(snap.token.as_deref(), Some("k"));
    }

    #[tokio::test]
    async fn from_env_key_none_yields_login_required() {
        let s = AuthStore::from_env_key(None);
        let snap = s.snapshot().await;
        assert_eq!(snap.state, AuthState::LoginRequired);
        assert_eq!(snap.source, None);
        assert!(snap.token.is_none());
    }

    #[tokio::test]
    async fn auth_set_transitions_to_connected_authset() {
        let s = AuthStore::from_env_key(None);
        let snap = s.apply_auth_set("tok".into()).await;
        assert_eq!(snap.state, AuthState::Connected);
        assert_eq!(snap.source, Some(TokenSource::AuthSet));
        assert_eq!(snap.token.as_deref(), Some("tok"));
    }

    #[tokio::test]
    async fn auth_set_overrides_env_token_marking_source_authset() {
        let s = AuthStore::from_env_key(Some("envkey".into()));
        let snap = s.apply_auth_set("newtok".into()).await;
        assert_eq!(snap.token.as_deref(), Some("newtok"));
        assert_eq!(snap.source, Some(TokenSource::AuthSet));
    }

    #[tokio::test]
    async fn logout_after_auth_set_clears_and_login_required() {
        let s = AuthStore::from_env_key(None);
        let _ = s.apply_auth_set("tok".into()).await;
        let outcome = s.apply_logout().await;
        assert_eq!(outcome, LogoutOutcome::Cleared);
        let snap = s.snapshot().await;
        assert_eq!(snap.state, AuthState::LoginRequired);
        assert!(snap.token.is_none());
        assert!(snap.source.is_none());
    }

    #[tokio::test]
    async fn logout_with_env_token_refused_no_clear() {
        let s = AuthStore::from_env_key(Some("envkey".into()));
        let outcome = s.apply_logout().await;
        assert_eq!(outcome, LogoutOutcome::RefusedEnv);
        let snap = s.snapshot().await;
        assert_eq!(snap.state, AuthState::Connected);
        assert_eq!(snap.token.as_deref(), Some("envkey"));
        assert_eq!(snap.source, Some(TokenSource::Env));
    }

    #[tokio::test]
    async fn logout_with_no_token_refused() {
        let s = AuthStore::from_env_key(None);
        let outcome = s.apply_logout().await;
        assert_eq!(outcome, LogoutOutcome::RefusedEnv);
    }

    #[tokio::test]
    async fn login_requested_transitions_to_error() {
        let s = AuthStore::from_env_key(None);
        let snap = s.apply_login_requested("no flow".into()).await;
        match snap.state {
            AuthState::Error(m) => assert!(m.contains("no flow")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mark_auth_error_transitions_to_error() {
        let s = AuthStore::from_env_key(Some("k".into()));
        let snap = s.mark_auth_error("HTTP 401".into()).await;
        assert_eq!(snap.state.wire_str(), "error");
    }

    #[test]
    fn wire_str_matches_contract() {
        assert_eq!(AuthState::Connected.wire_str(), "connected");
        assert_eq!(AuthState::LoginRequired.wire_str(), "login_required");
        assert_eq!(AuthState::Error("x".into()).wire_str(), "error");
    }
}
