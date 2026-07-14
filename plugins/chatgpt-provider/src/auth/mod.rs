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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    Connected,
    LoginRequired,
    LoginInProgress,
    Error(String),
}

impl AuthState {
    pub fn wire_str(&self) -> &'static str {
        match self {
            AuthState::Connected => "connected",
            AuthState::LoginRequired => "login_required",
            AuthState::LoginInProgress => "login_in_progress",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginLease(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginStartOutcome {
    Started(LoginLease),
    AlreadyInProgress,
    AlreadyConnected,
}

pub struct AuthStore {
    inner: Mutex<Inner>,
    refresh_lock: Mutex<()>,
    refresh_url: String,
}

struct Inner {
    auth: Option<AuthDotJson>,
    state: AuthState,
    path: PathBuf,
    source: Option<TokenSource>,
    login_generation: u64,
    active_login: Option<LoginLease>,
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
                login_generation: 0,
                active_login: None,
            }),
            refresh_lock: Mutex::new(()),
            refresh_url: oauth::TOKEN_URL.to_owned(),
        })
    }

    #[doc(hidden)]
    pub async fn load_from_disk_with_refresh_url(
        path: &Path,
        refresh_url: String,
    ) -> Result<Self, ChatgptError> {
        let mut store = Self::load_from_disk(path).await?;
        store.refresh_url = refresh_url;
        Ok(store)
    }

    pub async fn snapshot(&self) -> AuthSnapshot {
        let g = self.inner.lock().await;
        AuthSnapshot {
            tokens: g.auth.as_ref().map(|a| a.tokens.clone()),
            state: g.state.clone(),
            source: g.source,
        }
    }

    pub async fn begin_login(&self) -> LoginStartOutcome {
        let mut g = self.inner.lock().await;
        match g.state {
            AuthState::LoginInProgress => LoginStartOutcome::AlreadyInProgress,
            AuthState::Connected => LoginStartOutcome::AlreadyConnected,
            AuthState::LoginRequired | AuthState::Error(_) => {
                g.login_generation = g.login_generation.wrapping_add(1);
                let lease = LoginLease(g.login_generation);
                g.active_login = Some(lease);
                g.state = AuthState::LoginInProgress;
                LoginStartOutcome::Started(lease)
            }
        }
    }

    /// Apply a fresh login result: persist to disk and transition to
    /// Connected. Used by the CLI `login` subcommand after `run_login`
    /// returns successfully.
    pub async fn apply_login_result(
        &self,
        lease: LoginLease,
        td: TokenData,
    ) -> Result<bool, ChatgptError> {
        let mut g = self.inner.lock().await;
        if g.active_login != Some(lease) || !matches!(g.state, AuthState::LoginInProgress) {
            return Ok(false);
        }
        let auth = AuthDotJson {
            tokens: td,
            last_refresh: Utc::now(),
        };
        store::save(&g.path, &auth)?;
        g.auth = Some(auth);
        g.state = AuthState::Connected;
        g.source = Some(TokenSource::Oauth);
        g.active_login = None;
        Ok(true)
    }

    pub async fn apply_login_error(
        &self,
        lease: LoginLease,
        message: String,
    ) -> Option<AuthSnapshot> {
        let mut g = self.inner.lock().await;
        if g.active_login != Some(lease) || !matches!(g.state, AuthState::LoginInProgress) {
            return None;
        }
        g.active_login = None;
        g.state = AuthState::Error(message);
        Some(snapshot_from_inner(&g))
    }

    /// Apply `<prefix>.auth.set { token }` — adopt a synthetic
    /// TokenData wrapping the raw bearer (no refresh available).
    /// Transitions to Connected with source=AuthSet. The id_token is
    /// stored empty because no JWT was minted by us; consumers that
    /// rely on `id_token` should special-case the empty string.
    pub async fn apply_auth_set(&self, raw_token: String) -> AuthSnapshot {
        self.apply_auth_set_at(raw_token, Utc::now()).await
    }

    #[doc(hidden)]
    pub async fn apply_auth_set_at(
        &self,
        raw_token: String,
        last_refresh: chrono::DateTime<Utc>,
    ) -> AuthSnapshot {
        let mut g = self.inner.lock().await;
        let td = TokenData {
            id_token: String::new(),
            access_token: AccessToken(raw_token),
            refresh_token: store::RefreshToken(String::new()),
            account_id: None,
        };
        let auth = AuthDotJson {
            tokens: td,
            last_refresh,
        };
        g.auth = Some(auth);
        g.state = AuthState::Connected;
        g.source = Some(TokenSource::AuthSet);
        g.active_login = None;
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
        g.active_login = None;
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
        snapshot_from_inner(&g)
    }

    /// Return a usable access token. OAuth credentials refresh from the
    /// JWT expiry; externally supplied tokens are never treated as OAuth.
    pub async fn current_access_token(&self) -> Result<AccessToken, ChatgptError> {
        let (source, observed_token, needs_refresh) = {
            let g = self.inner.lock().await;
            let auth = g.auth.as_ref().ok_or(ChatgptError::NoTokens)?;
            (
                g.source,
                auth.tokens.access_token.clone(),
                refresh::needs_refresh(auth, Utc::now()),
            )
        };
        if source == Some(TokenSource::Oauth) && needs_refresh {
            self.refresh_oauth(&observed_token, false).await?;
        }

        let g = self.inner.lock().await;
        Ok(g.auth
            .as_ref()
            .ok_or(ChatgptError::NoTokens)?
            .tokens
            .access_token
            .clone())
    }

    /// Adopt a changed auth file only when it belongs to the same account.
    /// This lets a standalone login repair a long-running provider.
    pub async fn adopt_disk_credentials(&self) -> Result<bool, ChatgptError> {
        let _refresh_guard = self.refresh_lock.lock().await;
        let mut g = self.inner.lock().await;
        adopt_disk_locked(&mut g)
    }

    /// Force one refresh after a 401, unless another waiter or process has
    /// already replaced the exact access token that failed.
    pub async fn force_refresh_after(
        &self,
        failed_token: &AccessToken,
    ) -> Result<AccessToken, ChatgptError> {
        self.refresh_oauth(failed_token, true).await
    }

    async fn refresh_oauth(
        &self,
        observed_token: &AccessToken,
        force: bool,
    ) -> Result<AccessToken, ChatgptError> {
        let _refresh_guard = self.refresh_lock.lock().await;
        let prior_tokens = {
            let mut g = self.inner.lock().await;
            if g.source != Some(TokenSource::Oauth) {
                return g
                    .auth
                    .as_ref()
                    .map(|auth| auth.tokens.access_token.clone())
                    .ok_or(ChatgptError::NoTokens);
            }
            let current = g.auth.as_ref().ok_or(ChatgptError::NoTokens)?;
            if &current.tokens.access_token != observed_token {
                return Ok(current.tokens.access_token.clone());
            }
            if adopt_disk_locked(&mut g)? {
                return g
                    .auth
                    .as_ref()
                    .map(|auth| auth.tokens.access_token.clone())
                    .ok_or(ChatgptError::NoTokens);
            }
            let current = g.auth.as_ref().ok_or(ChatgptError::NoTokens)?;
            if !force && !refresh::needs_refresh(current, Utc::now()) {
                return Ok(current.tokens.access_token.clone());
            }
            current.tokens.clone()
        };

        let fresh = match refresh::refresh_tokens_at(&prior_tokens, &self.refresh_url).await {
            Ok(fresh) => fresh,
            Err(error) => {
                let mut g = self.inner.lock().await;
                let current = g.auth.as_ref().ok_or(ChatgptError::NoTokens)?;
                if &current.tokens.access_token != observed_token {
                    return Ok(current.tokens.access_token.clone());
                }
                if adopt_disk_locked(&mut g)? {
                    return g
                        .auth
                        .as_ref()
                        .map(|auth| auth.tokens.access_token.clone())
                        .ok_or(ChatgptError::NoTokens);
                }
                return Err(error);
            }
        };
        let mut g = self.inner.lock().await;
        let current = g.auth.as_ref().ok_or(ChatgptError::NoTokens)?;
        if &current.tokens.access_token != observed_token {
            return Ok(current.tokens.access_token.clone());
        }
        if adopt_disk_locked(&mut g)? {
            return g
                .auth
                .as_ref()
                .map(|auth| auth.tokens.access_token.clone())
                .ok_or(ChatgptError::NoTokens);
        }
        let auth = AuthDotJson {
            tokens: fresh,
            last_refresh: Utc::now(),
        };
        store::save(&g.path, &auth)?;
        let access_token = auth.tokens.access_token.clone();
        g.auth = Some(auth);
        g.state = AuthState::Connected;
        g.source = Some(TokenSource::Oauth);
        Ok(access_token)
    }

    pub async fn mark_error(&self, msg: String) {
        let mut g = self.inner.lock().await;
        g.state = AuthState::Error(msg);
    }
}

fn snapshot_from_inner(inner: &Inner) -> AuthSnapshot {
    AuthSnapshot {
        tokens: inner.auth.as_ref().map(|a| a.tokens.clone()),
        state: inner.state.clone(),
        source: inner.source,
    }
}

fn adopt_disk_locked(inner: &mut Inner) -> Result<bool, ChatgptError> {
    if inner.source != Some(TokenSource::Oauth) {
        return Ok(false);
    }
    let Some(disk) = store::load(&inner.path)? else {
        return Ok(false);
    };
    let Some(current) = inner.auth.as_ref() else {
        return Ok(false);
    };
    if disk.tokens.access_token == current.tokens.access_token {
        return Ok(false);
    }
    let same_account = matches!(
        (&disk.tokens.account_id, &current.tokens.account_id),
        (Some(disk_account), Some(current_account)) if disk_account == current_account
    );
    if !same_account {
        tracing::warn!("ignored changed auth file for a different account");
        return Ok(false);
    }
    inner.auth = Some(disk);
    inner.state = AuthState::Connected;
    inner.source = Some(TokenSource::Oauth);
    Ok(true)
}
