//! Auth state machine for chatgpt-provider.
//!
//! Wraps the on-disk `AuthDotJson` plus a lifecycle enum. Owners hold
//! an `Arc<AuthStore>` and call `current_access_token()` to get a
//! ready-to-use bearer; the store transparently refreshes when needed.

pub mod oauth;
pub mod pkce;
pub mod refresh;
pub mod store;

use std::path::{Path, PathBuf};

use chrono::Utc;
use tokio::sync::Mutex;

use crate::auth::store::{AccessToken, AuthDotJson, TokenData};
use crate::error::ChatgptError;

/// Default access-token freshness window. Refresh once we cross this
/// many seconds since `last_refresh`. Picked conservatively: codex
/// refreshes every 8 minutes, but their refresh runs in the background;
/// we refresh on-demand and don't want every call to round-trip.
pub const DEFAULT_TOKEN_MAX_AGE_SECS: i64 = 28 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    Connected,
    LoginRequired,
    Error(String),
}

impl AuthState {
    pub fn wire_str(&self) -> &'static str {
        match self {
            AuthState::Connected => "connected",
            AuthState::LoginRequired => "login_required",
            AuthState::Error(_) => "error",
        }
    }
}

/// Where the current credentials came from. Drives logout semantics: a
/// token set via `chatgpt.auth.set` can be cleared at runtime; an
/// env-supplied access token can't (the env doesn't change under us).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// Loaded from disk via the OAuth login flow.
    Oauth,
    /// Pushed in via `<prefix>.auth.set`. No refresh token available.
    AuthSet,
    /// Loaded from a static env var (e.g. ngrok-style fallback).
    Env,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSnapshot {
    pub tokens: Option<TokenData>,
    pub state: AuthState,
    pub source: Option<TokenSource>,
}

/// Outcome of a logout request. The dispatcher pattern-matches on this
/// to either emit a Connected→LoginRequired status transition or an
/// Error status explaining the env-source refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogoutOutcome {
    Cleared,
    RefusedEnv,
}

pub struct AuthStore {
    inner: Mutex<Inner>,
}

struct Inner {
    auth: Option<AuthDotJson>,
    state: AuthState,
    path: PathBuf,
    source: Option<TokenSource>,
}

impl AuthStore {
    /// Load from disk. Missing file → `LoginRequired`; parse error
    /// surfaces as `ChatgptError::Json`.
    pub async fn load_from_disk(path: &Path) -> Result<Self, ChatgptError> {
        let auth = store::load(path)?;
        let (state, source) = match &auth {
            Some(_) => (AuthState::Connected, Some(TokenSource::Oauth)),
            None => (AuthState::LoginRequired, None),
        };
        Ok(Self {
            inner: Mutex::new(Inner {
                auth,
                state,
                path: path.to_path_buf(),
                source,
            }),
        })
    }

    pub async fn snapshot(&self) -> AuthSnapshot {
        let g = self.inner.lock().await;
        AuthSnapshot {
            tokens: g.auth.as_ref().map(|a| a.tokens.clone()),
            state: g.state.clone(),
            source: g.source,
        }
    }

    /// Apply a fresh login result: persist to disk and transition to
    /// Connected. Used by the CLI `login` subcommand after `run_login`
    /// returns successfully.
    pub async fn apply_login_result(&self, td: TokenData) -> Result<(), ChatgptError> {
        let mut g = self.inner.lock().await;
        let auth = AuthDotJson {
            tokens: td,
            last_refresh: Utc::now(),
        };
        store::save(&g.path, &auth)?;
        g.auth = Some(auth);
        g.state = AuthState::Connected;
        g.source = Some(TokenSource::Oauth);
        Ok(())
    }

    /// Apply `<prefix>.auth.set { token }` — adopt a synthetic
    /// TokenData wrapping the raw bearer (no refresh available).
    /// Transitions to Connected with source=AuthSet. The id_token is
    /// stored empty because no JWT was minted by us; consumers that
    /// rely on `id_token` should special-case the empty string.
    pub async fn apply_auth_set(&self, raw_token: String) -> AuthSnapshot {
        let mut g = self.inner.lock().await;
        let td = TokenData {
            id_token: String::new(),
            access_token: AccessToken(raw_token),
            refresh_token: store::RefreshToken(String::new()),
            account_id: None,
        };
        let auth = AuthDotJson {
            tokens: td,
            last_refresh: Utc::now(),
        };
        g.auth = Some(auth);
        g.state = AuthState::Connected;
        g.source = Some(TokenSource::AuthSet);
        AuthSnapshot {
            tokens: g.auth.as_ref().map(|a| a.tokens.clone()),
            state: g.state.clone(),
            source: g.source,
        }
    }

    /// Apply `<prefix>.logout_requested`. Refuses for env-sourced
    /// credentials so the next refresh doesn't silently re-pull from
    /// the env. The on-disk file is left intact when source is
    /// `AuthSet` (it was never written) but cleared when source is
    /// `Oauth`.
    pub async fn apply_logout(&self) -> LogoutOutcome {
        let mut g = self.inner.lock().await;
        match g.source {
            Some(TokenSource::Env) => LogoutOutcome::RefusedEnv,
            Some(TokenSource::Oauth) => {
                // Best-effort: remove the file. Any error is logged
                // but does not block the in-memory clear — the
                // operator's intent ("forget my creds") wins.
                if let Err(e) = std::fs::remove_file(&g.path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(error = %e, "logout: could not remove auth file");
                    }
                }
                g.auth = None;
                g.state = AuthState::LoginRequired;
                g.source = None;
                LogoutOutcome::Cleared
            }
            Some(TokenSource::AuthSet) | None => {
                g.auth = None;
                g.state = AuthState::LoginRequired;
                g.source = None;
                LogoutOutcome::Cleared
            }
        }
    }

    /// Transition to `Error(message)` for an explicit auth failure
    /// (HTTP 401 on Responses, OAuth login that errored out, etc.).
    pub async fn apply_error(&self, message: String) -> AuthSnapshot {
        let mut g = self.inner.lock().await;
        g.state = AuthState::Error(message);
        AuthSnapshot {
            tokens: g.auth.as_ref().map(|a| a.tokens.clone()),
            state: g.state.clone(),
            source: g.source,
        }
    }

    /// Return a usable access token, refreshing first when the
    /// in-memory copy is older than `DEFAULT_TOKEN_MAX_AGE_SECS`.
    /// Fails with `NoTokens` if the store is in `LoginRequired`.
    pub async fn current_access_token(&self) -> Result<AccessToken, ChatgptError> {
        let needs_refresh = {
            let g = self.inner.lock().await;
            match &g.auth {
                None => return Err(ChatgptError::NoTokens),
                Some(auth) => refresh::is_expired(auth, Utc::now(), DEFAULT_TOKEN_MAX_AGE_SECS),
            }
        };

        if needs_refresh {
            // Grab a copy of the refresh token under the lock, drop the
            // lock before the network call so other callers aren't
            // blocked on HTTP latency.
            let refresh_token = {
                let g = self.inner.lock().await;
                g.auth
                    .as_ref()
                    .ok_or(ChatgptError::NoTokens)?
                    .tokens
                    .refresh_token
                    .clone()
            };
            let fresh = refresh::refresh_tokens(&refresh_token).await?;
            let mut g = self.inner.lock().await;
            let auth = AuthDotJson {
                tokens: fresh,
                last_refresh: Utc::now(),
            };
            store::save(&g.path, &auth)?;
            g.auth = Some(auth);
            g.state = AuthState::Connected;
        }

        let g = self.inner.lock().await;
        Ok(g.auth
            .as_ref()
            .ok_or(ChatgptError::NoTokens)?
            .tokens
            .access_token
            .clone())
    }

    pub async fn mark_error(&self, msg: String) {
        let mut g = self.inner.lock().await;
        g.state = AuthState::Error(msg);
    }
}
