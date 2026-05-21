//! NCP event dispatch loop and per-turn streaming task.
//!
//! Two layers live here:
//!
//! 1. `run_dispatch_loop` — owns the inbound envelope channel, routes
//!    each event to a handler. Cancelled by stdin close, by an
//!    incoming `Body::System(Shutdown)`, or by ctrl-c.
//! 2. `spawn_turn` — the per-chat task that POSTs to `/responses`,
//!    streams events, persists assistant messages, and emits
//!    `<prefix>.stream.delta`/`stream.end`/`chat.complete.result`
//!    along the way. Tool calls are yielded back to the caller.
//!
//! Shape mirrors openai-provider's main.rs but threaded through the
//! Responses-API typed stream from Phase 3 instead of the
//! chat-completions SSE parser.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::auth::{AuthSnapshot, AuthState, AuthStore, LogoutOutcome};
use crate::broker::{ToolBroker, ToolResult};
use crate::catalog::ToolCatalog;
use crate::config::ServeArgs;
use crate::error::ChatgptError;
use nefor_plugin_sdk::TransportError;
use crate::responses::request::{Reasoning, ReasoningSummary, ResponseItem, ResponsesApiRequest};
use crate::responses::stream::ResponseEvent;
use crate::responses::{ModelEntry, ResponsesClient};
use crate::state::{ChatId, ChatStats, Chats, ChatsError, Message, ToolCall, ToolCallFunction};
use crate::translator;

pub const PROTOCOL_VERSION: &str = "0.1";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Cap on tool-call iterations per turn. The model can loop forever
/// asking for tools; this prevents runaways.
pub const TOOL_LOOP_MAX_ITERATIONS: u32 = 20;

const LOGOUT_REFUSED_ENV_MESSAGE: &str =
    "no login to revoke — credentials come from the environment; restart the plugin without it";

const HTTP_401_MESSAGE: &str = "auth failed (HTTP 401) — re-login via chatgpt-provider login";

const NO_LOGIN_FLOW_IN_PROGRESS_MESSAGE: &str =
    "login already in progress; wait for completion or restart the plugin";

// ---------------------------------------------------------------------
// Wire shape helpers — every event we emit is built here.
//
// Field names match openai-provider's emissions so downstream UIs can
// adapt the same body shape regardless of which provider produced the
// event. Differences from openai-provider:
//
// - No `last_turn_context_tokens` in session.stats (no prompt cache
//   awareness in v1; can be added when we wire pre-stream header
//   parsing).
// - `auth.status` carries our `source` field for diagnostics.
// ---------------------------------------------------------------------

fn make_event(kind: String, mut fields: Map<String, Value>) -> Map<String, Value> {
    fields.insert("kind".into(), Value::String(kind));
    fields
}

fn hello_body(args: &ServeArgs) -> Map<String, Value> {
    let mut m = Map::new();
    // No `model` field: the openai-provider translator only fans hello
    // out to `chat.model.set_ack` when model is a non-empty string
    // (init.lua:127-134). We don't know the user's pick until they
    // /model after login, so leaving it absent keeps the status bar
    // from being hijacked by our internal placeholder.
    m.insert("name".into(), Value::String(args.provider_name.clone()));
    m.insert("version".into(), Value::String(PLUGIN_VERSION.into()));
    make_event(format!("{}hello", args.event_prefix()), m)
}

fn ready_body(args: &ServeArgs) -> Map<String, Value> {
    make_event(format!("{}ready", args.event_prefix()), Map::new())
}

fn goodbye_body(args: &ServeArgs, reason: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("reason".into(), Value::String(reason.to_owned()));
    make_event(format!("{}goodbye", args.event_prefix()), m)
}

fn auth_status_body(args: &ServeArgs, snap: &AuthSnapshot) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("state".into(), Value::String(snap.state.wire_str().into()));
    if let AuthState::Error(msg) = &snap.state {
        m.insert("message".into(), Value::String(msg.clone()));
    }
    if let Some(src) = snap.source {
        let s = match src {
            crate::auth::TokenSource::Oauth => "oauth",
            crate::auth::TokenSource::AuthSet => "auth_set",
            crate::auth::TokenSource::Env => "env",
        };
        m.insert("source".into(), Value::String(s.into()));
    }
    // Tells the chat surface that this provider has a real login/logout
    // flow. Providers without one (mock, ollama via openai-provider with
    // a static token) don't set this, and the /login + /logout pickers
    // filter them out — picking "log in" on an authless provider is a
    // no-op the surface shouldn't offer.
    m.insert("supports_login".into(), Value::Bool(true));
    make_event(format!("{}auth.status", args.event_prefix()), m)
}

fn chat_created_body(args: &ServeArgs, chat_id: &ChatId) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    make_event(format!("{}chat.created", args.event_prefix()), m)
}

fn chat_appended_body(args: &ServeArgs, chat_id: &ChatId) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    make_event(format!("{}chat.appended", args.event_prefix()), m)
}

fn chat_deleted_body(args: &ServeArgs, chat_id: &ChatId) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    make_event(format!("{}chat.deleted", args.event_prefix()), m)
}

fn chat_error_body(args: &ServeArgs, chat_id: &ChatId, message: String) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("message".into(), Value::String(message));
    make_event(format!("{}chat.error", args.event_prefix()), m)
}

fn turn_error_body(
    args: &ServeArgs,
    chat_id: Option<&ChatId>,
    message: &str,
) -> Map<String, Value> {
    let mut m = Map::new();
    if let Some(cid) = chat_id {
        m.insert("chat_id".into(), Value::String(cid.to_string()));
    }
    m.insert("message".into(), Value::String(message.to_owned()));
    make_event(format!("{}turn.error", args.event_prefix()), m)
}

fn stream_delta_body(prefix: &str, id: &str, chat_id: &ChatId, text: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("text".into(), Value::String(text.to_owned()));
    make_event(format!("{prefix}stream.delta"), m)
}

fn stream_reasoning_delta_body(
    prefix: &str,
    id: &str,
    chat_id: &ChatId,
    text: &str,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("text".into(), Value::String(text.to_owned()));
    make_event(format!("{prefix}stream.reasoning_delta"), m)
}

fn stream_reasoning_end_body(
    prefix: &str,
    id: &str,
    chat_id: &ChatId,
    text: &str,
    duration_ms: u64,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("text".into(), Value::String(text.to_owned()));
    m.insert("duration_ms".into(), Value::Number(duration_ms.into()));
    make_event(format!("{prefix}stream.reasoning_end"), m)
}

#[allow(clippy::too_many_arguments)]
fn stream_end_body(
    args: &ServeArgs,
    id: &str,
    chat_id: &ChatId,
    text: &str,
    model: &str,
    duration_ms: u64,
    finish_reason: Option<&str>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("text".into(), Value::String(text.to_owned()));
    m.insert("model".into(), Value::String(model.to_owned()));
    m.insert("duration_ms".into(), Value::Number(duration_ms.into()));
    if let Some(r) = finish_reason {
        m.insert("finish_reason".into(), Value::String(r.to_owned()));
    }
    make_event(format!("{}stream.end", args.event_prefix()), m)
}

fn session_stats_body(args: &ServeArgs, chat_id: &ChatId, stats: &ChatStats) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    if let Some(model) = &stats.model {
        m.insert("model".into(), Value::String(model.clone()));
    }
    m.insert("turns".into(), Value::Number(stats.turns_completed.into()));
    m.insert(
        "cumulative_input_tokens".into(),
        Value::Number(stats.cumulative_input_tokens.into()),
    );
    m.insert(
        "cumulative_output_tokens".into(),
        Value::Number(stats.cumulative_output_tokens.into()),
    );
    m.insert(
        "last_turn_input_tokens".into(),
        Value::Number(stats.last_turn_input_tokens.into()),
    );
    m.insert(
        "last_turn_output_tokens".into(),
        Value::Number(stats.last_turn_output_tokens.into()),
    );
    if let Some(d) = stats.last_turn_duration_ms {
        m.insert("last_turn_duration_ms".into(), Value::Number(d.into()));
    }
    make_event(format!("{}session.stats", args.event_prefix()), m)
}

fn models_listed_body(args: &ServeArgs, models: &[ModelEntry]) -> Map<String, Value> {
    let arr: Vec<Value> = models
        .iter()
        .map(|m| Value::String(m.slug.clone()))
        .collect();
    let mut m = Map::new();
    m.insert("models".into(), Value::Array(arr));
    let ctx_map: Map<String, Value> = models
        .iter()
        .filter_map(|me| {
            me.context_length
                .map(|cw| (me.slug.clone(), Value::Number(cw.into())))
        })
        .collect();
    if !ctx_map.is_empty() {
        m.insert("context_windows".into(), Value::Object(ctx_map));
    }
    make_event(format!("{}models.listed", args.event_prefix()), m)
}

fn model_set_ack_body(
    args: &ServeArgs,
    model: &str,
    chat_id: Option<&ChatId>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("model".into(), Value::String(model.to_owned()));
    if let Some(cid) = chat_id {
        m.insert("chat_id".into(), Value::String(cid.to_string()));
    }
    make_event(format!("{}model.set_ack", args.event_prefix()), m)
}

#[allow(clippy::too_many_arguments)]
fn chat_complete_result_body(
    args: &ServeArgs,
    chat_id: &ChatId,
    text: &str,
    tool_calls: &[ToolCall],
    finish_reason: Option<&str>,
) -> Map<String, Value> {
    let mut output = Map::new();
    output.insert("text".into(), Value::String(text.to_owned()));
    if !tool_calls.is_empty() {
        let arr: Vec<Value> = tool_calls
            .iter()
            .map(|tc| {
                let args_v = serde_json::from_str::<Value>(&tc.function.arguments)
                    .unwrap_or_else(|_| Value::String(tc.function.arguments.clone()));
                let mut entry = Map::new();
                entry.insert("id".into(), Value::String(tc.id.clone()));
                entry.insert("name".into(), Value::String(tc.function.name.clone()));
                entry.insert("arguments".into(), args_v);
                Value::Object(entry)
            })
            .collect();
        output.insert("tool_calls".into(), Value::Array(arr));
    }
    if let Some(r) = finish_reason {
        output.insert("finish_reason".into(), Value::String(r.to_owned()));
    }
    let mut m = Map::new();
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("output".into(), Value::Object(output));
    if let Some(r) = finish_reason {
        m.insert("finish_reason".into(), Value::String(r.to_owned()));
    }
    make_event(format!("{}chat.complete.result", args.event_prefix()), m)
}

// ---------------------------------------------------------------------
// Public helpers used by main.rs / tests.
// ---------------------------------------------------------------------

pub async fn send_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: Map<String, Value>,
) -> Result<(), ChatgptError> {
    out_tx
        .send(PluginOutgoing::event(body))
        .await
        .map_err(|_| ChatgptError::Transport(TransportError::WriterClosed))
}

pub async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), ChatgptError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| ChatgptError::Transport(TransportError::WriterClosed))
}

/// Convenience used at startup to emit hello/ready/auth.status in order.
/// If auth is already Connected (tokens on disk), also kicks off a
/// background fetch of `/models` so the chat surface's `/model` picker
/// has entries the first time the user opens it.
pub async fn emit_startup_events(
    args: &Arc<ServeArgs>,
    auth: &Arc<AuthStore>,
    chats: &Arc<Chats>,
    responses_client: &Arc<ResponsesClient>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
) -> Result<(), ChatgptError> {
    send_event(out_tx, hello_body(args)).await?;
    send_event(out_tx, ready_body(args)).await?;
    let snap = auth.snapshot().await;
    send_event(out_tx, auth_status_body(args, &snap)).await?;
    if matches!(snap.state, AuthState::Connected) {
        spawn_models_fetch(
            args.clone(),
            auth.clone(),
            chats.clone(),
            responses_client.clone(),
            out_tx.clone(),
        );
    }
    Ok(())
}

/// Fire-and-forget OAuth flow. Runs `run_login` on a tokio task,
/// applies the result to AuthStore, emits the new `auth.status` and
/// (on success) kicks off a `/models` fetch. Used by both
/// `<prefix>.login_requested` and `<prefix>.model.set` (when not yet
/// Connected — picking a model auto-logs in).
fn spawn_login_flow(
    args: Arc<ServeArgs>,
    auth: Arc<AuthStore>,
    chats: Arc<Chats>,
    responses_client: Arc<ResponsesClient>,
    out_tx: mpsc::Sender<PluginOutgoing>,
) {
    tokio::spawn(async move {
        let result = crate::auth::oauth::run_login(
            true,
            &crate::auth::store::default_auth_path()
                .unwrap_or_else(|_| std::path::PathBuf::from("chatgpt-auth.json")),
        )
        .await;
        match result {
            Ok(td) => {
                if let Err(e) = auth.apply_login_result(td).await {
                    let snap = auth.apply_error(format!("apply login: {e}")).await;
                    let _ = send_event(&out_tx, auth_status_body(&args, &snap)).await;
                    return;
                }
                let snap = auth.snapshot().await;
                let _ = send_event(&out_tx, auth_status_body(&args, &snap)).await;
                if matches!(snap.state, AuthState::Connected) {
                    spawn_models_fetch(
                        args.clone(),
                        auth.clone(),
                        chats.clone(),
                        responses_client.clone(),
                        out_tx.clone(),
                    );
                }
            }
            Err(e) => {
                let snap = auth.apply_error(format!("login: {e}")).await;
                let _ = send_event(&out_tx, auth_status_body(&args, &snap)).await;
            }
        }
    });
}

/// Fire-and-forget background fetch of `/models`. Emits
/// `<prefix>.models.listed` on success (the translator fans it out to
/// `chat.models.listed`); on error logs and drops. Called whenever
/// auth transitions to Connected — at startup if tokens are on disk,
/// or after `auth.set`/`login_requested` completes.
fn spawn_models_fetch(
    args: Arc<ServeArgs>,
    auth: Arc<AuthStore>,
    chats: Arc<Chats>,
    responses_client: Arc<ResponsesClient>,
    out_tx: mpsc::Sender<PluginOutgoing>,
) {
    tokio::spawn(async move {
        let snap = auth.snapshot().await;
        if !matches!(snap.state, AuthState::Connected) {
            tracing::debug!("spawn_models_fetch: not connected, skipping");
            return;
        }
        match responses_client.list_models(&snap).await {
            Ok(models) => {
                tracing::info!(
                    count = models.len(),
                    slugs = ?models.iter().map(|m| m.slug.as_str()).collect::<Vec<_>>(),
                    "fetched /models from backend"
                );
                // Cache the API-reported capabilities so subsequent
                // chat.complete turns can decide on reasoning without
                // re-fetching. The backend is the authoritative source
                // (the `supports_reasoning_summaries` field tells us
                // exactly which models accept the parameter).
                chats
                    .record_model_capabilities(models.iter().map(|m| {
                        (
                            m.slug.clone(),
                            crate::state::ModelCapabilities {
                                supports_reasoning_summaries: m.supports_reasoning_summaries,
                            },
                        )
                    }))
                    .await;
                let body = models_listed_body(&args, &models);
                if let Err(e) = send_event(&out_tx, body).await {
                    tracing::warn!(error = %e, "spawn_models_fetch: send_event failed");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "spawn_models_fetch: list_models failed");
            }
        }
    });
}

/// Shared state threaded through every dispatch handler. Bundles the
/// seven Arc'd singletons that every event path needs so function
/// signatures stay short.
#[derive(Clone)]
pub struct DispatcherContext {
    pub args: Arc<ServeArgs>,
    pub chats: Arc<Chats>,
    pub auth: Arc<AuthStore>,
    pub catalog: Arc<ToolCatalog>,
    pub broker: Arc<ToolBroker>,
    pub responses_client: Arc<ResponsesClient>,
    pub out_tx: mpsc::Sender<PluginOutgoing>,
}

/// Top-level event loop. Returns on `Body::System(Shutdown)`, stdin
/// close, or ctrl-c. The caller emits goodbye after we return.
pub async fn run_dispatch_loop(
    ctx: DispatcherContext,
    mut in_rx: mpsc::Receiver<Result<Envelope, TransportError>>,
) -> Result<(), ChatgptError> {
    loop {
        tokio::select! {
            maybe = in_rx.recv() => {
                match maybe {
                    Some(Ok(env)) => match &env.body {
                        Body::System(SystemBody::Shutdown { .. }) => {
                            tracing::info!("shutdown received");
                            ctx.chats.interrupt_all().await;
                            return Ok(());
                        }
                        Body::System(_) => {
                            tracing::warn!(?env, "unexpected system envelope after handshake");
                        }
                        Body::Event(map) => {
                            if let Err(e) = dispatch_event(&ctx, &env.from, map).await {
                                tracing::error!(error = %e, "dispatch_event errored; continuing");
                            }
                        }
                    },
                    Some(Err(e)) => {
                        tracing::error!(error = %e, "stdin parse error; dropping line");
                    }
                    None => {
                        tracing::info!("stdin closed; exiting");
                        return Ok(());
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("ctrl-c; exiting");
                ctx.chats.interrupt_all().await;
                return Ok(());
            }
        }
    }
}

/// Emit goodbye after the loop returns.
pub async fn emit_goodbye(args: &ServeArgs, out_tx: &mpsc::Sender<PluginOutgoing>, reason: &str) {
    let _ = out_tx
        .send(PluginOutgoing::event(goodbye_body(args, reason)))
        .await;
}

async fn dispatch_event(
    ctx: &DispatcherContext,
    from: &PluginName,
    body: &Map<String, Value>,
) -> Result<(), ChatgptError> {
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(k) => k,
        None => return Ok(()),
    };

    // Cross-plugin namespace first: tool.register and tool.result.
    match kind {
        "tool.register" => {
            let tools = body
                .get("tools")
                .map(ToolCatalog::parse_tools)
                .unwrap_or_default();
            let from_str = from.as_str().to_owned();
            tracing::info!(plugin = %from_str, count = tools.len(), "tool.register");
            ctx.catalog.register_from(&from_str, tools).await;
            return Ok(());
        }
        "tool.result" => {
            let id = match body.get("id").and_then(Value::as_str) {
                Some(s) if !s.is_empty() => s.to_owned(),
                _ => {
                    tracing::warn!("tool.result missing required `id`; dropping");
                    return Ok(());
                }
            };
            let output = body
                .get("output")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let error = body.get("error").and_then(Value::as_str).map(str::to_owned);
            let delivered = ctx
                .broker
                .deliver(ToolResult {
                    id: id.clone(),
                    output,
                    error,
                })
                .await;
            if !delivered {
                tracing::debug!(id = %id, "tool.result for unknown id; dropping");
            }
            return Ok(());
        }
        _ => {}
    }

    let prefix = ctx.args.event_prefix();
    let suffix = match kind.strip_prefix(&prefix) {
        Some(s) => s,
        None => return Ok(()),
    };

    match suffix {
        "chat.create" => handle_chat_create(&ctx.args, &ctx.chats, &ctx.out_tx, body).await,
        "chat.append" => handle_chat_append(&ctx.args, &ctx.chats, &ctx.out_tx, body).await,
        "chat.complete" => handle_chat_complete(ctx, body).await,
        "chat.delete" => handle_chat_delete(&ctx.args, &ctx.chats, &ctx.out_tx, body).await,
        "interrupt" => {
            match read_chat_id(body) {
                Some(cid) => {
                    ctx.chats.interrupt(&cid).await;
                }
                None => ctx.chats.interrupt_all().await,
            }
            Ok(())
        }
        "reset" => {
            ctx.chats.interrupt_all().await;
            ctx.chats.reset_all().await;
            Ok(())
        }
        "auth.set" => {
            let token = match body.get("token").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => t.to_owned(),
                _ => {
                    tracing::warn!("auth.set without non-empty token; ignoring");
                    return Ok(());
                }
            };
            let snap = ctx.auth.apply_auth_set(token).await;
            send_event(&ctx.out_tx, auth_status_body(&ctx.args, &snap)).await?;
            if matches!(snap.state, AuthState::Connected) {
                spawn_models_fetch(
                    ctx.args.clone(),
                    ctx.auth.clone(),
                    ctx.chats.clone(),
                    ctx.responses_client.clone(),
                    ctx.out_tx.clone(),
                );
            }
            Ok(())
        }
        "login_requested" => {
            let snap = ctx.auth.snapshot().await;
            if matches!(snap.state, AuthState::LoginRequired | AuthState::Error(_)) {
                spawn_login_flow(
                    ctx.args.clone(),
                    ctx.auth.clone(),
                    ctx.chats.clone(),
                    ctx.responses_client.clone(),
                    ctx.out_tx.clone(),
                );
                Ok(())
            } else {
                let snap = ctx
                    .auth
                    .apply_error(NO_LOGIN_FLOW_IN_PROGRESS_MESSAGE.to_owned())
                    .await;
                send_event(&ctx.out_tx, auth_status_body(&ctx.args, &snap)).await
            }
        }
        "logout_requested" => {
            // Cancel any in-flight turns before tearing down auth — a
            // turn mid-stream would otherwise emit a confusing 401 turn
            // error after the user explicitly asked to log out.
            ctx.chats.interrupt_all().await;
            // Snapshot the refresh token BEFORE apply_logout clears it;
            // post the revoke on a background task so the user-visible
            // status update lands immediately. Revoke failures are
            // logged and ignored — local-side cleanup happens regardless.
            let pre = ctx.auth.snapshot().await;
            let refresh_token = pre.tokens.as_ref().map(|t| t.refresh_token.clone());
            match ctx.auth.apply_logout().await {
                LogoutOutcome::Cleared => {
                    if let Some(rt) = refresh_token {
                        tokio::spawn(async move {
                            if let Err(e) = crate::auth::refresh::revoke_tokens(&rt).await {
                                tracing::warn!(error = %e, "logout: revoke call failed");
                            } else {
                                tracing::info!("logout: refresh token revoked server-side");
                            }
                        });
                    }
                    let snap = ctx.auth.snapshot().await;
                    send_event(&ctx.out_tx, auth_status_body(&ctx.args, &snap)).await
                }
                LogoutOutcome::RefusedEnv => {
                    let snap = ctx
                        .auth
                        .apply_error(LOGOUT_REFUSED_ENV_MESSAGE.to_owned())
                        .await;
                    send_event(&ctx.out_tx, auth_status_body(&ctx.args, &snap)).await
                }
            }
        }
        "models.list_requested" => {
            let snap = ctx.auth.snapshot().await;
            if !matches!(snap.state, AuthState::Connected) {
                // No tokens to authenticate the /models call. Surface an
                // empty list rather than 401-erroring on the chat surface.
                tracing::debug!("models.list_requested while not connected; emitting empty list");
                return send_event(&ctx.out_tx, models_listed_body(&ctx.args, &[])).await;
            }
            match ctx.responses_client.list_models(&snap).await {
                Ok(models) => {
                    send_event(&ctx.out_tx, models_listed_body(&ctx.args, &models)).await
                }
                Err(e) => {
                    let msg = format!("failed to fetch /models: {e}");
                    tracing::warn!(error = %e, "models.list_requested failed");
                    send_event(&ctx.out_tx, turn_error_body(&ctx.args, None, &msg)).await
                }
            }
        }
        "model.set" => {
            let model = match body.get("model").and_then(Value::as_str) {
                Some(m) if !m.is_empty() => m.to_owned(),
                _ => {
                    tracing::warn!("model.set without non-empty model; ignoring");
                    return Ok(());
                }
            };
            ctx.chats.set_default_model(model.clone()).await;
            let chat_id = read_chat_id(body);
            if let Some(cid) = &chat_id {
                if ctx.chats.exists(cid).await {
                    let _ = ctx.chats.set_chat_model(cid, model.clone()).await;
                }
            }
            send_event(
                &ctx.out_tx,
                model_set_ack_body(&ctx.args, &model, chat_id.as_ref()),
            )
            .await?;
            // Picking a chatgpt model implicitly opts into auth: if we
            // aren't connected yet, kick off OAuth so the user doesn't
            // have to separately `/login`. Status events keep the chat
            // surface in sync as the flow progresses.
            let snap = ctx.auth.snapshot().await;
            if matches!(snap.state, AuthState::LoginRequired | AuthState::Error(_)) {
                spawn_login_flow(
                    ctx.args.clone(),
                    ctx.auth.clone(),
                    ctx.chats.clone(),
                    ctx.responses_client.clone(),
                    ctx.out_tx.clone(),
                );
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------
// Per-event handlers.
// ---------------------------------------------------------------------

fn read_chat_id(body: &Map<String, Value>) -> Option<ChatId> {
    body.get("chat_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ChatId::new)
}

/// A tool call from the model that failed to parse into a [`ToolCall`].
/// When `id` is present the caller can surface the error as a synthetic
/// tool-result message so the model sees what went wrong.
struct ToolCallParseFailure {
    id: Option<String>,
    error: String,
    raw: Value,
}

/// Parse result that carries both the message and any tool-call entries
/// that failed to deserialise. The caller is responsible for surfacing
/// failures (push synthetic tool results for those with IDs, warn for
/// the rest).
struct ParsedMessage {
    message: Message,
    tool_call_failures: Vec<ToolCallParseFailure>,
}

fn parse_provider_message(value: Option<&Value>) -> Result<ParsedMessage, String> {
    let obj = value
        .and_then(Value::as_object)
        .ok_or_else(|| "chat.append `message` must be an object".to_owned())?;
    let role = obj
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| "chat.append message missing `role`".to_owned())?;
    let content = obj.get("content").and_then(|v| match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    });
    match role {
        "user" => Ok(ParsedMessage {
            message: Message::User {
                content: content.unwrap_or_default(),
            },
            tool_call_failures: Vec::new(),
        }),
        "assistant" => {
            let mut tool_calls = Vec::new();
            let mut tool_call_failures = Vec::new();
            if let Some(arr) = obj.get("tool_calls").and_then(Value::as_array) {
                for v in arr {
                    match serde_json::from_value::<ToolCall>(v.clone()) {
                        Ok(tc) => tool_calls.push(tc),
                        Err(e) => {
                            let id = v.get("id").and_then(Value::as_str).map(str::to_owned);
                            tool_call_failures.push(ToolCallParseFailure {
                                id,
                                error: e.to_string(),
                                raw: v.clone(),
                            });
                        }
                    }
                }
            }
            Ok(ParsedMessage {
                message: Message::Assistant {
                    content,
                    tool_calls,
                },
                tool_call_failures,
            })
        }
        "system" => Ok(ParsedMessage {
            message: Message::System {
                content: content.unwrap_or_default(),
            },
            tool_call_failures: Vec::new(),
        }),
        "tool" => {
            let tool_call_id = obj
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_default();
            let name = obj.get("name").and_then(Value::as_str).map(str::to_owned);
            Ok(ParsedMessage {
                message: Message::Tool {
                    content: content.unwrap_or_default(),
                    tool_call_id,
                    name,
                },
                tool_call_failures: Vec::new(),
            })
        }
        other => Err(format!("chat.append message has unknown role `{other}`")),
    }
}

async fn handle_chat_create(
    args: &ServeArgs,
    chats: &Arc<Chats>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
) -> Result<(), ChatgptError> {
    let chat_id = match read_chat_id(body) {
        Some(id) => id,
        None => {
            send_event(
                out_tx,
                turn_error_body(args, None, "chat.create missing `chat_id`"),
            )
            .await?;
            return Ok(());
        }
    };
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let system = body
        .get("system")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    // chat.create.tools wire shape (matches openai-provider):
    //   - false  → tools disabled (empty allowlist filters everything out)
    //   - [names] → allowlist of tool names (provider filters its catalog)
    //   - absent  → no filter (use the whole catalog)
    // Reasoners/agentic-loop emit string arrays for the LEAD's
    // ORCHESTRATION_TOOLS. We previously parsed `tools` as tool-spec
    // objects, which silently produced an empty override and bypassed
    // the catalog — leaving the model with no tools at all.
    let tools_field = body.get("tools");
    let tool_allowlist: Option<Vec<String>> =
        if let Some(false) = tools_field.and_then(Value::as_bool) {
            Some(Vec::new())
        } else {
            tools_field.and_then(Value::as_array).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
        };
    let tool_overrides: Option<Vec<crate::catalog::ToolSpec>> = None;

    match chats
        .create(
            chat_id.clone(),
            model,
            system,
            tool_overrides,
            tool_allowlist,
        )
        .await
    {
        Ok(()) => send_event(out_tx, chat_created_body(args, &chat_id)).await,
        Err(e) => send_event(out_tx, chat_error_body(args, &chat_id, e.to_string())).await,
    }
}

async fn handle_chat_append(
    args: &ServeArgs,
    chats: &Arc<Chats>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
) -> Result<(), ChatgptError> {
    let chat_id = match read_chat_id(body) {
        Some(id) => id,
        None => {
            send_event(
                out_tx,
                turn_error_body(args, None, "chat.append missing `chat_id`"),
            )
            .await?;
            return Ok(());
        }
    };
    let parsed = match parse_provider_message(body.get("message")) {
        Ok(m) => m,
        Err(msg) => {
            send_event(out_tx, chat_error_body(args, &chat_id, msg)).await?;
            return Ok(());
        }
    };
    match chats.append(&chat_id, parsed.message).await {
        Ok(()) => {
            // Surface tool-call parse failures as synthetic tool
            // result messages so the model sees what went wrong and
            // can self-correct on the next turn.
            for failure in &parsed.tool_call_failures {
                if let Some(id) = &failure.id {
                    let error_content = format!(
                        "Failed to parse tool call: {}. Raw: {}",
                        failure.error, failure.raw
                    );
                    let tool_msg = Message::tool_result(id.clone(), error_content);
                    let _ = chats.append(&chat_id, tool_msg).await;
                } else {
                    tracing::warn!(
                        error = %failure.error,
                        raw = %failure.raw,
                        "tool_call parse failed and no id to surface error to model",
                    );
                }
            }
            send_event(out_tx, chat_appended_body(args, &chat_id)).await
        }
        Err(e) => send_event(out_tx, chat_error_body(args, &chat_id, e.to_string())).await,
    }
}

async fn handle_chat_complete(
    ctx: &DispatcherContext,
    body: &Map<String, Value>,
) -> Result<(), ChatgptError> {
    let chat_id = match read_chat_id(body) {
        Some(id) => id,
        None => {
            send_event(
                &ctx.out_tx,
                turn_error_body(&ctx.args, None, "chat.complete missing `chat_id`"),
            )
            .await?;
            return Ok(());
        }
    };
    let cancel = match ctx.chats.begin_turn(&chat_id).await {
        Ok(t) => t,
        Err(ChatsError::Busy(_)) => {
            send_event(
                &ctx.out_tx,
                turn_error_body(&ctx.args, Some(&chat_id), "busy"),
            )
            .await?;
            return Ok(());
        }
        Err(e) => {
            send_event(
                &ctx.out_tx,
                chat_error_body(&ctx.args, &chat_id, e.to_string()),
            )
            .await?;
            return Ok(());
        }
    };
    spawn_turn(ctx.clone(), chat_id, cancel);
    Ok(())
}

async fn handle_chat_delete(
    args: &ServeArgs,
    chats: &Arc<Chats>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
) -> Result<(), ChatgptError> {
    let chat_id = match read_chat_id(body) {
        Some(id) => id,
        None => {
            send_event(
                out_tx,
                turn_error_body(args, None, "chat.delete missing `chat_id`"),
            )
            .await?;
            return Ok(());
        }
    };
    chats.interrupt(&chat_id).await;
    match chats.delete(&chat_id).await {
        Ok(()) => send_event(out_tx, chat_deleted_body(args, &chat_id)).await,
        Err(e) => send_event(out_tx, chat_error_body(args, &chat_id, e.to_string())).await,
    }
}

// ---------------------------------------------------------------------
// Per-turn task.
// ---------------------------------------------------------------------

/// Per-call argument buffer for streaming function-call args.
#[derive(Default)]
struct ToolCallBuffer {
    /// item_id → (call_id, name, accumulated args). The Responses API
    /// keys deltas by item_id rather than call_id, so we have to track
    /// both.
    by_item_id: HashMap<String, PendingCall>,
    /// Insertion-ordered list of item_ids for stable iteration when
    /// emitting calls.
    order: Vec<String>,
}

struct PendingCall {
    call_id: String,
    name: String,
    args: String,
}

impl ToolCallBuffer {
    fn on_item_added(&mut self, item_id: String, call_id: String, name: String, args: String) {
        if !self.by_item_id.contains_key(&item_id) {
            self.order.push(item_id.clone());
        }
        self.by_item_id.insert(
            item_id,
            PendingCall {
                call_id,
                name,
                args,
            },
        );
    }

    fn on_args_delta(&mut self, item_id: Option<&str>, delta: &str) {
        let Some(item_id) = item_id else { return };
        if let Some(entry) = self.by_item_id.get_mut(item_id) {
            entry.args.push_str(delta);
        }
    }

    /// Finalize a call when `OutputItemDone` arrives. Prefer the
    /// `done` event's args when it's longer (some models send the
    /// complete JSON in done rather than via deltas).
    fn on_item_done(&mut self, item_id: Option<&str>, final_args: &str) {
        let Some(item_id) = item_id else { return };
        if let Some(entry) = self.by_item_id.get_mut(item_id) {
            if final_args.len() > entry.args.len() {
                entry.args = final_args.to_owned();
            }
        }
    }

    fn into_tool_calls(self) -> Vec<ToolCall> {
        let mut by_id = self.by_item_id;
        self.order
            .into_iter()
            .filter_map(|item_id| by_id.remove(&item_id))
            .map(|pc| ToolCall {
                id: pc.call_id,
                function: ToolCallFunction {
                    name: pc.name,
                    arguments: pc.args,
                },
            })
            .collect()
    }
}

fn spawn_turn(
    ctx: DispatcherContext,
    chat_id: ChatId,
    cancel: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        let turn_id = uuid::Uuid::new_v4().to_string();
        let started = std::time::Instant::now();
        let mut iterations: u32 = 0;
        let mut final_text = String::new();
        // Every loop path assigns this before break; the initial value
        // is unused but the linter can't see that across the loop,
        // so suppress the false positive.
        #[allow(unused_assignments)]
        let mut final_finish_reason: Option<String> = None;
        let mut final_tool_calls: Vec<ToolCall> = Vec::new();
        let mut interrupted = false;
        let mut errored = false;
        let mut total_input_tokens: u64 = 0;
        let mut total_output_tokens: u64 = 0;
        let mut active_model = String::new();

        loop {
            iterations += 1;
            if iterations > TOOL_LOOP_MAX_ITERATIONS {
                tracing::warn!(cap = TOOL_LOOP_MAX_ITERATIONS, "tool-loop cap hit");
                errored = true;
                final_finish_reason = Some("error".into());
                let _ = ctx
                    .out_tx
                    .send(PluginOutgoing::event(turn_error_body(
                        &ctx.args,
                        Some(&chat_id),
                        &format!(
                            "tool-loop iteration cap hit ({} iterations); aborting",
                            TOOL_LOOP_MAX_ITERATIONS
                        ),
                    )))
                    .await;
                break;
            }

            let snapshot = match ctx.chats.snapshot(&chat_id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(chat_id = %chat_id, error = %e, "chat vanished mid-turn");
                    errored = true;
                    final_finish_reason = Some("error".into());
                    let _ = ctx
                        .out_tx
                        .send(PluginOutgoing::event(chat_error_body(
                            &ctx.args,
                            &chat_id,
                            e.to_string(),
                        )))
                        .await;
                    break;
                }
            };
            active_model = snapshot.model.clone();

            // Build tools list — per-chat overrides win; otherwise the
            // catalog, optionally filtered by allowlist.
            let tools_specs = match snapshot.tool_overrides.clone() {
                Some(t) => t,
                None => ctx.catalog.all().await,
            };
            let filtered_specs: Vec<_> = match &snapshot.tool_allowlist {
                Some(allowed) => tools_specs
                    .into_iter()
                    .filter(|t| allowed.iter().any(|a| a == &t.name))
                    .collect(),
                None => tools_specs,
            };
            let tools_json = translator::tools_to_responses_format(&filtered_specs);
            tracing::info!(
                count = filtered_specs.len(),
                names = ?filtered_specs.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
                allowlist = ?snapshot.tool_allowlist,
                "sending tools to Responses API"
            );

            let translated =
                translator::history_to_input(&snapshot.history, snapshot.system.as_deref());
            tracing::info!(
                instructions_len = translated.instructions.len(),
                instructions_preview = %translated.instructions.chars().take(120).collect::<String>(),
                input_items = translated.input.len(),
                model = %snapshot.model,
                "Responses request — final payload summary"
            );

            // Reasoning capability resolution, in order of authority:
            // 1. Runtime no-reasoning override (set on a 400 below)
            //    always wins — the live endpoint disagreed with us once.
            // 2. /models capability cache — backend tells us directly
            //    whether `reasoning.summary` is accepted for this slug.
            // 3. Static heuristic by slug prefix (gpt-5* / o-series) —
            //    only used when /models hasn't been fetched yet for
            //    this model. The /models fetch fires on startup +
            //    auth.set, so this fallback is rare in practice.
            let supports_reasoning =
                if ctx.chats.model_reasoning_unsupported(&snapshot.model).await {
                    false
                } else if let Some(api) =
                    ctx.chats.model_capability_reasoning(&snapshot.model).await
                {
                    api
                } else {
                    translator::model_supports_reasoning(&snapshot.model)
                };
            let include = if supports_reasoning {
                vec!["reasoning.encrypted_content".to_string()]
            } else {
                Vec::new()
            };
            let reasoning = supports_reasoning.then_some(Reasoning {
                effort: None,
                summary: Some(ReasoningSummary::Concise),
            });

            let req = ResponsesApiRequest {
                model: snapshot.model.clone(),
                instructions: translated.instructions,
                input: translated.input,
                tools: tools_json,
                tool_choice: "auto".into(),
                parallel_tool_calls: false,
                reasoning,
                store: false,
                stream: true,
                include,
                service_tier: None,
                prompt_cache_key: None,
                text: None,
            };

            // Snapshot auth before sending so we can fail fast on
            // LoginRequired/Error without burning an HTTP round trip.
            let auth_snap = ctx.auth.snapshot().await;
            if !matches!(auth_snap.state, AuthState::Connected) {
                let _ = ctx
                    .out_tx
                    .send(PluginOutgoing::event(auth_status_body(&ctx.args, &auth_snap)))
                    .await;
                let _ = ctx
                    .out_tx
                    .send(PluginOutgoing::event(turn_error_body(
                        &ctx.args,
                        Some(&chat_id),
                        "auth not connected; cannot complete turn",
                    )))
                    .await;
                errored = true;
                final_finish_reason = Some("error".into());
                break;
            }

            // Refresh token if needed before building headers. The
            // current_access_token call refreshes under the hood; we
            // grab a fresh snapshot afterwards because the cached
            // tokens may have rotated.
            if let Err(e) = ctx.auth.current_access_token().await {
                let snap = ctx.auth.apply_error(format!("refresh: {e}")).await;
                let _ = ctx
                    .out_tx
                    .send(PluginOutgoing::event(auth_status_body(&ctx.args, &snap)))
                    .await;
                let _ = ctx
                    .out_tx
                    .send(PluginOutgoing::event(turn_error_body(
                        &ctx.args,
                        Some(&chat_id),
                        &format!("token refresh failed: {e}"),
                    )))
                    .await;
                errored = true;
                final_finish_reason = Some("error".into());
                break;
            }
            let auth_snap = ctx.auth.snapshot().await;

            let mut stream = match ctx.responses_client.stream(&req, &auth_snap).await {
                Ok(s) => s,
                Err(ChatgptError::ResponsesEndpoint { status, body }) => {
                    if status == 401 {
                        let snap = ctx.auth.apply_error(HTTP_401_MESSAGE.to_owned()).await;
                        let _ = ctx
                            .out_tx
                            .send(PluginOutgoing::event(auth_status_body(&ctx.args, &snap)))
                            .await;
                    }
                    // Reactive fallback: some gpt-5-family slugs
                    // (`gpt-5.3-codex-spark`, etc.) match
                    // `model_supports_reasoning`'s `gpt-5` prefix but
                    // reject the `reasoning.summary` parameter the
                    // request carries. Mark the model and retry the
                    // same iteration with reasoning disabled; the
                    // next pass builds the request without it because
                    // `chats.model_reasoning_unsupported` is now true.
                    if status == 400
                        && supports_reasoning
                        && body_signals_reasoning_unsupported(&body)
                    {
                        tracing::info!(
                            model = %snapshot.model,
                            body = %snippet(&body),
                            "model rejected reasoning — falling back to no-reasoning mode for this model",
                        );
                        ctx.chats
                            .mark_model_reasoning_unsupported(&snapshot.model)
                            .await;
                        iterations = iterations.saturating_sub(1);
                        continue;
                    }
                    let _ = ctx
                        .out_tx
                        .send(PluginOutgoing::event(turn_error_body(
                            &ctx.args,
                            Some(&chat_id),
                            &format!("HTTP {status}: {}", snippet(&body)),
                        )))
                        .await;
                    errored = true;
                    final_finish_reason = Some("error".into());
                    break;
                }
                Err(e) => {
                    let _ = ctx
                        .out_tx
                        .send(PluginOutgoing::event(turn_error_body(
                            &ctx.args,
                            Some(&chat_id),
                            &format!("request failed: {e}"),
                        )))
                        .await;
                    errored = true;
                    final_finish_reason = Some("error".into());
                    break;
                }
            };

            let mut output_text = String::new();
            let mut reasoning_text = String::new();
            let mut reasoning_started_at: Option<std::time::Instant> = None;
            let mut tool_buf = ToolCallBuffer::default();
            let mut iter_finish_reason: Option<String> = None;
            let mut iter_input_tokens: u64 = 0;
            let mut iter_output_tokens: u64 = 0;
            let mut iter_interrupted = false;
            let mut iter_errored: Option<String> = None;

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        iter_interrupted = true;
                        break;
                    }
                    next = stream.next() => {
                        match next {
                            Some(Ok(event)) => match event {
                                ResponseEvent::OutputTextDelta { delta, .. } => {
                                    output_text.push_str(&delta);
                                    let body = stream_delta_body(
                                        &ctx.args.event_prefix(),
                                        &turn_id,
                                        &chat_id,
                                        &delta,
                                    );
                                    let _ = ctx.out_tx.try_send(PluginOutgoing::event(body));
                                }
                                ResponseEvent::ReasoningSummaryDelta { delta, .. }
                                | ResponseEvent::ReasoningContentDelta { delta, .. } => {
                                    if reasoning_started_at.is_none() {
                                        reasoning_started_at = Some(std::time::Instant::now());
                                    }
                                    reasoning_text.push_str(&delta);
                                    let body = stream_reasoning_delta_body(
                                        &ctx.args.event_prefix(),
                                        &turn_id,
                                        &chat_id,
                                        &delta,
                                    );
                                    let _ = ctx.out_tx.try_send(PluginOutgoing::event(body));
                                }
                                ResponseEvent::FunctionCallArgumentsDelta { delta, item_id } => {
                                    tool_buf.on_args_delta(item_id.as_deref(), &delta);
                                }
                                ResponseEvent::FunctionCallArgumentsDone {
                                    arguments,
                                    item_id,
                                } => {
                                    // Terminal for the streamed args.
                                    // `output_item.done` follows shortly
                                    // after with the same payload; using
                                    // both is harmless because
                                    // `on_item_done` only overwrites
                                    // when the new value is longer.
                                    tool_buf.on_item_done(item_id.as_deref(), &arguments);
                                }
                                ResponseEvent::OutputItemAdded {
                                    item:
                                        ResponseItem::FunctionCall {
                                            id,
                                            call_id,
                                            name,
                                            arguments,
                                        },
                                    ..
                                } => {
                                    // Deltas arrive keyed by the item's
                                    // server-side `id` (`fc_…`), not by
                                    // `call_id` (`call_…`). Use the id
                                    // when present so streamed args land
                                    // in the right buffer.
                                    let item_id = id.unwrap_or_else(|| call_id.clone());
                                    tool_buf.on_item_added(item_id, call_id, name, arguments);
                                }
                                ResponseEvent::OutputItemAdded { .. } => {}
                                ResponseEvent::OutputItemDone {
                                    item:
                                        ResponseItem::FunctionCall {
                                            id,
                                            call_id,
                                            name,
                                            arguments,
                                        },
                                    ..
                                } => {
                                    let item_id = id.unwrap_or_else(|| call_id.clone());
                                    if !tool_buf.by_item_id.contains_key(&item_id) {
                                        // Single-shot: no Added + no
                                        // deltas, only Done. Seed and
                                        // finalize in one step.
                                        tool_buf.on_item_added(
                                            item_id,
                                            call_id,
                                            name,
                                            arguments,
                                        );
                                    } else {
                                        tool_buf.on_item_done(Some(&item_id), &arguments);
                                    }
                                }
                                ResponseEvent::OutputItemDone { .. } => {}
                                ResponseEvent::Completed { response } => {
                                    iter_finish_reason =
                                        response.get("finish_reason").and_then(|v| v.as_str()).map(str::to_owned);
                                    if let Some(usage) = response.get("usage") {
                                        if let Some(input) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                                            iter_input_tokens = input;
                                        }
                                        if let Some(output) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                                            iter_output_tokens = output;
                                        }
                                    }
                                    break;
                                }
                                ResponseEvent::Failed { response } => {
                                    let msg = response
                                        .get("error")
                                        .and_then(|e| e.get("message"))
                                        .and_then(|m| m.as_str())
                                        .unwrap_or("response.failed")
                                        .to_string();
                                    iter_errored = Some(msg);
                                    break;
                                }
                                ResponseEvent::Incomplete { response } => {
                                    iter_finish_reason = response
                                        .get("incomplete_details")
                                        .and_then(|d| d.get("reason"))
                                        .and_then(|r| r.as_str())
                                        .map(str::to_owned)
                                        .or_else(|| Some("incomplete".into()));
                                    break;
                                }
                                _ => {}
                            },
                            Some(Err(e)) => {
                                iter_errored = Some(format!("stream error: {e}"));
                                break;
                            }
                            None => break,
                        }
                    }
                }
            }

            // Emit per-turn reasoning_end if we accumulated any.
            if !reasoning_text.is_empty() {
                let duration_ms = reasoning_started_at
                    .map(|s| s.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                let body = stream_reasoning_end_body(
                    &ctx.args.event_prefix(),
                    &turn_id,
                    &chat_id,
                    &reasoning_text,
                    duration_ms,
                );
                let _ = ctx.out_tx.send(PluginOutgoing::event(body)).await;
            }

            total_input_tokens = total_input_tokens.saturating_add(iter_input_tokens);
            total_output_tokens = total_output_tokens.saturating_add(iter_output_tokens);

            if let Some(err_msg) = iter_errored {
                let _ = ctx
                    .out_tx
                    .send(PluginOutgoing::event(turn_error_body(
                        &ctx.args,
                        Some(&chat_id),
                        &err_msg,
                    )))
                    .await;
                errored = true;
                final_finish_reason = Some("error".into());
                break;
            }

            if iter_interrupted {
                if !output_text.is_empty() {
                    let _ = ctx.chats.push_assistant(&chat_id, output_text.clone()).await;
                }
                final_text = output_text;
                final_finish_reason = Some("interrupted".into());
                interrupted = true;
                break;
            }

            let tool_calls = tool_buf.into_tool_calls();
            if !tool_calls.is_empty() {
                let _ = ctx
                    .chats
                    .push_assistant_tool_calls(&chat_id, output_text.clone(), tool_calls.clone())
                    .await;

                final_text = output_text;
                final_finish_reason = iter_finish_reason.or(Some("tool_calls".into()));
                final_tool_calls = tool_calls;
                break;
            }

            // No tool calls → terminal turn.
            if !output_text.is_empty() {
                let _ = ctx.chats.push_assistant(&chat_id, output_text.clone()).await;
            }
            final_text = output_text;
            final_finish_reason = iter_finish_reason.or(Some("stop".into()));
            break;
        }

        let elapsed_ms = started.elapsed().as_millis() as u64;
        let _ = ctx
            .chats
            .record_turn(
                &chat_id,
                Some(&active_model),
                total_input_tokens,
                total_output_tokens,
                elapsed_ms,
            )
            .await;

        let body = stream_end_body(
            &ctx.args,
            &turn_id,
            &chat_id,
            &final_text,
            &active_model,
            elapsed_ms,
            final_finish_reason.as_deref(),
        );
        let _ = ctx.out_tx.send(PluginOutgoing::event(body)).await;
        if let Ok(stats) = ctx.chats.stats_snapshot(&chat_id).await {
            let _ = ctx
                .out_tx
                .send(PluginOutgoing::event(session_stats_body(
                    &ctx.args, &chat_id, &stats,
                )))
                .await;
        }
        if interrupted && !errored {
            let _ = ctx
                .out_tx
                .send(PluginOutgoing::event(turn_error_body(
                    &ctx.args,
                    Some(&chat_id),
                    "interrupted",
                )))
                .await;
        }

        let body = chat_complete_result_body(
            &ctx.args,
            &chat_id,
            &final_text,
            &final_tool_calls,
            final_finish_reason.as_deref(),
        );
        let _ = ctx.out_tx.send(PluginOutgoing::event(body)).await;

        ctx.chats.end_turn(&chat_id).await;
    });
}

fn snippet(s: &str) -> String {
    if s.len() <= 200 {
        s.to_owned()
    } else {
        format!("{}…", &s[..200])
    }
}

/// Does a 400 response body indicate the model rejected the `reasoning`
/// parameter? Codex's backend phrases the failure as
/// `"Unsupported parameter: 'reasoning.summary' is not supported with
/// the '<model>' model."` — we substring-match the `reasoning.` prefix
/// inside an `Unsupported parameter` clause so future reasoning
/// sub-fields (effort, etc.) trigger the same fallback.
fn body_signals_reasoning_unsupported(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("unsupported parameter") && lower.contains("reasoning.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> ServeArgs {
        ServeArgs {
            provider_name: "chatgpt".into(),
            base_url: "https://example.invalid".into(),
        }
    }

    #[test]
    fn hello_body_omits_model_to_avoid_status_bar_hijack() {
        let body = hello_body(&args());
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("chatgpt.hello")
        );
        // No `model` — the openai-provider translator drops hello when
        // model is absent, so chatgpt's startup doesn't masquerade as
        // a chat.model.set_ack.
        assert!(body.get("model").is_none());
        assert_eq!(body.get("name").and_then(Value::as_str), Some("chatgpt"));
        assert!(body.get("version").and_then(Value::as_str).is_some());
    }

    #[test]
    fn auth_status_body_includes_state_and_source() {
        let snap = AuthSnapshot {
            tokens: None,
            state: AuthState::LoginRequired,
            source: None,
        };
        let body = auth_status_body(&args(), &snap);
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("chatgpt.auth.status")
        );
        assert_eq!(
            body.get("state").and_then(Value::as_str),
            Some("login_required")
        );
        assert!(body.get("message").is_none());
    }

    #[test]
    fn auth_status_body_includes_error_message() {
        let snap = AuthSnapshot {
            tokens: None,
            state: AuthState::Error("HTTP 401".into()),
            source: Some(crate::auth::TokenSource::Oauth),
        };
        let body = auth_status_body(&args(), &snap);
        assert_eq!(body.get("state").and_then(Value::as_str), Some("error"));
        assert_eq!(
            body.get("message").and_then(Value::as_str),
            Some("HTTP 401")
        );
        assert_eq!(body.get("source").and_then(Value::as_str), Some("oauth"));
    }

    #[test]
    fn models_listed_emits_flat_slug_strings() {
        let fetched = vec![
            ModelEntry {
                slug: "gpt-5".into(),
                display_name: Some("GPT-5".into()),
                description: None,
                priority: Some(10),
                supports_reasoning_summaries: true,
                context_length: None,
            },
            ModelEntry {
                slug: "gpt-5-codex".into(),
                display_name: Some("GPT-5 Codex".into()),
                description: Some("coding model".into()),
                priority: Some(20),
                supports_reasoning_summaries: false,
                context_length: None,
            },
        ];
        let body = models_listed_body(&args(), &fetched);
        let models = body.get("models").and_then(Value::as_array).expect("array");
        let slugs: Vec<&str> = models.iter().filter_map(Value::as_str).collect();
        assert_eq!(slugs, vec!["gpt-5", "gpt-5-codex"]);
    }

    #[test]
    fn models_listed_empty_when_no_models_fetched() {
        let body = models_listed_body(&args(), &[]);
        let models = body.get("models").and_then(Value::as_array).expect("array");
        assert!(models.is_empty());
    }

    #[test]
    fn parse_provider_message_round_trips_user_role() {
        let v = serde_json::json!({"role": "user", "content": "hello"});
        let parsed = parse_provider_message(Some(&v)).expect("ok");
        assert_eq!(parsed.message.role(), "user");
        assert_eq!(parsed.message.content(), Some("hello"));
        assert!(parsed.message.tool_calls().is_empty());
        assert!(parsed.tool_call_failures.is_empty());
    }

    #[test]
    fn parse_provider_message_round_trips_tool_role() {
        let v = serde_json::json!({
            "role": "tool",
            "content": "ok",
            "tool_call_id": "call_1",
            "name": "read_file",
        });
        let parsed = parse_provider_message(Some(&v)).expect("ok");
        assert_eq!(parsed.message.role(), "tool");
        assert_eq!(parsed.message.tool_call_id(), Some("call_1"));
        match &parsed.message {
            Message::Tool { name, .. } => assert_eq!(name.as_deref(), Some("read_file")),
            _ => panic!("expected Tool variant"),
        }
        assert!(parsed.tool_call_failures.is_empty());
    }

    #[test]
    fn parse_provider_message_rejects_non_object() {
        let v = serde_json::json!(42);
        assert!(parse_provider_message(Some(&v)).is_err());
    }

    #[test]
    fn parse_provider_message_surfaces_malformed_tool_call_with_id() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [
                {
                    "id": "call_good",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"/x\"}"}
                },
                {
                    "id": "call_bad",
                    "garbage": true
                }
            ]
        });
        let parsed = parse_provider_message(Some(&v)).expect("ok");
        assert_eq!(parsed.message.tool_calls().len(), 1);
        assert_eq!(parsed.message.tool_calls()[0].id, "call_good");
        assert_eq!(parsed.tool_call_failures.len(), 1);
        assert_eq!(parsed.tool_call_failures[0].id.as_deref(), Some("call_bad"));
        assert!(!parsed.tool_call_failures[0].error.is_empty());
    }

    #[test]
    fn parse_provider_message_surfaces_malformed_tool_call_without_id() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": "hi",
            "tool_calls": [{"no_id": true}]
        });
        let parsed = parse_provider_message(Some(&v)).expect("ok");
        assert!(parsed.message.tool_calls().is_empty());
        assert_eq!(parsed.tool_call_failures.len(), 1);
        assert!(parsed.tool_call_failures[0].id.is_none());
    }

    #[test]
    fn read_chat_id_returns_some_for_non_empty() {
        let mut body = Map::new();
        body.insert("chat_id".into(), Value::String("abc".into()));
        assert_eq!(read_chat_id(&body), Some(ChatId::new("abc")));
    }

    #[test]
    fn read_chat_id_returns_none_for_empty_or_missing() {
        let mut body = Map::new();
        body.insert("chat_id".into(), Value::String(String::new()));
        assert_eq!(read_chat_id(&body), None);
        let body2 = Map::new();
        assert_eq!(read_chat_id(&body2), None);
    }

    #[test]
    fn tool_call_buffer_accumulates_args_in_order() {
        let mut b = ToolCallBuffer::default();
        b.on_item_added(
            "call_1".into(),
            "call_1".into(),
            "read_file".into(),
            String::new(),
        );
        b.on_args_delta(Some("call_1"), r#"{"pa"#);
        b.on_args_delta(Some("call_1"), r#"th":"/x"}"#);
        b.on_item_done(Some("call_1"), r#"{"path":"/x"}"#);
        let calls = b.into_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, r#"{"path":"/x"}"#);
    }

    #[test]
    fn tool_call_buffer_prefers_done_args_when_longer() {
        // Some models skip the deltas and send the full args on done.
        let mut b = ToolCallBuffer::default();
        b.on_item_added("call_1".into(), "call_1".into(), "x".into(), String::new());
        b.on_item_done(Some("call_1"), r#"{"a":1}"#);
        let calls = b.into_tool_calls();
        assert_eq!(calls[0].function.arguments, r#"{"a":1}"#);
    }

    #[test]
    fn body_signals_reasoning_unsupported_matches_real_400() {
        let body = r#"{
  "error": {
    "message": "Unsupported parameter: 'reasoning.summary' is not supported with the 'gpt-5.3-codex-spark' model.",
    "type": "invalid_request_error",
    "param": "reasoning.summary",
    "code": null
  }
}"#;
        assert!(body_signals_reasoning_unsupported(body));
    }

    #[test]
    fn body_signals_reasoning_unsupported_matches_future_subfields() {
        // Defensive: if the backend ever flags another reasoning.*
        // subfield as unsupported, the same fallback fires.
        let body = r#"{"error":{"message":"Unsupported parameter: 'reasoning.effort' is not supported","type":"invalid_request_error"}}"#;
        assert!(body_signals_reasoning_unsupported(body));
    }

    #[test]
    fn body_signals_reasoning_unsupported_ignores_unrelated_400() {
        let unrelated = r#"{"error":{"message":"No tool call found for function call output with call_id call_X"}}"#;
        assert!(!body_signals_reasoning_unsupported(unrelated));

        let model_error = r#"{"detail":"The 'gpt-5-codex' model is not supported when using Codex with a ChatGPT account."}"#;
        assert!(!body_signals_reasoning_unsupported(model_error));
    }

    #[tokio::test]
    async fn chats_track_reasoning_unsupported_per_model() {
        let chats = Chats::with_default_model(None);
        assert!(
            !chats
                .model_reasoning_unsupported("gpt-5.3-codex-spark")
                .await
        );
        chats
            .mark_model_reasoning_unsupported("gpt-5.3-codex-spark")
            .await;
        assert!(
            chats
                .model_reasoning_unsupported("gpt-5.3-codex-spark")
                .await
        );
        // Per-model: other models are unaffected.
        assert!(!chats.model_reasoning_unsupported("gpt-5.5").await);
    }
}
