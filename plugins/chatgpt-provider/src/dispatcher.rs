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

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody};
use rand::Rng;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::auth::{
    AuthSnapshot, AuthState, AuthStore, LoginLease, LoginStartOutcome, LogoutOutcome,
};
use crate::broker::{ToolBroker, ToolResult};
use crate::catalog::ToolCatalog;
use crate::config::ServeArgs;
use crate::error::ChatgptError;
use crate::responses::request::{
    Reasoning, ReasoningEffort, ReasoningSummary, ResponseItem, ResponsesApiRequest, TextControls,
};
use crate::responses::stream::{ResponseEvent, ResponseStream};
use crate::responses::{
    is_transient_status, response_signals_terminal_quota_denial, ModelEntry, ResponsesClient,
    ResponsesTurnContext, UsageSnapshot,
};
use crate::state::{
    ChatConfiguration, ChatId, ChatStats, Chats, ChatsError, HistoryEntry, Message, MessageRestore,
    ServiceTier, ToolCall, ToolCallFunction, TurnToken,
};
use crate::translator;
use crate::web::{SearchRequest, WebClient, WebCommand};
use nefor_plugin_sdk::TransportError;

pub const PROTOCOL_VERSION: &str = "0.1";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Cap on tool-call iterations per turn. The model can loop forever
/// asking for tools; this prevents runaways.
pub const TOOL_LOOP_MAX_ITERATIONS: u32 = 20;

/// Provider attempts remain provisional until a semantic terminal event. A
/// transient failure may therefore discard the whole attempt and replay it,
/// including after text, reasoning, native output, or tool-call fragments.
/// Production recovery is unbounded; tests may install explicit bounds.
const PRE_OUTPUT_RETRY_BASE_DELAY_MS: u64 = 200;
const PRE_OUTPUT_RETRY_MAX_DELAY_MS: u64 = 30_000;

const LOGOUT_REFUSED_ENV_MESSAGE: &str =
    "no login to revoke — credentials come from the environment; restart the plugin without it";

const HTTP_401_MESSAGE: &str = "auth failed (HTTP 401) — re-login via chatgpt-provider login";

const USAGE_POLL_INTERVAL_SECS: u64 = 5 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Auth401Action {
    RetryReloaded,
    ForceRefresh,
    Fail,
}

fn auth_401_action(stage: u8, disk_credentials_adopted: bool) -> Auth401Action {
    if stage == 0 && disk_credentials_adopted {
        Auth401Action::RetryReloaded
    } else if stage < 2 {
        Auth401Action::ForceRefresh
    } else {
        Auth401Action::Fail
    }
}

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

fn usage_updated_body(
    args: &ServeArgs,
    snapshot: &UsageSnapshot,
) -> Result<Map<String, Value>, ChatgptError> {
    let Value::Object(fields) = serde_json::to_value(snapshot)? else {
        return Err(ChatgptError::ResponsesStreamParse(
            "usage snapshot did not serialize as an object".into(),
        ));
    };
    Ok(make_event(
        format!("{}usage.updated", args.event_prefix()),
        fields,
    ))
}

fn usage_error_body(args: &ServeArgs, message: &str) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("message".into(), Value::String(message.to_owned()));
    make_event(format!("{}usage.error", args.event_prefix()), fields)
}

async fn handle_refresh_error(
    ctx: &DispatcherContext,
    chat_id: Option<&ChatId>,
    error: ChatgptError,
) -> Result<(), ChatgptError> {
    match error {
        ChatgptError::Http(e) => {
            let snap = ctx.auth.snapshot().await;
            send_event(&ctx.out_tx, auth_status_body(&ctx.args, &snap)).await?;
            send_event(
                &ctx.out_tx,
                turn_error_body(
                    &ctx.args,
                    chat_id,
                    &format!("token refresh failed: HTTP transport error: {e}"),
                ),
            )
            .await
        }
        ChatgptError::RefreshTransient(message) => {
            let snap = ctx.auth.snapshot().await;
            send_event(&ctx.out_tx, auth_status_body(&ctx.args, &snap)).await?;
            send_event(
                &ctx.out_tx,
                turn_error_body(
                    &ctx.args,
                    chat_id,
                    &format!("token refresh temporarily unavailable: {message}"),
                ),
            )
            .await
        }
        e @ ChatgptError::RefreshFailed(_) => {
            let snap = ctx.auth.apply_error(format!("refresh: {e}")).await;
            send_event(&ctx.out_tx, auth_status_body(&ctx.args, &snap)).await?;
            send_event(
                &ctx.out_tx,
                turn_error_body(&ctx.args, chat_id, &format!("token refresh failed: {e}")),
            )
            .await
        }
        e => {
            let snap = ctx.auth.snapshot().await;
            send_event(&ctx.out_tx, auth_status_body(&ctx.args, &snap)).await?;
            send_event(
                &ctx.out_tx,
                turn_error_body(&ctx.args, chat_id, &format!("token refresh failed: {e}")),
            )
            .await
        }
    }
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

fn stream_tool_call_delta_body(
    prefix: &str,
    chat_id: &ChatId,
    item_id: Option<&str>,
    fragment: &str,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    if let Some(item_id) = item_id {
        m.insert("tool_call_id".into(), Value::String(item_id.to_owned()));
    }
    m.insert("fragment".into(), Value::String(fragment.to_owned()));
    make_event(format!("{prefix}stream.tool_call_delta"), m)
}

fn stream_retry_body(
    args: &ServeArgs,
    chat_id: &ChatId,
    attempt: u32,
    delay: Duration,
    failure: &StreamFailure,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("attempt".into(), Value::Number(attempt.into()));
    m.insert("retry".into(), Value::Bool(true));
    m.insert(
        "retry_reason".into(),
        Value::String(
            RetryDecisionReason::RetryableProvisionalFailure
                .as_str()
                .into(),
        ),
    );
    m.insert(
        "failure_kind".into(),
        Value::String(failure.kind.as_str().into()),
    );
    m.insert(
        "delay_ms".into(),
        Value::Number((delay.as_millis() as u64).into()),
    );
    m.insert("message".into(), Value::String(failure.message.clone()));
    make_event(format!("{}stream.retry", args.event_prefix()), m)
}

fn retry_decision_body(
    args: &ServeArgs,
    request_id: &str,
    attempt: u32,
    delay: Duration,
    failure: &StreamFailure,
    decision: RetryDecision,
    state: StreamAttemptState,
) -> Map<String, Value> {
    completion_event_body(
        args,
        request_id,
        "retry_decision",
        [
            ("attempt", Value::Number(attempt.into())),
            ("retry", Value::Bool(decision.retry)),
            (
                "retry_reason",
                Value::String(decision.reason.as_str().into()),
            ),
            ("failure_kind", Value::String(failure.kind.as_str().into())),
            ("delay_ms", Value::Number((delay.as_millis() as u64).into())),
            (
                "terminal_event_seen",
                Value::Bool(state.terminal_event_seen),
            ),
            (
                "visible_state_observed",
                Value::Bool(state.visible_state_observed),
            ),
            (
                "native_output_state_observed",
                Value::Bool(state.native_output_state_observed),
            ),
            (
                "tool_state_observed",
                Value::Bool(state.tool_state_observed),
            ),
            ("error", Value::String(failure.message.clone())),
        ],
    )
}

fn attempt_discarded_body(
    args: &ServeArgs,
    request_id: &str,
    attempt: u32,
    failure: &StreamFailure,
    state: StreamAttemptState,
) -> Map<String, Value> {
    completion_event_body(
        args,
        request_id,
        "attempt_discarded",
        [
            ("attempt", Value::Number(attempt.into())),
            ("failure_kind", Value::String(failure.kind.as_str().into())),
            ("message", Value::String(failure.message.clone())),
            (
                "visible_state_observed",
                Value::Bool(state.visible_state_observed),
            ),
            (
                "native_output_state_observed",
                Value::Bool(state.native_output_state_observed),
            ),
            (
                "tool_state_observed",
                Value::Bool(state.tool_state_observed),
            ),
        ],
    )
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

/// Provider-owned context metadata for models whose `/models` entry omits it.
///
/// These values mirror Codex's bundled model catalog. Exact slugs are
/// intentional: a newly introduced model is not assumed to share a family
/// window until either the backend or this metadata says so.
fn known_context_window(slug: &str) -> Option<u64> {
    match slug {
        "gpt-5.6-sol" | "gpt-5.5" | "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.3-codex"
        | "gpt-5.2-codex" | "gpt-5.2" | "gpt-5.1-codex-max" | "gpt-5.1-codex" | "gpt-5.1"
        | "gpt-5-codex" | "gpt-5" | "gpt-5.1-codex-mini" | "gpt-5-codex-mini"
        | "codex-auto-review" => Some(272_000),
        "gpt-oss-120b" | "gpt-oss-20b" => Some(128_000),
        _ => None,
    }
}

fn provider_context_window(model: &ModelEntry) -> Option<u64> {
    model
        .context_length
        .or_else(|| known_context_window(&model.slug))
}

fn models_listed_body(args: &ServeArgs, models: &[ModelEntry]) -> Map<String, Value> {
    // A listed model is selectable by generic Lua, which requires a context
    // maximum. Keep entries with authoritative upstream metadata or an exact
    // provider-owned fallback; withhold unknown incomplete entries rather than
    // publishing an invented limit.
    let resolved: Vec<(&ModelEntry, u64)> = models
        .iter()
        .filter_map(|model| provider_context_window(model).map(|window| (model, window)))
        .collect();
    let arr: Vec<Value> = resolved
        .iter()
        .map(|(model, _)| Value::String(model.slug.clone()))
        .collect();
    let mut m = Map::new();
    m.insert("models".into(), Value::Array(arr));
    let ctx_map: Map<String, Value> = resolved
        .iter()
        .map(|(model, window)| (model.slug.clone(), Value::Number((*window).into())))
        .collect();
    if !ctx_map.is_empty() {
        m.insert("context_windows".into(), Value::Object(ctx_map));
    }
    let caps: Map<String, Value> = models
        .iter()
        .filter(|me| me.supports_reasoning_summaries)
        .map(|me| {
            let levels = ["none", "minimal", "low", "medium", "high", "xhigh"]
                .iter()
                .map(|s| Value::String((*s).to_owned()))
                .collect::<Vec<_>>();
            let mut reasoning = Map::new();
            reasoning.insert("levels".into(), Value::Array(levels));
            reasoning.insert("default".into(), Value::String("medium".into()));
            let mut entry = Map::new();
            entry.insert("reasoning".into(), Value::Object(reasoning));
            (me.slug.clone(), Value::Object(entry))
        })
        .collect();
    if !caps.is_empty() {
        m.insert("model_capabilities".into(), Value::Object(caps));
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

fn reasoning_set_ack_body(
    args: &ServeArgs,
    effort: ReasoningEffort,
    chat_id: Option<&ChatId>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "effort".into(),
        Value::String(reasoning_effort_wire(effort).to_owned()),
    );
    if let Some(cid) = chat_id {
        m.insert("chat_id".into(), Value::String(cid.to_string()));
    }
    make_event(format!("{}reasoning.set_ack", args.event_prefix()), m)
}

#[allow(clippy::too_many_arguments)]
fn chat_complete_result_body(
    args: &ServeArgs,
    chat_id: &ChatId,
    text: &str,
    tool_calls: &[ToolCall],
    finish_reason: Option<&str>,
    error: Option<&str>,
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
    // The turn's failure detail, threaded so a consumer of the single terminal
    // result (the mag capability bridge) sees WHY a finish_reason "error"
    // round died without also tracking the separate turn.error/chat.error
    // events. Carried in the output (the ProviderInput consumers read) and
    // top-level (envelope-level symmetry with finish_reason).
    if let Some(e) = error {
        output.insert("error".into(), Value::String(e.to_owned()));
    }
    let mut m = Map::new();
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("output".into(), Value::Object(output));
    if let Some(r) = finish_reason {
        m.insert("finish_reason".into(), Value::String(r.to_owned()));
    }
    if let Some(e) = error {
        m.insert("error".into(), Value::String(e.to_owned()));
    }
    make_event(format!("{}chat.complete.result", args.event_prefix()), m)
}

fn chat_compaction_commit_body(
    args: &ServeArgs,
    chat_id: &ChatId,
    model: &str,
    trigger: &str,
    before_items: usize,
    after_items: usize,
    compacted_items: &[ResponseItem],
) -> Map<String, Value> {
    let mut artifact = Map::new();
    artifact.insert("kind".into(), Value::String("responses.compaction".into()));
    artifact.insert("opaque".into(), Value::Bool(true));
    artifact.insert(
        "item_count".into(),
        Value::Number((after_items as u64).into()),
    );
    artifact.insert(
        "items".into(),
        serde_json::to_value(compacted_items).unwrap_or(Value::Array(Vec::new())),
    );
    let encrypted = compacted_items.iter().any(|item| match item {
        ResponseItem::Compaction { .. } => true,
        ResponseItem::ContextCompaction {
            encrypted_content: Some(_),
            ..
        } => true,
        ResponseItem::CompactionSummary {
            encrypted_content, ..
        } => encrypted_content.is_some(),
        _ => false,
    });
    artifact.insert("encrypted".into(), Value::Bool(encrypted));
    let summary_text = compaction_summary_text(compacted_items);

    let mut metadata = Map::new();
    metadata.insert(
        "before_items".into(),
        Value::Number((before_items as u64).into()),
    );
    metadata.insert(
        "after_items".into(),
        Value::Number((after_items as u64).into()),
    );
    metadata.insert(
        "retained_messages".into(),
        Value::Number((after_items as u64).into()),
    );
    metadata.insert(
        "has_summary".into(),
        Value::Bool(summary_text.as_deref().is_some_and(|s| !s.is_empty())),
    );

    let mut m = Map::new();
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("strategy".into(), Value::String("responses-compact".into()));
    m.insert("trigger".into(), Value::String(trigger.to_owned()));
    m.insert("model".into(), Value::String(model.to_owned()));
    m.insert(
        "display_summary".into(),
        Value::String(summary_text.unwrap_or_else(|| {
            format!(
                "Native compaction installed: {before_items} history items sealed into {after_items} model-context items."
            )
        })),
    );
    m.insert("model_context_artifact".into(), Value::Object(artifact));
    m.insert("metadata".into(), Value::Object(metadata));
    make_event(format!("{}chat.compaction.commit", args.event_prefix()), m)
}

fn compaction_summary_text(items: &[ResponseItem]) -> Option<String> {
    for item in items.iter().rev() {
        let ResponseItem::CompactionSummary { text, summary, .. } = item else {
            continue;
        };
        if let Some(text) = non_empty_trimmed(text.as_deref()) {
            return Some(text.to_owned());
        }
        if let Some(text) = summary.as_ref().and_then(text_from_summary_value) {
            return Some(text);
        }
    }
    None
}

fn text_from_summary_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => non_empty_trimmed(Some(s)).map(str::to_owned),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .and_then(|s| non_empty_trimmed(Some(s)))
                })
                .collect::<Vec<_>>()
                .join("\n");
            non_empty_trimmed(Some(&text)).map(str::to_owned)
        }
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .and_then(|s| non_empty_trimmed(Some(s)).map(str::to_owned)),
        _ => None,
    }
}

fn non_empty_trimmed(value: Option<&str>) -> Option<&str> {
    let trimmed = value?.trim();
    (!trimmed.is_empty()).then_some(trimmed)
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
        spawn_usage_fetch(
            args.clone(),
            auth.clone(),
            responses_client.clone(),
            out_tx.clone(),
            false,
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
    lease: LoginLease,
) {
    tokio::spawn(async move {
        let result = crate::auth::oauth::run_login_without_persisting(true).await;
        match result {
            Ok(td) => {
                match auth.apply_login_result(lease, td).await {
                    Ok(true) => {}
                    Ok(false) => return,
                    Err(e) => {
                        if let Some(snap) = auth
                            .apply_login_error(lease, format!("apply login: {e}"))
                            .await
                        {
                            let _ = send_event(&out_tx, auth_status_body(&args, &snap)).await;
                        }
                        return;
                    }
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
                    spawn_usage_fetch(
                        args.clone(),
                        auth.clone(),
                        responses_client.clone(),
                        out_tx.clone(),
                        false,
                    );
                }
            }
            Err(e) => {
                if let Some(snap) = auth.apply_login_error(lease, format!("login: {e}")).await {
                    let _ = send_event(&out_tx, auth_status_body(&args, &snap)).await;
                }
            }
        }
    });
}

async fn start_login_flow(
    args: Arc<ServeArgs>,
    auth: Arc<AuthStore>,
    chats: Arc<Chats>,
    responses_client: Arc<ResponsesClient>,
    out_tx: mpsc::Sender<PluginOutgoing>,
) -> Result<(), ChatgptError> {
    let outcome = auth.begin_login().await;
    let snap = auth.snapshot().await;
    send_event(&out_tx, auth_status_body(&args, &snap)).await?;
    if let LoginStartOutcome::Started(lease) = outcome {
        spawn_login_flow(args, auth, chats, responses_client, out_tx, lease);
    }
    Ok(())
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
                                supports_parallel_tool_calls: m.supports_parallel_tool_calls,
                            },
                        )
                    }))
                    .await;
                chats
                    .record_model_context_windows(models.iter().filter_map(|model| {
                        provider_context_window(model).map(|window| (model.slug.clone(), window))
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

fn spawn_usage_fetch(
    args: Arc<ServeArgs>,
    auth: Arc<AuthStore>,
    responses_client: Arc<ResponsesClient>,
    out_tx: mpsc::Sender<PluginOutgoing>,
    report_errors: bool,
) {
    tokio::spawn(async move {
        let snap = auth.snapshot().await;
        if !matches!(snap.state, AuthState::Connected) {
            if report_errors {
                let _ =
                    send_event(&out_tx, usage_error_body(&args, "ChatGPT login required")).await;
            }
            return;
        }
        if let Err(error) = auth.current_access_token().await {
            if report_errors {
                let _ = send_event(&out_tx, usage_error_body(&args, &error.to_string())).await;
            }
            return;
        }
        let snap = auth.snapshot().await;
        match responses_client.usage(&snap).await {
            Ok(usage) => match usage_updated_body(&args, &usage) {
                Ok(body) => {
                    let _ = send_event(&out_tx, body).await;
                }
                Err(error) => tracing::warn!(%error, "could not encode usage snapshot"),
            },
            Err(error) => {
                tracing::warn!(%error, "usage fetch failed");
                if report_errors {
                    let _ = send_event(&out_tx, usage_error_body(&args, &error.to_string())).await;
                }
            }
        }
    });
}

#[derive(Clone)]
struct DirectCompletion {
    owner: u64,
    chats: Arc<Chats>,
    chat_id: ChatId,
}

#[derive(Default)]
struct DirectCompletions {
    runs: Mutex<HashMap<String, DirectCompletion>>,
    next_owner: AtomicU64,
}

impl DirectCompletions {
    async fn begin(
        &self,
        request_id: String,
        chats: Arc<Chats>,
        chat_id: ChatId,
    ) -> Result<DirectCompletion, String> {
        let mut runs = self.runs.lock().await;
        if runs.contains_key(&request_id) {
            return Err(format!(
                "completion request `{request_id}` is already in flight"
            ));
        }
        let run = DirectCompletion {
            owner: self.next_owner.fetch_add(1, Ordering::Relaxed),
            chats,
            chat_id,
        };
        runs.insert(request_id, run.clone());
        Ok(run)
    }

    #[cfg(test)]
    async fn get(&self, request_id: &str) -> Option<DirectCompletion> {
        self.runs.lock().await.get(request_id).cloned()
    }

    async fn cancel(&self, request_id: &str) -> Option<DirectCompletion> {
        self.runs.lock().await.remove(request_id)
    }

    async fn finish(&self, request_id: &str, owner: u64) -> bool {
        let mut runs = self.runs.lock().await;
        if runs.get(request_id).is_some_and(|run| run.owner == owner) {
            runs.remove(request_id);
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WebInvocationProvenance {
    provider: String,
    model: String,
    actor_id: String,
    capability_id: String,
    logical_scope: String,
}

#[derive(Clone)]
struct WebExecution {
    owner: u64,
    cancellation: CancellationToken,
    provenance: WebInvocationProvenance,
}

#[derive(Default)]
struct WebExecutions {
    runs: Mutex<HashMap<String, WebExecution>>,
    next_owner: AtomicU64,
}

impl WebExecutions {
    async fn begin(
        &self,
        id: String,
        provenance: WebInvocationProvenance,
    ) -> Result<WebExecution, String> {
        let mut runs = self.runs.lock().await;
        if runs.contains_key(&id) {
            return Err(format!("web request `{id}` is already in flight"));
        }
        let execution = WebExecution {
            owner: self.next_owner.fetch_add(1, Ordering::Relaxed),
            cancellation: CancellationToken::new(),
            provenance,
        };
        runs.insert(id, execution.clone());
        Ok(execution)
    }

    async fn cancel(&self, id: &str, provenance: &WebInvocationProvenance) -> bool {
        let mut runs = self.runs.lock().await;
        if !runs
            .get(id)
            .is_some_and(|execution| &execution.provenance == provenance)
        {
            return false;
        }
        let Some(execution) = runs.remove(id) else {
            return false;
        };
        execution.cancellation.cancel();
        true
    }

    async fn finish(&self, id: &str, owner: u64) -> bool {
        let mut runs = self.runs.lock().await;
        if runs
            .get(id)
            .is_some_and(|execution| execution.owner == owner)
        {
            runs.remove(id);
            true
        } else {
            false
        }
    }

    async fn cancel_all(&self) {
        let runs = std::mem::take(&mut *self.runs.lock().await);
        for execution in runs.into_values() {
            execution.cancellation.cancel();
        }
    }
}

/// Shared state threaded through every dispatch handler. Bundles the
/// shared dependencies that every event path needs so function signatures
/// stay short.
#[derive(Clone)]
pub struct DispatcherContext {
    pub args: Arc<ServeArgs>,
    pub chats: Arc<Chats>,
    pub auth: Arc<AuthStore>,
    pub catalog: Arc<ToolCatalog>,
    pub broker: Arc<ToolBroker>,
    pub responses_client: Arc<ResponsesClient>,
    pub web_client: Arc<WebClient>,
    pub out_tx: mpsc::Sender<PluginOutgoing>,
    stream_retry_policy: StreamRetryPolicy,
    recovery_gate: Arc<ProviderRecoveryGate>,
    direct_completions: Arc<DirectCompletions>,
    web_executions: Arc<WebExecutions>,
}

impl DispatcherContext {
    pub fn new(
        args: Arc<ServeArgs>,
        chats: Arc<Chats>,
        auth: Arc<AuthStore>,
        catalog: Arc<ToolCatalog>,
        broker: Arc<ToolBroker>,
        responses_client: Arc<ResponsesClient>,
        out_tx: mpsc::Sender<PluginOutgoing>,
    ) -> Self {
        let stream_retry_budget = args.stream_retry_timeout_seconds.map(Duration::from_secs);
        let web_client = Arc::new(WebClient::with_http(
            responses_client.http_client(),
            responses_client.base_url().to_owned(),
            responses_client.installation_id().to_owned(),
            responses_client.originator().to_owned(),
        ));
        Self {
            args,
            chats,
            auth,
            catalog,
            broker,
            responses_client,
            web_client,
            out_tx,
            stream_retry_policy: StreamRetryPolicy {
                budget: stream_retry_budget,
                ..StreamRetryPolicy::default()
            },
            recovery_gate: Arc::new(ProviderRecoveryGate::default()),
            direct_completions: Arc::new(DirectCompletions::default()),
            web_executions: Arc::new(WebExecutions::default()),
        }
    }

    /// Install an elapsed recovery bound for transport-boundary tests.
    pub fn with_pre_output_retry_budget(mut self, budget: Duration) -> Self {
        self.stream_retry_policy.budget = Some(budget);
        self
    }

    /// Install an attempt bound for transport-boundary tests.
    pub fn with_pre_output_retry_max_attempts(mut self, max_attempts: u32) -> Self {
        self.stream_retry_policy.max_attempts = Some(max_attempts);
        self
    }
}

/// Top-level event loop. Returns on `Body::System(Shutdown)`, stdin
/// close, or ctrl-c. The caller emits goodbye after we return.
pub async fn run_dispatch_loop(
    ctx: DispatcherContext,
    mut in_rx: mpsc::Receiver<Result<Envelope, TransportError>>,
) -> Result<(), ChatgptError> {
    let first_usage_poll =
        tokio::time::Instant::now() + std::time::Duration::from_secs(USAGE_POLL_INTERVAL_SECS);
    let mut usage_interval = tokio::time::interval_at(
        first_usage_poll,
        std::time::Duration::from_secs(USAGE_POLL_INTERVAL_SECS),
    );
    usage_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            maybe = in_rx.recv() => {
                match maybe {
                    Some(Ok(env)) => match &env.body {
                        Body::System(SystemBody::Shutdown { .. }) => {
                            tracing::info!("shutdown received");
                            ctx.chats.interrupt_all().await;
                            ctx.web_executions.cancel_all().await;
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
                        ctx.web_executions.cancel_all().await;
                        return Ok(());
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("ctrl-c; exiting");
                ctx.chats.interrupt_all().await;
                ctx.web_executions.cancel_all().await;
                return Ok(());
            }
            _ = usage_interval.tick() => {
                spawn_usage_fetch(
                    ctx.args.clone(),
                    ctx.auth.clone(),
                    ctx.responses_client.clone(),
                    ctx.out_tx.clone(),
                    false,
                );
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
            let output = body.get("output").map(tool_output_for_text_model);
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
        "web.request" => handle_web_request(ctx, from, body).await,
        "web.cancel" => handle_web_cancel(ctx, from, body).await,
        "completion.request" => handle_completion_request(ctx, body).await,
        "completion.cancel" => {
            if let Some(request_id) = read_request_id(body) {
                if let Some(owned) = ctx.direct_completions.cancel(&request_id).await {
                    if !owned.chats.cancel_turn(&owned.chat_id).await {
                        tracing::debug!(%request_id, "completion.cancel for finished request; no-op");
                    }
                } else {
                    tracing::debug!(%request_id, "completion.cancel for unknown request; no-op");
                }
            }
            Ok(())
        }
        "chat.create" => handle_chat_create(&ctx.args, &ctx.chats, &ctx.out_tx, body).await,
        "chat.restore" => handle_chat_restore(&ctx.args, &ctx.chats, &ctx.out_tx, body).await,
        "chat.append" => handle_chat_append(&ctx.args, &ctx.chats, &ctx.out_tx, body).await,
        "chat.complete" => handle_chat_complete(ctx, body).await,
        "chat.compact" => handle_chat_compact(ctx, body).await,
        "chat.compaction.restore" => {
            handle_chat_compaction_restore(&ctx.args, &ctx.chats, &ctx.out_tx, body).await
        }
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
        // Hard cancel of an in-flight completion, keyed by the caller's
        // request id (`chat_id` — one in-flight completion per chat).
        // Aborts the streaming HTTP call and suppresses the terminal
        // result. Idempotent: unknown or already-finished ids are a
        // logged no-op, never an error. Pure capability-surface
        // completion — this handler knows nothing about actors, runs, or
        // graphs.
        "chat.cancel" => {
            match read_chat_id(body) {
                Some(cid) => {
                    if !ctx.chats.cancel_turn(&cid).await {
                        tracing::debug!(
                            chat_id = %cid,
                            "chat.cancel for unknown or finished request; no-op"
                        );
                    }
                }
                None => {
                    tracing::debug!("chat.cancel without chat_id; no-op");
                }
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
                spawn_usage_fetch(
                    ctx.args.clone(),
                    ctx.auth.clone(),
                    ctx.responses_client.clone(),
                    ctx.out_tx.clone(),
                    false,
                );
            }
            Ok(())
        }
        "login_requested" => {
            start_login_flow(
                ctx.args.clone(),
                ctx.auth.clone(),
                ctx.chats.clone(),
                ctx.responses_client.clone(),
                ctx.out_tx.clone(),
            )
            .await
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
                Ok(models) => send_event(&ctx.out_tx, models_listed_body(&ctx.args, &models)).await,
                Err(e) => {
                    let msg = format!("failed to fetch /models: {e}");
                    tracing::warn!(error = %e, "models.list_requested failed");
                    send_event(&ctx.out_tx, turn_error_body(&ctx.args, None, &msg)).await
                }
            }
        }
        "usage.requested" => {
            spawn_usage_fetch(
                ctx.args.clone(),
                ctx.auth.clone(),
                ctx.responses_client.clone(),
                ctx.out_tx.clone(),
                true,
            );
            Ok(())
        }
        "model.set" => {
            let model = match body.get("model").and_then(Value::as_str) {
                Some(m) if !m.is_empty() => m.to_owned(),
                _ => {
                    tracing::warn!("model.set without non-empty model; ignoring");
                    return Ok(());
                }
            };
            let context_window = ctx
                .chats
                .model_context_window(&model)
                .await
                .or_else(|| known_context_window(&model));
            if context_window.is_none() {
                send_event(
                    &ctx.out_tx,
                    turn_error_body(
                        &ctx.args,
                        read_chat_id(body).as_ref(),
                        &format!(
                            "model {model:?} has no known context window and cannot be selected"
                        ),
                    ),
                )
                .await?;
                return Ok(());
            }
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
                start_login_flow(
                    ctx.args.clone(),
                    ctx.auth.clone(),
                    ctx.chats.clone(),
                    ctx.responses_client.clone(),
                    ctx.out_tx.clone(),
                )
                .await?;
            }
            Ok(())
        }
        "reasoning.set" => {
            let effort = match parse_reasoning_effort(
                body.get("effort").or_else(|| body.get("reasoning_effort")),
            ) {
                Ok(Some(v)) => v,
                Ok(None) => {
                    tracing::warn!("reasoning.set without non-empty effort; ignoring");
                    return Ok(());
                }
                Err(e) => {
                    send_event(
                        &ctx.out_tx,
                        turn_error_body(&ctx.args, read_chat_id(body).as_ref(), &e),
                    )
                    .await?;
                    return Ok(());
                }
            };
            let chat_id = read_chat_id(body);
            if let Some(cid) = &chat_id {
                if ctx.chats.exists(cid).await {
                    let _ = ctx.chats.set_chat_reasoning_effort(cid, effort).await;
                }
            }
            send_event(
                &ctx.out_tx,
                reasoning_set_ack_body(&ctx.args, effort, chat_id.as_ref()),
            )
            .await
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------
// Per-event handlers.
// ---------------------------------------------------------------------

fn read_request_id(body: &Map<String, Value>) -> Option<String> {
    body.get("request_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn read_chat_id(body: &Map<String, Value>) -> Option<ChatId> {
    body.get("chat_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ChatId::new)
}

fn read_conversation_id(body: &Map<String, Value>) -> Option<String> {
    body.get("conversation_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn logical_routing_id(conversation_id: Option<&str>, chat_id: &ChatId) -> String {
    conversation_id
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| chat_id.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderRoutingIdentity {
    session_id: String,
    thread_id: String,
}

fn deterministic_provider_uuid(namespace: &[u8], logical_routing_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace);
    hasher.update([0]);
    hasher.update(logical_routing_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 9562 UUIDv8: application-defined payload with standard
    // version and variant bits. SHA-256 supplies the stable payload.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn provider_routing_identity(logical_routing_id: &str) -> ProviderRoutingIdentity {
    ProviderRoutingIdentity {
        session_id: deterministic_provider_uuid(
            b"nefor.chatgpt-provider.session.v1",
            logical_routing_id,
        ),
        thread_id: deterministic_provider_uuid(
            b"nefor.chatgpt-provider.thread.v1",
            logical_routing_id,
        ),
    }
}

fn reasoning_effort_wire(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None => "none",
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::XHigh => "xhigh",
    }
}

fn parse_reasoning_effort(value: Option<&Value>) -> Result<Option<ReasoningEffort>, String> {
    let Some(raw) = value.and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    match raw.to_ascii_lowercase().as_str() {
        "none" | "off" => Ok(Some(ReasoningEffort::None)),
        "minimal" => Ok(Some(ReasoningEffort::Minimal)),
        "low" => Ok(Some(ReasoningEffort::Low)),
        "medium" => Ok(Some(ReasoningEffort::Medium)),
        "high" => Ok(Some(ReasoningEffort::High)),
        "xhigh" | "x-high" => Ok(Some(ReasoningEffort::XHigh)),
        _ => Err(format!("unsupported reasoning effort `{raw}`")),
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum DirectCompletionTools {
    Disabled,
    Allowlist(Vec<String>),
}

#[derive(Debug, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CompletionProviderOptions {
    #[serde(default)]
    service_tier: Option<ServiceTier>,
}

fn parse_completion_provider_options(value: Option<&Value>) -> Result<Option<ServiceTier>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if !value.is_object() {
        return Err("`provider_options` must be an object".into());
    }
    if value.get("service_tier").is_some_and(Value::is_null) {
        return Err("`provider_options.service_tier` must be `fast` when present".into());
    }
    serde_json::from_value::<CompletionProviderOptions>(value.clone())
        .map(|options| options.service_tier)
        .map_err(|error| format!("invalid `provider_options`: {error}"))
}

fn parse_direct_completion_tools(value: Option<&Value>) -> Result<DirectCompletionTools, String> {
    let Some(value) = value else {
        // Direct completions are an untyped, single-shot boundary. Absence must not
        // implicitly expose the provider's process-wide catalog.
        return Ok(DirectCompletionTools::Disabled);
    };
    match value {
        Value::Bool(false) => Ok(DirectCompletionTools::Disabled),
        Value::Array(values) if values.is_empty() => Ok(DirectCompletionTools::Disabled),
        Value::Array(values) => {
            let mut names = Vec::with_capacity(values.len());
            for (index, value) in values.iter().enumerate() {
                let name = value.as_str().ok_or_else(|| {
                    format!(
                        "completion.request `tools[{index}]` must be a non-empty tool-name string"
                    )
                })?;
                let name = name.trim();
                if name.is_empty() {
                    return Err(format!(
                        "completion.request `tools[{index}]` must be a non-empty tool-name string"
                    ));
                }
                if names.iter().any(|known| known == name) {
                    return Err(format!(
                        "completion.request `tools` contains duplicate tool name `{name}`"
                    ));
                }
                names.push(name.to_owned());
            }
            Ok(DirectCompletionTools::Allowlist(names))
        }
        _ => {
            Err("completion.request `tools` must be false or an array of tool-name strings".into())
        }
    }
}

async fn project_direct_tool_specs(
    catalog: &ToolCatalog,
    names: &[String],
    direct_specs: Option<Vec<crate::catalog::ToolSpec>>,
) -> Vec<crate::catalog::ToolSpec> {
    let advertised = catalog.project_names(names).await;
    let direct_by_name = direct_specs
        .unwrap_or_default()
        .into_iter()
        .filter(|spec| names.iter().any(|name| name == &spec.name))
        .map(|spec| (spec.name.clone(), spec))
        .collect::<HashMap<_, _>>();
    let advertised_by_name = advertised
        .into_iter()
        .map(|spec| (spec.name.clone(), spec))
        .collect::<HashMap<_, _>>();

    names
        .iter()
        .filter_map(|name| {
            let advertised = advertised_by_name.get(name);
            if advertised.is_some_and(|spec| !spec.execution.is_routed()) {
                return advertised.cloned();
            }
            if let Some(spec) = direct_by_name
                .get(name)
                .filter(|spec| spec.execution.is_routed())
            {
                return Some(spec.clone());
            }
            advertised.cloned()
        })
        .collect()
}

fn completion_checkpoint(
    context: Option<&Value>,
    provider: &str,
    model: Option<&str>,
) -> Option<Vec<ResponseItem>> {
    let checkpoint = context?.get("compaction")?.get("checkpoint")?.as_object()?;
    if checkpoint.get("provider").and_then(Value::as_str) != Some(provider)
        || checkpoint.get("format").and_then(Value::as_str)
            != Some("chatgpt.responses.compaction.v1")
    {
        return None;
    }
    let checkpoint_model = checkpoint.get("model").and_then(Value::as_str);
    if checkpoint_model.is_some() && checkpoint_model != model {
        return None;
    }
    let items = checkpoint.get("artifact")?.get("items")?.clone();
    serde_json::from_value(items)
        .ok()
        .filter(|items: &Vec<ResponseItem>| !items.is_empty())
}

const RESPONSE_OUTPUT_CONTEXT_FORMAT: &str = "chatgpt.responses.output_items.v1";

fn message_provider_context_items(
    message: &Value,
    provider: &str,
    model: Option<&str>,
) -> Result<Option<Vec<ResponseItem>>, String> {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Ok(None);
    }
    let Some(context) = message.get("provider_context").and_then(Value::as_object) else {
        return Ok(None);
    };
    if context.get("provider").and_then(Value::as_str) != Some(provider)
        || context.get("format").and_then(Value::as_str) != Some(RESPONSE_OUTPUT_CONTEXT_FORMAT)
    {
        return Ok(None);
    }
    let context_model = context.get("model").and_then(Value::as_str);
    if context_model.is_some() && context_model != model {
        return Ok(None);
    }
    let items = context
        .get("artifact")
        .and_then(|artifact| artifact.get("items"))
        .cloned()
        .ok_or_else(|| "compatible provider_context is missing artifact.items".to_owned())?;
    let items: Vec<ResponseItem> = serde_json::from_value(items)
        .map_err(|error| format!("compatible provider_context has invalid items: {error}"))?;
    if items.is_empty() {
        return Err("compatible provider_context contains no output items".into());
    }
    Ok(Some(items))
}

fn response_output_context(provider: &str, model: &str, items: &[ResponseItem]) -> Value {
    serde_json::json!({
        "provider": provider,
        "format": RESPONSE_OUTPUT_CONTEXT_FORMAT,
        "model": model,
        "artifact": {
            "kind": "responses.output_items",
            "opaque": true,
            "items": items,
        },
    })
}

struct WebExecutionResult {
    response: crate::web::SearchResponse,
    semantic_error: Option<String>,
}

fn web_result_body(
    args: &ServeArgs,
    id: &str,
    result: Result<WebExecutionResult, String>,
) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("id".into(), Value::String(id.to_owned()));
    match result {
        Ok(result) => {
            let response = result.response;
            let mut output = Map::new();
            output.insert("text".into(), Value::String(response.output));
            if let Some(results) = response.results {
                output.insert("results".into(), Value::Array(results));
            }
            fields.insert("output".into(), Value::Object(output));
            if let Some(encrypted_output) = response.encrypted_output {
                fields.insert(
                    "provider_state".into(),
                    serde_json::json!({"encrypted_output": encrypted_output}),
                );
            }
            if let Some(error) = result.semantic_error {
                fields.insert("error".into(), Value::String(error));
            }
        }
        Err(error) => {
            fields.insert("error".into(), Value::String(error));
        }
    }
    make_event(format!("{}web.result", args.event_prefix()), fields)
}

fn nonempty_web_field<'a>(object: &'a Map<String, Value>, field: &str) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("web invocation provenance requires non-empty `{field}`"))
}

fn web_invocation_provenance(
    args: &ServeArgs,
    body: &Map<String, Value>,
) -> Result<WebInvocationProvenance, String> {
    let invocation = body
        .get("invocation")
        .and_then(Value::as_object)
        .ok_or_else(|| "web request requires invocation provenance".to_owned())?;
    let provider = nonempty_web_field(invocation, "provider")?;
    if provider != args.provider_name {
        return Err(format!(
            "web invocation provider `{provider}` does not match `{}`",
            args.provider_name
        ));
    }
    let model = nonempty_web_field(invocation, "model")?;
    let body_model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "web request is missing an acknowledged model".to_owned())?;
    if body_model != model {
        return Err("web request model does not match invocation provenance".into());
    }
    let actor_id = nonempty_web_field(invocation, "actor_id")?;
    let invoking_from = body
        .get("invoking_from")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "web request requires invoking actor identity".to_owned())?;
    if invoking_from != actor_id {
        return Err("web request invoking actor does not match invocation provenance".into());
    }
    let capability_id = nonempty_web_field(invocation, "capability_id")?;
    let caller_id = body
        .get("caller_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "web request requires gate caller correlation".to_owned())?;
    if caller_id != capability_id {
        return Err("web request caller correlation does not match capability provenance".into());
    }
    let inner_id = body
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "web request requires gate correlation id".to_owned())?;
    if inner_id == capability_id {
        return Err("web request gate correlation must be distinct from capability id".into());
    }
    let session_id = nonempty_web_field(invocation, "session_id")?;
    let _run_id = nonempty_web_field(invocation, "run_id")?;
    let run_scope = nonempty_web_field(invocation, "run_scope")?;
    if !capability_id.starts_with(&format!("{run_scope}/")) {
        return Err("web request capability id is outside its invocation scope".into());
    }
    let logical_scope = ["conversation_id", "root_conversation_id"]
        .into_iter()
        .find_map(|field| {
            invocation
                .get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{session_id}/{run_scope}"));

    Ok(WebInvocationProvenance {
        provider: provider.to_owned(),
        model: model.to_owned(),
        actor_id: actor_id.to_owned(),
        capability_id: capability_id.to_owned(),
        logical_scope,
    })
}

async fn send_web_error(
    ctx: &DispatcherContext,
    id: Option<&str>,
    error: impl Into<String>,
) -> Result<(), ChatgptError> {
    let error = error.into();
    let Some(id) = id.filter(|id| !id.is_empty()) else {
        tracing::warn!(%error, "web request missing correlation id; dropping");
        return Ok(());
    };
    send_event(&ctx.out_tx, web_result_body(&ctx.args, id, Err(error))).await
}

async fn handle_web_request(
    ctx: &DispatcherContext,
    from: &PluginName,
    body: &Map<String, Value>,
) -> Result<(), ChatgptError> {
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty());
    if from.as_str() != "engine" {
        return send_web_error(ctx, id, "web request requires engine origin").await;
    }
    let Some(id) = id.map(str::to_owned) else {
        tracing::warn!("web.request missing `id`; dropping");
        return Ok(());
    };
    let provenance = match web_invocation_provenance(&ctx.args, body) {
        Ok(provenance) => provenance,
        Err(error) => return send_web_error(ctx, Some(&id), error).await,
    };
    if let Some(error) = body.get("validation_error").and_then(Value::as_str) {
        return send_web_error(ctx, Some(&id), error.to_owned()).await;
    }
    let command = match body.get("commands") {
        Some(commands) => match WebCommand::from_commands(commands) {
            Ok(command) => command,
            Err(error) => return send_web_error(ctx, Some(&id), error).await,
        },
        None => {
            return send_web_error(ctx, Some(&id), "web request missing `commands`").await;
        }
    };
    if let Some(name) = body.get("name").and_then(Value::as_str) {
        if name != command.tool_name() {
            return send_web_error(
                ctx,
                Some(&id),
                format!(
                    "web request tool `{name}` does not match command `{}`",
                    command.tool_name()
                ),
            )
            .await;
        }
    }
    let model = provenance.model.clone();
    let session_id = provider_routing_identity(&provenance.logical_scope).session_id;
    let execution = match ctx.web_executions.begin(id.clone(), provenance).await {
        Ok(execution) => execution,
        Err(error) => return send_web_error(ctx, Some(&id), error).await,
    };
    let request = SearchRequest::new(session_id, model, command);
    let owned = ctx.clone();
    tokio::spawn(async move {
        let result = async {
            let auth = owned.auth.snapshot().await;
            if !matches!(auth.state, AuthState::Connected) {
                return Err("auth not connected; cannot execute web request".to_owned());
            }
            owned
                .auth
                .current_access_token()
                .await
                .map_err(|error| format!("web auth refresh failed: {error}"))?;
            let auth = owned.auth.snapshot().await;
            let response = owned
                .web_client
                .execute(&request, &auth, &execution.cancellation)
                .await
                .map_err(|error| error.to_string())?;
            let semantic_error = response
                .semantic_error_for(&request.commands)
                .map(str::to_owned);
            Ok(WebExecutionResult {
                response,
                semantic_error,
            })
        }
        .await;
        if owned.web_executions.finish(&id, execution.owner).await {
            let _ = send_event(&owned.out_tx, web_result_body(&owned.args, &id, result)).await;
        }
    });
    Ok(())
}

async fn handle_web_cancel(
    ctx: &DispatcherContext,
    from: &PluginName,
    body: &Map<String, Value>,
) -> Result<(), ChatgptError> {
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty());
    if from.as_str() != "engine" {
        tracing::warn!(
            origin = from.as_str(),
            "rejected web.cancel from non-engine origin"
        );
        return Ok(());
    }
    let Some(id) = id else {
        return Ok(());
    };
    let provenance = match web_invocation_provenance(&ctx.args, body) {
        Ok(provenance) => provenance,
        Err(error) => {
            tracing::warn!(%id, %error, "rejected web.cancel with invalid provenance");
            return Ok(());
        }
    };
    if ctx.web_executions.cancel(id, &provenance).await {
        send_event(
            &ctx.out_tx,
            web_result_body(&ctx.args, id, Err("web request cancelled".into())),
        )
        .await?;
    } else {
        tracing::debug!(%id, "web.cancel for unknown or settled request; no-op");
    }
    Ok(())
}

async fn handle_completion_request(
    ctx: &DispatcherContext,
    body: &Map<String, Value>,
) -> Result<(), ChatgptError> {
    let request_id = match read_request_id(body) {
        Some(id) => id,
        None => {
            send_completion_event(ctx, None, "failed", |event| {
                event.insert(
                    "error".into(),
                    Value::String("completion.request missing `request_id`".into()),
                );
            })
            .await?;
            return Ok(());
        }
    };
    let chat_id = ChatId::new(request_id.clone());
    let provider_options = match parse_completion_provider_options(body.get("provider_options")) {
        Ok(options) => options,
        Err(error) => {
            send_completion_event(ctx, Some(&request_id), "failed", |event| {
                event.insert("error".into(), Value::String(error));
            })
            .await?;
            return Ok(());
        }
    };
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let effective_model = match model.clone() {
        Some(model) => Some(model),
        None => ctx.chats.default_model().await,
    };
    let checkpoint = completion_checkpoint(
        body.get("conversation_context"),
        &ctx.args.provider_name,
        effective_model.as_deref(),
    );
    let message_values = body
        .get("conversation_context")
        .and_then(Value::as_object)
        .and_then(|context| {
            let field = if checkpoint.is_some() {
                "tail_messages"
            } else {
                "messages"
            };
            context.get(field).and_then(Value::as_array)
        })
        .or_else(|| body.get("messages").and_then(Value::as_array));
    let messages = match message_values {
        Some(messages) => messages,
        None => {
            send_completion_event(ctx, Some(&request_id), "failed", |event| {
                event.insert(
                    "error".into(),
                    Value::String("completion.request `messages` must be an array".into()),
                );
            })
            .await?;
            return Ok(());
        }
    };
    let mut history = Vec::with_capacity(messages.len());
    for message in messages {
        match message_provider_context_items(
            message,
            &ctx.args.provider_name,
            effective_model.as_deref(),
        ) {
            Ok(Some(items)) => {
                history.extend(items.into_iter().map(HistoryEntry::from));
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                send_completion_event(ctx, Some(&request_id), "failed", |event| {
                    event.insert("error".into(), Value::String(error));
                })
                .await?;
                return Ok(());
            }
        }
        match parse_provider_message(Some(message)) {
            Ok(parsed) if parsed.tool_call_failures.is_empty() => {
                history.push(parsed.message.into())
            }
            Ok(_) => {
                send_completion_event(ctx, Some(&request_id), "failed", |event| {
                    event.insert(
                        "error".into(),
                        Value::String("completion.request contains a malformed tool call".into()),
                    );
                })
                .await?;
                return Ok(());
            }
            Err(error) => {
                send_completion_event(ctx, Some(&request_id), "failed", |event| {
                    event.insert("error".into(), Value::String(error));
                })
                .await?;
                return Ok(());
            }
        }
    }
    let conversation_id = body
        .get("conversation_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let (tool_overrides, tool_allowlist) = match parse_direct_completion_tools(body.get("tools")) {
        Ok(DirectCompletionTools::Disabled) => (Some(Vec::new()), Some(Vec::new())),
        Ok(DirectCompletionTools::Allowlist(names)) => {
            let direct_specs = match body.get("tool_specs") {
                Some(value @ Value::Array(_)) => Some(ToolCatalog::parse_tools(value)),
                Some(_) => {
                    send_completion_event(ctx, Some(&request_id), "failed", |event| {
                        event.insert(
                            "error".into(),
                            Value::String(
                                "completion.request `tool_specs` must be an array".into(),
                            ),
                        );
                    })
                    .await?;
                    return Ok(());
                }
                None => None,
            };
            let advertised = ctx.catalog.project_names(&names).await;
            let conflicts = direct_specs.as_ref().is_some_and(|directs| {
                directs.iter().any(|direct| {
                    !direct.owner.is_empty()
                        && advertised
                            .iter()
                            .any(|spec| spec.name == direct.name && spec.owner != direct.owner)
                })
            });
            if conflicts {
                send_completion_event(ctx, Some(&request_id), "failed", |event| {
                    event.insert(
                        "error".into(),
                        Value::String(
                            "request-local tool owner conflicts with an advertised tool".into(),
                        ),
                    );
                })
                .await?;
                return Ok(());
            }
            let specs = project_direct_tool_specs(&ctx.catalog, &names, direct_specs).await;
            let projected_names = specs
                .iter()
                .map(|spec| spec.name.clone())
                .collect::<Vec<_>>();
            let missing = names
                .iter()
                .filter(|name| !projected_names.contains(name))
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                tracing::warn!(
                    request_id,
                    missing = ?missing,
                    "projected stale tool names out of completion request"
                );
            }
            (Some(specs), Some(projected_names))
        }
        Err(error) => {
            send_completion_event(ctx, Some(&request_id), "failed", |event| {
                event.insert("error".into(), Value::String(error));
            })
            .await?;
            return Ok(());
        }
    };
    let reasoning_effort = match parse_reasoning_effort(body.get("reasoning_effort")) {
        Ok(value) => value,
        Err(error) => {
            send_completion_event(ctx, Some(&request_id), "failed", |event| {
                event.insert("error".into(), Value::String(error));
            })
            .await?;
            return Ok(());
        }
    };
    let system = body
        .get("system")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let direct_chats = Arc::new(Chats::with_default_model(ctx.chats.default_model().await));
    if let Err(error) = direct_chats
        .restore_messages(MessageRestore {
            id: chat_id.clone(),
            model,
            conversation_id,
            system,
            tool_overrides,
            tool_allowlist,
            reasoning_effort,
            service_tier: provider_options,
            history,
        })
        .await
    {
        send_completion_event(ctx, Some(&request_id), "failed", |event| {
            event.insert("error".into(), Value::String(error.to_string()));
        })
        .await?;
        return Ok(());
    }
    if let Some(items) = checkpoint {
        if let Err(error) = direct_chats.prepend_native_history(&chat_id, items).await {
            send_completion_event(ctx, Some(&request_id), "failed", |event| {
                event.insert("error".into(), Value::String(error.to_string()));
            })
            .await?;
            return Ok(());
        }
    }
    let cancel = match direct_chats.begin_turn(&chat_id).await {
        Ok(cancel) => cancel,
        Err(error) => {
            send_completion_event(ctx, Some(&request_id), "failed", |event| {
                event.insert("error".into(), Value::String(error.to_string()));
            })
            .await?;
            return Ok(());
        }
    };
    let completion = match ctx
        .direct_completions
        .begin(request_id.clone(), direct_chats.clone(), chat_id.clone())
        .await
    {
        Ok(completion) => completion,
        Err(error) => {
            direct_chats.end_turn(&chat_id).await;
            send_completion_event(ctx, Some(&request_id), "failed", |event| {
                event.insert("error".into(), Value::String(error));
            })
            .await?;
            return Ok(());
        }
    };
    let mut direct_ctx = ctx.clone();
    direct_ctx.chats = direct_chats;
    spawn_turn(
        direct_ctx,
        chat_id,
        cancel,
        body.get("output_schema").cloned(),
        Some((request_id, completion)),
    );
    Ok(())
}

fn completion_event_body<I, S>(
    args: &ServeArgs,
    request_id: &str,
    event: &str,
    fields: I,
) -> Map<String, Value>
where
    I: IntoIterator<Item = (S, Value)>,
    S: Into<String>,
{
    let mut body = Map::new();
    body.insert("request_id".into(), Value::String(request_id.to_owned()));
    body.insert("event".into(), Value::String(event.to_owned()));
    for (name, value) in fields {
        body.insert(name.into(), value);
    }
    make_event(format!("{}completion.event", args.event_prefix()), body)
}

async fn send_completion_event(
    ctx: &DispatcherContext,
    request_id: Option<&str>,
    event: &str,
    fill: impl FnOnce(&mut Map<String, Value>),
) -> Result<(), ChatgptError> {
    let mut body = Map::new();
    if let Some(request_id) = request_id {
        body.insert("request_id".into(), Value::String(request_id.to_owned()));
    }
    body.insert("event".into(), Value::String(event.to_owned()));
    fill(&mut body);
    send_event(
        &ctx.out_tx,
        make_event(format!("{}completion.event", ctx.args.event_prefix()), body),
    )
    .await
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
    let conversation_id = read_conversation_id(body);
    let system = body
        .get("system")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    // chat.create.tools wire shape (matches openai-provider):
    //   - false  → tools disabled (empty allowlist filters everything out)
    //   - [names] → allowlist of tool names (provider filters its catalog)
    //   - absent  → no filter (use the whole catalog)
    // Callers emit string arrays (the lead's :tools list from its
    // turn-program). We previously parsed `tools` as tool-spec
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
    let reasoning_effort = match parse_reasoning_effort(body.get("reasoning_effort")) {
        Ok(v) => v,
        Err(e) => {
            send_event(out_tx, chat_error_body(args, &chat_id, e)).await?;
            return Ok(());
        }
    };
    let provider_options = match parse_completion_provider_options(body.get("provider_options")) {
        Ok(options) => options,
        Err(error) => {
            send_event(out_tx, chat_error_body(args, &chat_id, error)).await?;
            return Ok(());
        }
    };

    match chats
        .recreate(
            chat_id.clone(),
            ChatConfiguration {
                model,
                conversation_id: None,
                system,
                tool_overrides,
                tool_allowlist,
                reasoning_effort,
                service_tier: provider_options,
            },
        )
        .await
    {
        Ok(()) => {
            if let Some(conversation_id) = conversation_id {
                if let Err(error) = chats.set_conversation_id(&chat_id, conversation_id).await {
                    send_event(out_tx, chat_error_body(args, &chat_id, error.to_string())).await?;
                    return Ok(());
                }
            }
            send_event(out_tx, chat_created_body(args, &chat_id)).await
        }
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

async fn handle_chat_restore(
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
                turn_error_body(args, None, "chat.restore missing `chat_id`"),
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
    let conversation_id = read_conversation_id(body);
    let system = body
        .get("system")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
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
    let reasoning_effort = match parse_reasoning_effort(body.get("reasoning_effort")) {
        Ok(v) => v,
        Err(e) => {
            send_event(out_tx, chat_error_body(args, &chat_id, e)).await?;
            return Ok(());
        }
    };
    let provider_options = match parse_completion_provider_options(body.get("provider_options")) {
        Ok(options) => options,
        Err(error) => {
            send_event(out_tx, chat_error_body(args, &chat_id, error)).await?;
            return Ok(());
        }
    };

    let mut history = Vec::new();
    if let Some(items) = body.get("history").and_then(Value::as_array) {
        for item in items {
            let parsed = match parse_provider_message(Some(item)) {
                Ok(m) => m,
                Err(msg) => {
                    send_event(out_tx, chat_error_body(args, &chat_id, msg)).await?;
                    return Ok(());
                }
            };
            history.push(parsed.message.into());
            for failure in &parsed.tool_call_failures {
                if let Some(id) = &failure.id {
                    history.push(
                        Message::tool_result(
                            id.clone(),
                            format!(
                                "Failed to parse tool call: {}. Raw: {}",
                                failure.error, failure.raw
                            ),
                        )
                        .into(),
                    );
                } else {
                    tracing::warn!(
                        error = %failure.error,
                        raw = %failure.raw,
                        "tool_call parse failed and no id to surface error to model",
                    );
                }
            }
        }
    }

    match chats
        .restore_messages(MessageRestore {
            id: chat_id.clone(),
            model,
            conversation_id,
            system,
            tool_overrides,
            tool_allowlist,
            reasoning_effort,
            service_tier: provider_options,
            history,
        })
        .await
    {
        Ok(()) => send_event(out_tx, chat_appended_body(args, &chat_id)).await,
        Err(e) => send_event(out_tx, chat_error_body(args, &chat_id, e.to_string())).await,
    }
}

async fn handle_chat_compaction_restore(
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
                turn_error_body(args, None, "chat.compaction.restore missing `chat_id`"),
            )
            .await?;
            return Ok(());
        }
    };
    let items_value = body
        .get("items")
        .cloned()
        .or_else(|| {
            body.get("model_context_artifact")
                .and_then(|v| v.get("items"))
                .cloned()
        })
        .unwrap_or(Value::Array(Vec::new()));
    let items: Vec<ResponseItem> = match serde_json::from_value(items_value) {
        Ok(v) => v,
        Err(e) => {
            send_event(
                out_tx,
                chat_error_body(args, &chat_id, format!("invalid compaction items: {e}")),
            )
            .await?;
            return Ok(());
        }
    };
    match chats.replace_with_native_history(&chat_id, items).await {
        Ok(()) => send_event(out_tx, chat_appended_body(args, &chat_id)).await,
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
    let output_schema = body.get("output_schema").cloned();
    spawn_turn(ctx.clone(), chat_id, cancel, output_schema, None);
    Ok(())
}

async fn handle_chat_compact(
    ctx: &DispatcherContext,
    body: &Map<String, Value>,
) -> Result<(), ChatgptError> {
    let chat_id = match read_chat_id(body) {
        Some(id) => id,
        None => {
            send_event(
                &ctx.out_tx,
                turn_error_body(&ctx.args, None, "chat.compact missing `chat_id`"),
            )
            .await?;
            return Ok(());
        }
    };
    let trigger = body
        .get("trigger")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("manual")
        .to_owned();

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

    let result = compact_chat(ctx, &chat_id, &trigger, &cancel).await;
    ctx.chats.end_turn(&chat_id).await;
    result
}

async fn compact_chat(
    ctx: &DispatcherContext,
    chat_id: &ChatId,
    trigger: &str,
    cancel: &TurnToken,
) -> Result<(), ChatgptError> {
    let snapshot = match ctx.chats.snapshot(chat_id).await {
        Ok(s) => s,
        Err(e) => {
            send_event(
                &ctx.out_tx,
                chat_error_body(&ctx.args, chat_id, e.to_string()),
            )
            .await?;
            return Ok(());
        }
    };
    let before_items = snapshot.history.len();
    if before_items == 0 {
        send_event(
            &ctx.out_tx,
            turn_error_body(&ctx.args, Some(chat_id), "nothing to compact"),
        )
        .await?;
        return Ok(());
    }

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
    let (provider_tool_names, tools_json) =
        match translator::tools_to_responses_format(&filtered_specs, &ctx.args.provider_name) {
            Ok(mapped) => mapped,
            Err(error) => {
                send_event(
                    &ctx.out_tx,
                    turn_error_body(&ctx.args, Some(chat_id), &error.to_string()),
                )
                .await?;
                return Ok(());
            }
        };
    let mut translated =
        translator::history_to_input(&snapshot.history, snapshot.system.as_deref());
    if let Err(error) = provider_tool_names.map_input_to_provider(&mut translated.input) {
        send_event(
            &ctx.out_tx,
            turn_error_body(&ctx.args, Some(chat_id), &error.to_string()),
        )
        .await?;
        return Ok(());
    }
    let supports_reasoning = if ctx.chats.model_reasoning_unsupported(&snapshot.model).await {
        false
    } else if let Some(api) = ctx.chats.model_capability_reasoning(&snapshot.model).await {
        api
    } else {
        translator::model_supports_reasoning(&snapshot.model)
    };
    let reasoning = supports_reasoning.then_some(Reasoning {
        effort: snapshot.reasoning_effort,
        summary: Some(ReasoningSummary::Concise),
    });
    let conversation_id = logical_routing_id(snapshot.conversation_id.as_deref(), chat_id);
    let provider_routing = provider_routing_identity(&conversation_id);
    let mut response_turn = ResponsesTurnContext::new(
        provider_routing.session_id,
        provider_routing.thread_id.clone(),
    );

    let auth_snap = ctx.auth.snapshot().await;
    if !matches!(auth_snap.state, AuthState::Connected) {
        send_event(&ctx.out_tx, auth_status_body(&ctx.args, &auth_snap)).await?;
        send_event(
            &ctx.out_tx,
            turn_error_body(
                &ctx.args,
                Some(chat_id),
                "auth not connected; cannot compact chat",
            ),
        )
        .await?;
        return Ok(());
    }
    if let Err(e) = ctx.auth.current_access_token().await {
        handle_refresh_error(ctx, Some(chat_id), e).await?;
        return Ok(());
    }
    let auth_snap = ctx.auth.snapshot().await;

    let mut input = translated.input;
    input.push(ResponseItem::CompactionTrigger {});
    let req = ResponsesApiRequest {
        model: snapshot.model.clone(),
        instructions: translated.instructions,
        input,
        tools: tools_json,
        tool_choice: "auto".into(),
        parallel_tool_calls: false,
        reasoning,
        store: false,
        stream: true,
        include: vec![],
        service_tier: snapshot
            .service_tier
            .map(|tier| tier.as_wire_value().to_owned()),
        prompt_cache_key: Some(provider_routing.thread_id),
        text: None,
    };

    let compacted = tokio::select! {
        _ = cancel.cancelled() => {
            send_event(
                &ctx.out_tx,
                turn_error_body(&ctx.args, Some(chat_id), "interrupted"),
            )
            .await?;
            return Ok(());
        }
        result = ctx.responses_client.compact_v2(&req, &auth_snap, &mut response_turn) => result,
    };

    let compacted = match compacted {
        Ok(mut items) if !items.is_empty() => {
            if let Err(error) = provider_tool_names.map_output_to_internal(&mut items) {
                send_event(
                    &ctx.out_tx,
                    turn_error_body(&ctx.args, Some(chat_id), &error.to_string()),
                )
                .await?;
                return Ok(());
            }
            items
        }
        Ok(_) => {
            send_event(
                &ctx.out_tx,
                turn_error_body(&ctx.args, Some(chat_id), "compact returned no items"),
            )
            .await?;
            return Ok(());
        }
        Err(ChatgptError::ResponsesEndpoint { status, body }) => {
            send_event(
                &ctx.out_tx,
                turn_error_body(
                    &ctx.args,
                    Some(chat_id),
                    &format!("compact HTTP {status}: {}", snippet(&body)),
                ),
            )
            .await?;
            return Ok(());
        }
        Err(e) => {
            send_event(
                &ctx.out_tx,
                turn_error_body(&ctx.args, Some(chat_id), &format!("compact failed: {e}")),
            )
            .await?;
            return Ok(());
        }
    };

    let after_items = compacted.len();
    if let Err(e) = ctx
        .chats
        .replace_with_native_history(chat_id, compacted.clone())
        .await
    {
        send_event(
            &ctx.out_tx,
            chat_error_body(&ctx.args, chat_id, e.to_string()),
        )
        .await?;
        return Ok(());
    }

    send_event(
        &ctx.out_tx,
        chat_compaction_commit_body(
            &ctx.args,
            chat_id,
            &snapshot.model,
            trigger,
            before_items,
            after_items,
            &compacted,
        ),
    )
    .await
}

fn tool_output_for_text_model(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        return s.to_owned();
    }

    if value.get("type").and_then(Value::as_str) == Some("media") {
        let media_type = value
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or("media");
        if media_type.starts_with("image/") {
            let filename = value
                .get("filename")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("image");
            return format!(
                "ERROR: Cannot read \"{filename}\" (this model does not support image input). Inform the user."
            );
        }
    }

    value.to_string()
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

#[derive(Default)]
struct NativeOutputBuffer {
    indexed: BTreeMap<u32, ResponseItem>,
    unindexed: Vec<ResponseItem>,
}

impl NativeOutputBuffer {
    fn push(&mut self, output_index: Option<u32>, item: ResponseItem) -> Result<(), String> {
        if let Some(output_index) = output_index {
            if self.indexed.insert(output_index, item).is_some() {
                return Err(format!(
                    "Responses stream completed more than one output item at index {output_index}"
                ));
            }
        } else {
            self.unindexed.push(item);
        }
        Ok(())
    }

    fn into_items(self) -> Vec<ResponseItem> {
        self.indexed.into_values().chain(self.unindexed).collect()
    }
}

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

    fn into_tool_calls(
        self,
        names: &crate::provider_tool_names::ProviderToolNames,
    ) -> Result<Vec<ToolCall>, crate::provider_tool_names::ProviderToolNameError> {
        let mut by_id = self.by_item_id;
        self.order
            .into_iter()
            .filter_map(|item_id| by_id.remove(&item_id))
            .map(|pc| {
                Ok(ToolCall {
                    id: pc.call_id,
                    function: ToolCallFunction {
                        name: names.to_internal(&pc.name)?.to_owned(),
                        arguments: pc.args,
                    },
                })
            })
            .collect()
    }
}

struct ReasoningSummaryFormatter {
    buffer: String,
    next_step: usize,
    detail_active: bool,
}

impl Default for ReasoningSummaryFormatter {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            next_step: 1,
            detail_active: false,
        }
    }
}

impl ReasoningSummaryFormatter {
    fn push_delta(&mut self, delta: &str) -> Vec<String> {
        self.buffer.push_str(delta);
        self.drain(false)
    }

    fn finish(&mut self) -> Vec<String> {
        self.drain(true)
    }

    fn drain(&mut self, flush: bool) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            if self.buffer.is_empty() {
                break;
            }

            if self.buffer.starts_with("**") {
                let Some(close_rel) = self.buffer[2..].find("**") else {
                    if flush {
                        let title = self.buffer[2..].trim().to_owned();
                        self.buffer.clear();
                        self.push_title(&mut out, &title);
                    }
                    break;
                };
                let close = 2 + close_rel;
                let title = self.buffer[2..close].trim().to_owned();
                self.buffer.drain(..close + 2);
                self.push_title(&mut out, &title);
                continue;
            }

            let marker = self.buffer.find("**");
            let take_len = marker.unwrap_or_else(|| {
                if !flush && self.buffer.ends_with('*') {
                    self.buffer.len() - 1
                } else {
                    self.buffer.len()
                }
            });
            if take_len == 0 {
                break;
            }
            let detail = self.buffer[..take_len].to_owned();
            self.buffer.drain(..take_len);
            self.push_detail(&mut out, &detail);
        }
        out
    }

    fn push_title(&mut self, out: &mut Vec<String>, title: &str) {
        if title.is_empty() {
            return;
        }
        let prefix = if self.next_step == 1 {
            ""
        } else if self.detail_active {
            "\n"
        } else {
            ""
        };
        out.push(format!("{prefix}{}. {title}\n", self.next_step));
        self.next_step = self.next_step.saturating_add(1);
        self.detail_active = false;
    }

    fn push_detail(&mut self, out: &mut Vec<String>, detail: &str) {
        let text = if self.detail_active {
            detail.to_owned()
        } else {
            detail.trim_start().to_owned()
        };
        if text.trim().is_empty() {
            return;
        }
        let mut formatted = text.replace('\n', "\n   ");
        if !self.detail_active {
            formatted = format!("   {formatted}");
        }
        self.detail_active = true;
        out.push(formatted);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedTokenUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    service_tier: Option<String>,
}

impl ParsedTokenUsage {
    fn billing_component(&self) -> Value {
        let mut component = Map::from_iter([
            ("usage_available".into(), Value::Bool(true)),
            (
                "input_tokens".into(),
                Value::Number(self.input_tokens.into()),
            ),
            (
                "output_tokens".into(),
                Value::Number(self.output_tokens.into()),
            ),
            ("input_tokens_include_cache_read".into(), Value::Bool(true)),
        ]);
        if let Some(tokens) = self.cache_read_input_tokens {
            component.insert(
                "cache_read_input_tokens".into(),
                Value::Number(tokens.into()),
            );
        }
        if let Some(tokens) = self.cache_write_input_tokens {
            component.insert(
                "cache_write_input_tokens".into(),
                Value::Number(tokens.into()),
            );
        }
        if let Some(tokens) = self.reasoning_tokens {
            component.insert("reasoning_tokens".into(), Value::Number(tokens.into()));
        }
        if let Some(tier) = &self.service_tier {
            component.insert("service_tier".into(), Value::String(tier.clone()));
        }
        Value::Object(component)
    }
}

fn parsed_token_usage(response: &Value) -> Option<ParsedTokenUsage> {
    let usage = response.get("usage")?;
    Some(ParsedTokenUsage {
        input_tokens: usage.get("input_tokens")?.as_u64()?,
        output_tokens: usage.get("output_tokens")?.as_u64()?,
        cache_read_input_tokens: usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64),
        cache_write_input_tokens: usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cache_write_tokens"))
            .and_then(Value::as_u64),
        reasoning_tokens: usage
            .get("output_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64),
        service_tier: response
            .get("service_tier")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OperationUsage {
    input_tokens: u64,
    output_tokens: u64,
    context_input_tokens: Option<u64>,
    aggregate_totals_overflowed: bool,
    billing_components: Vec<Option<ParsedTokenUsage>>,
}

fn sum_optional_tokens(
    components: &[Option<ParsedTokenUsage>],
    get: impl Fn(&ParsedTokenUsage) -> Option<u64>,
) -> Option<u64> {
    components.iter().try_fold(0_u64, |total, component| {
        Some(total.saturating_add(get(component.as_ref()?)?))
    })
}

impl OperationUsage {
    fn record_iteration(current: Option<Self>, iteration: Option<ParsedTokenUsage>) -> Self {
        let Some(mut prior) = current else {
            let (input_tokens, output_tokens, context_input_tokens) = iteration
                .as_ref()
                .map(|usage| {
                    (
                        usage.input_tokens,
                        usage.output_tokens,
                        Some(usage.input_tokens),
                    )
                })
                .unwrap_or((0, 0, None));
            return Self {
                input_tokens,
                output_tokens,
                context_input_tokens,
                aggregate_totals_overflowed: false,
                billing_components: vec![iteration],
            };
        };
        if let Some(usage) = &iteration {
            let input = prior.input_tokens.checked_add(usage.input_tokens);
            let output = prior.output_tokens.checked_add(usage.output_tokens);
            prior.aggregate_totals_overflowed |= input.is_none() || output.is_none();
            prior.input_tokens = input.unwrap_or(u64::MAX);
            prior.output_tokens = output.unwrap_or(u64::MAX);
            prior.context_input_tokens = Some(usage.input_tokens);
        } else {
            prior.context_input_tokens = None;
        }
        prior.billing_components.push(iteration);
        prior
    }

    fn complete(&self) -> bool {
        self.billing_components.iter().all(Option::is_some)
    }

    fn aggregate_totals_exact(&self) -> bool {
        self.complete()
            && !self.aggregate_totals_overflowed
            && self.input_tokens.checked_add(self.output_tokens).is_some()
    }

    fn totals(&self) -> Option<(u64, u64)> {
        self.aggregate_totals_exact()
            .then_some((self.input_tokens, self.output_tokens))
    }

    fn completion_fields(
        &self,
        provider: &str,
        model: &str,
        duration_ms: u64,
    ) -> Vec<(&'static str, Value)> {
        let mut fields = vec![
            (
                "billing_components",
                Value::Array(
                    self.billing_components
                        .iter()
                        .map(|component| {
                            component.as_ref().map_or_else(
                                || serde_json::json!({"usage_available": false}),
                                |usage| usage.billing_component(),
                            )
                        })
                        .collect(),
                ),
            ),
            ("billing_components_complete", Value::Bool(self.complete())),
            (
                "aggregate_totals_exact",
                Value::Bool(self.aggregate_totals_exact()),
            ),
            ("provider", Value::String(provider.to_owned())),
            ("model", Value::String(model.to_owned())),
            ("duration_ms", Value::Number(duration_ms.into())),
        ];
        if let Some(context_input_tokens) = self.context_input_tokens {
            fields.push((
                "context_input_tokens",
                Value::Number(context_input_tokens.into()),
            ));
        }
        if !self.aggregate_totals_exact() {
            return fields;
        }
        let Some(total_tokens) = self.input_tokens.checked_add(self.output_tokens) else {
            return fields;
        };
        fields.extend([
            ("prompt_tokens", Value::Number(self.input_tokens.into())),
            ("input_tokens", Value::Number(self.input_tokens.into())),
            (
                "completion_tokens",
                Value::Number(self.output_tokens.into()),
            ),
            ("output_tokens", Value::Number(self.output_tokens.into())),
            ("total_tokens", Value::Number(total_tokens.into())),
            ("input_tokens_include_cache_read", Value::Bool(true)),
        ]);
        for (name, tokens) in [
            (
                "cache_read_input_tokens",
                sum_optional_tokens(&self.billing_components, |usage| {
                    usage.cache_read_input_tokens
                }),
            ),
            (
                "cache_write_input_tokens",
                sum_optional_tokens(&self.billing_components, |usage| {
                    usage.cache_write_input_tokens
                }),
            ),
            (
                "reasoning_tokens",
                sum_optional_tokens(&self.billing_components, |usage| usage.reasoning_tokens),
            ),
        ] {
            if let Some(tokens) = tokens {
                fields.push((name, Value::Number(tokens.into())));
            }
        }
        fields
    }
}

fn usage_to_record(interrupted: bool, total_usage: Option<(u64, u64)>) -> Option<(u64, u64)> {
    if interrupted {
        total_usage
    } else {
        Some(total_usage.unwrap_or((0, 0)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamFailureKind {
    BodyRead,
    IdleTimeout,
    RecoveryTimeout,
    CleanEof,
    Parse,
    Endpoint,
    Semantic,
    Other,
}

impl StreamFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::BodyRead => "body_read",
            Self::IdleTimeout => "idle_timeout",
            Self::RecoveryTimeout => "recovery_timeout",
            Self::CleanEof => "clean_eof",
            Self::Parse => "parse",
            Self::Endpoint => "endpoint",
            Self::Semantic => "semantic",
            Self::Other => "other",
        }
    }

    fn transport_retryable(self) -> bool {
        matches!(
            self,
            Self::BodyRead | Self::IdleTimeout | Self::RecoveryTimeout | Self::CleanEof
        )
    }
}

#[derive(Debug, Clone)]
struct StreamFailure {
    kind: StreamFailureKind,
    message: String,
    retryable: bool,
}

impl StreamFailure {
    fn from_error(error: ChatgptError) -> Self {
        let request_retryable = match &error {
            ChatgptError::ResponsesEndpoint { status, body } => {
                is_transient_status(*status)
                    && !response_signals_terminal_quota_denial(*status, body)
            }
            ChatgptError::Http(error) => {
                error.is_connect() || error.is_timeout() || error.is_request()
            }
            _ => false,
        };
        let kind = match error {
            ChatgptError::ResponsesStreamRead(_) => StreamFailureKind::BodyRead,
            ChatgptError::ResponsesStreamIdleTimeout { .. } => StreamFailureKind::IdleTimeout,
            ChatgptError::ResponsesStreamRecoveryTimeout { .. } => {
                StreamFailureKind::RecoveryTimeout
            }
            ChatgptError::ResponsesStreamEnded => StreamFailureKind::CleanEof,
            ChatgptError::ResponsesStreamParse(_) => StreamFailureKind::Parse,
            ChatgptError::ResponsesEndpoint { .. } => StreamFailureKind::Endpoint,
            _ => StreamFailureKind::Other,
        };
        Self {
            kind,
            message: format!("stream error: {error}"),
            retryable: kind.transport_retryable() || request_retryable,
        }
    }

    fn semantic(response: &Value) -> Self {
        Self {
            kind: StreamFailureKind::Semantic,
            message: response_failure_message(response),
            retryable: response_failure_is_transient(response),
        }
    }
}

async fn open_response_stream(
    client: &ResponsesClient,
    request: &ResponsesApiRequest,
    auth: &AuthSnapshot,
    turn: &mut ResponsesTurnContext,
    deadline: Option<tokio::time::Instant>,
    budget: Duration,
) -> Result<ResponseStream, ChatgptError> {
    let open = client.stream(request, auth, turn);
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, open)
            .await
            .unwrap_or_else(|_| {
                Err(ChatgptError::ResponsesStreamRecoveryTimeout {
                    budget_ms: budget.as_millis() as u64,
                })
            }),
        None => open.await,
    }
}

async fn next_response_event(
    stream: &mut ResponseStream,
    deadline: Option<tokio::time::Instant>,
    budget: Duration,
) -> Option<Result<ResponseEvent, ChatgptError>> {
    let next = stream.next();
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, next)
            .await
            .unwrap_or_else(|_| {
                Some(Err(ChatgptError::ResponsesStreamRecoveryTimeout {
                    budget_ms: budget.as_millis() as u64,
                }))
            }),
        None => next.await,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamAttemptState {
    visible_state_observed: bool,
    native_output_state_observed: bool,
    tool_state_observed: bool,
    terminal_event_seen: bool,
}

impl StreamAttemptState {
    fn has_provisional_state(self) -> bool {
        self.visible_state_observed || self.native_output_state_observed || self.tool_state_observed
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct StreamRetryPolicy {
    max_attempts: Option<u32>,
    budget: Option<Duration>,
}

#[derive(Debug, Default)]
struct RecoveryGateState {
    open_until: Option<tokio::time::Instant>,
    probe_in_flight: bool,
}

/// Coordinates recovery across every turn owned by one provider process. Once
/// any turn observes a transient stream failure, new requests wait behind one
/// half-open probe instead of stampeding the same unhealthy upstream path.
#[derive(Debug, Default)]
struct ProviderRecoveryGate {
    state: Mutex<RecoveryGateState>,
    changed: Notify,
}

impl ProviderRecoveryGate {
    async fn record_failure(&self, delay: Duration) -> Duration {
        let now = tokio::time::Instant::now();
        let candidate = now + delay;
        let mut state = self.state.lock().await;
        state.open_until = Some(
            state
                .open_until
                .map_or(candidate, |existing| existing.max(candidate)),
        );
        state.probe_in_flight = false;
        let remaining = state
            .open_until
            .expect("recovery deadline was just set")
            .saturating_duration_since(now);
        drop(state);
        self.changed.notify_waiters();
        remaining
    }

    async fn admit(&self, cancel: &TurnToken) -> bool {
        loop {
            let wait_until = {
                let mut state = self.state.lock().await;
                let Some(open_until) = state.open_until else {
                    return true;
                };
                if tokio::time::Instant::now() >= open_until && !state.probe_in_flight {
                    state.probe_in_flight = true;
                    return true;
                }
                open_until
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return false,
                _ = self.changed.notified() => {},
                _ = tokio::time::sleep_until(wait_until) => {},
            }
        }
    }

    async fn record_success(&self) {
        let mut state = self.state.lock().await;
        state.open_until = None;
        state.probe_in_flight = false;
        drop(state);
        self.changed.notify_waiters();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryDecisionReason {
    RetryableProvisionalFailure,
    FailureNotRetryable,
    AttemptLimitReached,
    ElapsedBudgetExhausted,
}

impl RetryDecisionReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::RetryableProvisionalFailure => "retryable_provisional_failure",
            Self::FailureNotRetryable => "failure_not_retryable",
            Self::AttemptLimitReached => "attempt_limit_reached",
            Self::ElapsedBudgetExhausted => "elapsed_budget_exhausted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetryDecision {
    retry: bool,
    reason: RetryDecisionReason,
}

fn decide_pre_output_retry(
    failure: &StreamFailure,
    attempt: u32,
    elapsed: Duration,
    next_delay: Duration,
    policy: StreamRetryPolicy,
) -> RetryDecision {
    let reason = if !failure.retryable {
        RetryDecisionReason::FailureNotRetryable
    } else if policy.max_attempts.is_some_and(|limit| attempt >= limit) {
        RetryDecisionReason::AttemptLimitReached
    } else if policy
        .budget
        .is_some_and(|budget| elapsed.saturating_add(next_delay) >= budget)
    {
        RetryDecisionReason::ElapsedBudgetExhausted
    } else {
        return RetryDecision {
            retry: true,
            reason: RetryDecisionReason::RetryableProvisionalFailure,
        };
    };
    RetryDecision {
        retry: false,
        reason,
    }
}

fn pre_output_retry_delay(retry_number: u32) -> Duration {
    let ceiling = pre_output_retry_delay_ceiling(retry_number);
    Duration::from_millis(rand::thread_rng().gen_range(0..=ceiling.as_millis() as u64))
}

fn pre_output_retry_delay_ceiling(retry_number: u32) -> Duration {
    let shift = retry_number.saturating_sub(1).min(32);
    let factor = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
    let delay = PRE_OUTPUT_RETRY_BASE_DELAY_MS
        .saturating_mul(factor)
        .min(PRE_OUTPUT_RETRY_MAX_DELAY_MS);
    Duration::from_millis(delay)
}

fn response_failure_message(response: &Value) -> String {
    response
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("response.failed")
        .to_owned()
}

fn response_failure_is_transient(response: &Value) -> bool {
    let error = response.get("error").unwrap_or(response);
    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    let kind = error.get("type").and_then(Value::as_str).unwrap_or("");
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    let fingerprint = format!("{code} {kind} {message}").to_ascii_lowercase();

    [
        "overload",
        "server_error",
        "service_unavailable",
        "temporarily unavailable",
        "server busy",
        "try again later",
        "an error occurred while processing your request",
    ]
    .iter()
    .any(|needle| fingerprint.contains(needle))
}

fn terminal_stream_failure_message(
    failure: &StreamFailure,
    decision: RetryDecision,
    attempt: u32,
) -> String {
    if failure.kind.transport_retryable()
        && matches!(
            decision.reason,
            RetryDecisionReason::AttemptLimitReached | RetryDecisionReason::ElapsedBudgetExhausted
        )
    {
        return format!(
            "ChatGPT stream transport failed after {attempt} provisional attempts. Configured automatic recovery is exhausted; retrying the turn is safe. Cause: {}",
            failure.message
        );
    }
    failure.message.clone()
}

fn spawn_turn(
    ctx: DispatcherContext,
    chat_id: ChatId,
    cancel: TurnToken,
    output_schema: Option<Value>,
    direct_request: Option<(String, DirectCompletion)>,
) {
    tokio::spawn(async move {
        let turn_id = uuid::Uuid::new_v4().to_string();
        let started = Instant::now();
        let mut iterations: u32 = 0;
        let mut final_text = String::new();
        let mut final_reasoning = String::new();
        // Every loop path assigns this before break; the initial value
        // is unused but the linter can't see that across the loop,
        // so suppress the false positive.
        #[allow(unused_assignments)]
        let mut final_finish_reason: Option<String> = None;
        // The failure detail of an errored turn, threaded into the terminal
        // chat.complete.result so its single consumer sees why the round died
        // (turn.error/chat.error stay the live-surface signals).
        let mut final_error: Option<String> = None;
        let mut final_tool_calls: Vec<ToolCall> = Vec::new();
        let mut final_native_output: Vec<ResponseItem> = Vec::new();
        let mut interrupted = false;
        let mut errored = false;
        let mut operation_usage: Option<OperationUsage> = None;
        let mut active_model = String::new();
        let mut pre_output_stream_retries: u32 = 0;
        let mut stream_recovery_started_at: Option<tokio::time::Instant> = None;
        let mut final_stream_attempts: u32 = 0;
        let mut final_terminal_event_seen = false;
        let mut auth_401_recovery_stage: u8 = 0;
        let conversation_id = logical_routing_id(
            ctx.chats
                .conversation_id(&chat_id)
                .await
                .ok()
                .flatten()
                .as_deref(),
            &chat_id,
        );
        let provider_routing = provider_routing_identity(&conversation_id);
        let cache_key = provider_routing.thread_id.clone();
        let mut response_turn =
            ResponsesTurnContext::new(provider_routing.session_id, provider_routing.thread_id);

        loop {
            iterations += 1;
            if iterations > TOOL_LOOP_MAX_ITERATIONS {
                tracing::warn!(cap = TOOL_LOOP_MAX_ITERATIONS, "tool-loop cap hit");
                errored = true;
                final_finish_reason = Some("error".into());
                let msg = format!(
                    "tool-loop iteration cap hit ({} iterations); aborting",
                    TOOL_LOOP_MAX_ITERATIONS
                );
                final_error = Some(msg.clone());
                let _ = ctx
                    .out_tx
                    .send(PluginOutgoing::event(turn_error_body(
                        &ctx.args,
                        Some(&chat_id),
                        &msg,
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
                    final_error = Some(e.to_string());
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
            let (provider_tool_names, tools_json) = match translator::tools_to_responses_format(
                &filtered_specs,
                &ctx.args.provider_name,
            ) {
                Ok(mapped) => mapped,
                Err(error) => {
                    errored = true;
                    final_finish_reason = Some("error".into());
                    final_error = Some(error.to_string());
                    let _ = ctx
                        .out_tx
                        .send(PluginOutgoing::event(turn_error_body(
                            &ctx.args,
                            Some(&chat_id),
                            &error.to_string(),
                        )))
                        .await;
                    break;
                }
            };
            tracing::info!(
                count = filtered_specs.len(),
                names = ?filtered_specs.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
                allowlist = ?snapshot.tool_allowlist,
                "sending tools to Responses API"
            );

            let mut translated =
                translator::history_to_input(&snapshot.history, snapshot.system.as_deref());
            if let Err(error) = provider_tool_names.map_input_to_provider(&mut translated.input) {
                errored = true;
                final_finish_reason = Some("error".into());
                final_error = Some(error.to_string());
                let _ = ctx
                    .out_tx
                    .send(PluginOutgoing::event(turn_error_body(
                        &ctx.args,
                        Some(&chat_id),
                        &error.to_string(),
                    )))
                    .await;
                break;
            }
            tracing::info!(
                instructions_len = translated.instructions.len(),
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
            let supports_reasoning = if ctx.chats.model_reasoning_unsupported(&snapshot.model).await
            {
                false
            } else if let Some(api) = ctx.chats.model_capability_reasoning(&snapshot.model).await {
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
                effort: snapshot.reasoning_effort,
                summary: Some(ReasoningSummary::Concise),
            });
            let supports_parallel_tool_calls = ctx
                .chats
                .model_capability_parallel_tool_calls(&snapshot.model)
                .await
                .unwrap_or(true);

            let text = structured_text_controls(output_schema.as_ref());
            let req = ResponsesApiRequest {
                model: snapshot.model.clone(),
                instructions: translated.instructions,
                input: translated.input,
                tools: tools_json,
                tool_choice: "auto".into(),
                parallel_tool_calls: supports_parallel_tool_calls,
                reasoning,
                store: false,
                stream: true,
                include,
                service_tier: snapshot
                    .service_tier
                    .map(|tier| tier.as_wire_value().to_owned()),
                prompt_cache_key: Some(cache_key.clone()),
                text,
            };

            // This request is the provider's exact model-visible representation.
            // Publish a content-free estimate now so canonical user/tool/deferred
            // additions move the statusline before the response completes. A later
            // backend usage event replaces it with the authoritative token count.
            if let Some((request_id, _)) = &direct_request {
                let estimate = crate::request_usage::estimate_serialized_tokens(&req);
                let body = completion_event_body(
                    &ctx.args,
                    request_id,
                    "usage",
                    [
                        ("context_input_tokens", Value::Number(estimate.into())),
                        ("context_input_accuracy", Value::String("estimated".into())),
                    ],
                );
                let _ = ctx.out_tx.send(PluginOutgoing::event(body)).await;
            }

            // Snapshot auth before sending so we can fail fast on
            // LoginRequired/Error without burning an HTTP round trip.
            let auth_snap = ctx.auth.snapshot().await;
            if !matches!(auth_snap.state, AuthState::Connected) {
                let _ = ctx
                    .out_tx
                    .send(PluginOutgoing::event(auth_status_body(
                        &ctx.args, &auth_snap,
                    )))
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
                final_error = Some("auth not connected; cannot complete turn".into());
                break;
            }

            // Refresh token if needed before building headers. The
            // current_access_token call refreshes under the hood; we
            // grab a fresh snapshot afterwards because the cached
            // tokens may have rotated.
            if let Err(e) = ctx.auth.current_access_token().await {
                final_error = Some(format!("token refresh failed: {e}"));
                let _ = handle_refresh_error(&ctx, Some(&chat_id), e).await;
                errored = true;
                final_finish_reason = Some("error".into());
                break;
            }
            let auth_snap = ctx.auth.snapshot().await;

            // Production recovery has no elapsed deadline. Tests and explicit
            // embeddings may install one; it begins at the first transient
            // failure rather than charging healthy request time to recovery.
            let attempt_deadline = if pre_output_stream_retries > 0 {
                ctx.stream_retry_policy
                    .budget
                    .and_then(|budget| stream_recovery_started_at.map(|started| started + budget))
            } else {
                None
            };
            if !ctx.recovery_gate.admit(&cancel).await {
                break;
            }
            let mut stream = match open_response_stream(
                &ctx.responses_client,
                &req,
                &auth_snap,
                &mut response_turn,
                attempt_deadline,
                ctx.stream_retry_policy.budget.unwrap_or_default(),
            )
            .await
            {
                Ok(s) => {
                    auth_401_recovery_stage = 0;
                    ctx.recovery_gate.record_success().await;
                    s
                }
                Err(error @ ChatgptError::ResponsesStreamRecoveryTimeout { .. }) => {
                    ResponseStream::new(
                        Box::pin(futures::stream::once(async move { Err(error) })),
                        None,
                    )
                }
                Err(ChatgptError::ResponsesEndpoint { status, body }) => {
                    if is_transient_status(status)
                        && !response_signals_terminal_quota_denial(status, &body)
                    {
                        let error = ChatgptError::ResponsesEndpoint { status, body };
                        ResponseStream::new(
                            Box::pin(futures::stream::once(async move { Err(error) })),
                            None,
                        )
                    } else {
                        if status == 401 {
                            let failed_token = auth_snap
                                .tokens
                                .as_ref()
                                .map(|tokens| tokens.access_token.clone());
                            let disk_credentials_adopted = if auth_401_recovery_stage == 0 {
                                match ctx.auth.adopt_disk_credentials().await {
                                    Ok(adopted) => adopted,
                                    Err(error) => {
                                        tracing::warn!(%error, "could not reload auth file after 401");
                                        false
                                    }
                                }
                            } else {
                                false
                            };
                            let recovery_action =
                                auth_401_action(auth_401_recovery_stage, disk_credentials_adopted);
                            match recovery_action {
                                Auth401Action::RetryReloaded => {
                                    auth_401_recovery_stage = 1;
                                    operation_usage = Some(OperationUsage::record_iteration(
                                        operation_usage,
                                        None,
                                    ));
                                    iterations = iterations.saturating_sub(1);
                                    continue;
                                }
                                Auth401Action::ForceRefresh => {}
                                Auth401Action::Fail => {
                                    let snap =
                                        ctx.auth.apply_error(HTTP_401_MESSAGE.to_owned()).await;
                                    let _ = ctx
                                        .out_tx
                                        .send(PluginOutgoing::event(auth_status_body(
                                            &ctx.args, &snap,
                                        )))
                                        .await;
                                }
                            }
                            if recovery_action == Auth401Action::ForceRefresh {
                                if let Some(failed_token) = failed_token {
                                    match ctx.auth.force_refresh_after(&failed_token).await {
                                        Ok(_) => {
                                            auth_401_recovery_stage = 2;
                                            operation_usage =
                                                Some(OperationUsage::record_iteration(
                                                    operation_usage,
                                                    None,
                                                ));
                                            iterations = iterations.saturating_sub(1);
                                            continue;
                                        }
                                        Err(error) => {
                                            final_error =
                                                Some(format!("token refresh failed: {error}"));
                                            let _ =
                                                handle_refresh_error(&ctx, Some(&chat_id), error)
                                                    .await;
                                            errored = true;
                                            final_finish_reason = Some("error".into());
                                            break;
                                        }
                                    }
                                } else {
                                    let snap =
                                        ctx.auth.apply_error(HTTP_401_MESSAGE.to_owned()).await;
                                    let _ = ctx
                                        .out_tx
                                        .send(PluginOutgoing::event(auth_status_body(
                                            &ctx.args, &snap,
                                        )))
                                        .await;
                                }
                            }
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
                            operation_usage =
                                Some(OperationUsage::record_iteration(operation_usage, None));
                            iterations = iterations.saturating_sub(1);
                            continue;
                        }
                        let msg = format!("HTTP {status}: {}", snippet(&body));
                        final_error = Some(msg.clone());
                        let _ = ctx
                            .out_tx
                            .send(PluginOutgoing::event(turn_error_body(
                                &ctx.args,
                                Some(&chat_id),
                                &msg,
                            )))
                            .await;
                        errored = true;
                        final_finish_reason = Some("error".into());
                        break;
                    }
                }
                Err(error @ ChatgptError::Http(_)) => ResponseStream::new(
                    Box::pin(futures::stream::once(async move { Err(error) })),
                    None,
                ),
                Err(e) => {
                    let msg = format!("request failed: {e}");
                    final_error = Some(msg.clone());
                    let _ = ctx
                        .out_tx
                        .send(PluginOutgoing::event(turn_error_body(
                            &ctx.args,
                            Some(&chat_id),
                            &msg,
                        )))
                        .await;
                    errored = true;
                    final_finish_reason = Some("error".into());
                    break;
                }
            };

            if let Some(usage) = stream.usage() {
                if let Ok(body) = usage_updated_body(&ctx.args, usage) {
                    let _ = ctx.out_tx.try_send(PluginOutgoing::event(body));
                }
            }

            let mut output_text = String::new();
            let mut reasoning_text = String::new();
            let mut reasoning_formatter = ReasoningSummaryFormatter::default();
            let mut reasoning_started_at: Option<std::time::Instant> = None;
            let mut tool_buf = ToolCallBuffer::default();
            let mut iter_native_output = NativeOutputBuffer::default();
            let mut iter_finish_reason: Option<String> = None;
            let mut iter_usage: Option<ParsedTokenUsage> = None;
            let mut iter_interrupted = false;
            let mut iter_failure: Option<StreamFailure> = None;
            let mut iter_errored: Option<String> = None;
            let mut terminal_event_seen = false;
            let mut visible_state_observed = false;
            let mut native_output_state_observed = false;
            let mut tool_state_observed = false;

            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        iter_interrupted = true;
                        break;
                    }
                    next = next_response_event(
                        &mut stream,
                        attempt_deadline,
                        ctx.stream_retry_policy.budget.unwrap_or_default(),
                    ) => {
                        match next {
                            Some(Ok(event)) => match event {
                                ResponseEvent::OutputTextDelta { delta, .. } => {
                                    visible_state_observed = true;
                                    output_text.push_str(&delta);
                                    if output_schema.is_none() {
                                        let body = if let Some((request_id, _)) = &direct_request {
                                            completion_event_body(
                                                &ctx.args,
                                                request_id,
                                                "text_delta",
                                                [("text", Value::String(delta.clone()))],
                                            )
                                        } else {
                                            stream_delta_body(
                                                &ctx.args.event_prefix(),
                                                &turn_id,
                                                &chat_id,
                                                &delta,
                                            )
                                        };
                                        let _ = ctx.out_tx.try_send(PluginOutgoing::event(body));
                                    }
                                }
                                ResponseEvent::ReasoningSummaryDelta { delta, .. } => {
                                    visible_state_observed = true;
                                    if reasoning_started_at.is_none() {
                                        reasoning_started_at = Some(std::time::Instant::now());
                                    }
                                    for formatted in reasoning_formatter.push_delta(&delta) {
                                        if formatted.is_empty() {
                                            continue;
                                        }
                                        reasoning_text.push_str(&formatted);
                                        let body = if let Some((request_id, _)) = &direct_request {
                                            completion_event_body(
                                                &ctx.args,
                                                request_id,
                                                "reasoning_delta",
                                                [("text", Value::String(formatted.clone()))],
                                            )
                                        } else {
                                            stream_reasoning_delta_body(
                                                &ctx.args.event_prefix(),
                                                &turn_id,
                                                &chat_id,
                                                &formatted,
                                            )
                                        };
                                        let _ = ctx.out_tx.try_send(PluginOutgoing::event(body));
                                    }
                                }
                                ResponseEvent::ReasoningSummaryPartAdded { .. } => {
                                    visible_state_observed = true;
                                    if reasoning_started_at.is_none() {
                                        reasoning_started_at = Some(std::time::Instant::now());
                                    }
                                }
                                ResponseEvent::ReasoningContentDelta { delta, .. } => {
                                    visible_state_observed = true;
                                    if reasoning_started_at.is_none() {
                                        reasoning_started_at = Some(std::time::Instant::now());
                                    }
                                    for formatted in reasoning_formatter.finish() {
                                        if formatted.is_empty() {
                                            continue;
                                        }
                                        reasoning_text.push_str(&formatted);
                                        let body = if let Some((request_id, _)) = &direct_request {
                                            completion_event_body(
                                                &ctx.args,
                                                request_id,
                                                "reasoning_delta",
                                                [("text", Value::String(formatted.clone()))],
                                            )
                                        } else {
                                            stream_reasoning_delta_body(
                                                &ctx.args.event_prefix(),
                                                &turn_id,
                                                &chat_id,
                                                &formatted,
                                            )
                                        };
                                        let _ = ctx.out_tx.try_send(PluginOutgoing::event(body));
                                    }
                                    reasoning_text.push_str(&delta);
                                    let body = if let Some((request_id, _)) = &direct_request {
                                        completion_event_body(
                                            &ctx.args,
                                            request_id,
                                            "reasoning_delta",
                                            [("text", Value::String(delta.clone()))],
                                        )
                                    } else {
                                        stream_reasoning_delta_body(
                                            &ctx.args.event_prefix(),
                                            &turn_id,
                                            &chat_id,
                                            &delta,
                                        )
                                    };
                                    let _ = ctx.out_tx.try_send(PluginOutgoing::event(body));
                                }
                                ResponseEvent::FunctionCallArgumentsDelta { delta, item_id } => {
                                    tool_state_observed = true;
                                    tool_buf.on_args_delta(item_id.as_deref(), &delta);
                                    let body = stream_tool_call_delta_body(
                                        &ctx.args.event_prefix(),
                                        &chat_id,
                                        item_id.as_deref(),
                                        &delta,
                                    );
                                    let _ = ctx.out_tx.try_send(PluginOutgoing::event(body));
                                }
                                ResponseEvent::FunctionCallArgumentsDone {
                                    arguments,
                                    item_id,
                                } => {
                                    tool_state_observed = true;
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
                                    native_output_state_observed = true;
                                    tool_state_observed = true;
                                    // Deltas arrive keyed by the item's
                                    // server-side `id` (`fc_…`), not by
                                    // `call_id` (`call_…`). Use the id
                                    // when present so streamed args land
                                    // in the right buffer.
                                    let item_id = id.unwrap_or_else(|| call_id.clone());
                                    tool_buf.on_item_added(item_id, call_id, name, arguments);
                                }
                                ResponseEvent::OutputItemAdded { .. } => {
                                    native_output_state_observed = true;
                                }
                                ResponseEvent::OutputItemDone {
                                    item,
                                    output_index,
                                } => {
                                    native_output_state_observed = true;
                                    if let ResponseItem::FunctionCall {
                                        id,
                                        call_id,
                                        name,
                                        arguments,
                                    } = &item
                                    {
                                        tool_state_observed = true;
                                        let item_id =
                                            id.clone().unwrap_or_else(|| call_id.clone());
                                        if !tool_buf.by_item_id.contains_key(&item_id) {
                                            // Single-shot: no Added + no
                                            // deltas, only Done. Seed and
                                            // finalize in one step.
                                            tool_buf.on_item_added(
                                                item_id,
                                                call_id.clone(),
                                                name.clone(),
                                                arguments.clone(),
                                            );
                                        } else {
                                            tool_buf.on_item_done(Some(&item_id), arguments);
                                        }
                                    }
                                    if let Err(error) = iter_native_output.push(output_index, item) {
                                        iter_errored = Some(error);
                                        break;
                                    }
                                }
                                ResponseEvent::Completed { response } => {
                                    terminal_event_seen = true;
                                    iter_finish_reason =
                                        response.get("finish_reason").and_then(|v| v.as_str()).map(str::to_owned);
                                    iter_usage = parsed_token_usage(&response);
                                    break;
                                }
                                ResponseEvent::Failed { response } => {
                                    terminal_event_seen = true;
                                    iter_usage = parsed_token_usage(&response);
                                    iter_failure = Some(StreamFailure::semantic(&response));
                                    break;
                                }
                                ResponseEvent::Incomplete { response } => {
                                    terminal_event_seen = true;
                                    iter_usage = parsed_token_usage(&response);
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
                            Some(Err(error)) => {
                                iter_failure = Some(StreamFailure::from_error(error));
                                break;
                            }
                            None => {
                                iter_failure = Some(StreamFailure::from_error(
                                    ChatgptError::ResponsesStreamEnded,
                                ));
                                break;
                            }
                        }
                    }
                }
            }

            let mut iter_native_output = iter_native_output.into_items();

            // Hard cancel arrived mid-stream: drop this turn entirely.
            // The reqwest byte stream was already aborted when the select
            // took the `cancelled()` branch above (the stream future is
            // dropped on break); suppress every terminal emission by
            // breaking out before any result is built. A graceful
            // `interrupt` is NOT suppressed and falls through to the
            // interrupted-result path below.
            if cancel.is_suppressed() {
                break;
            }

            final_stream_attempts = pre_output_stream_retries + 1;
            final_terminal_event_seen = terminal_event_seen;

            if let Some(failure) = iter_failure.take() {
                let attempt = pre_output_stream_retries + 1;
                let candidate_delay = pre_output_retry_delay(attempt);
                let state = StreamAttemptState {
                    visible_state_observed,
                    native_output_state_observed,
                    tool_state_observed,
                    terminal_event_seen,
                };
                let recovery_started_at =
                    *stream_recovery_started_at.get_or_insert_with(tokio::time::Instant::now);
                let retry_elapsed = recovery_started_at.elapsed();
                let decision = decide_pre_output_retry(
                    &failure,
                    attempt,
                    retry_elapsed,
                    candidate_delay,
                    ctx.stream_retry_policy,
                );
                let delay = if decision.retry {
                    ctx.recovery_gate.record_failure(candidate_delay).await
                } else {
                    Duration::ZERO
                };
                tracing::warn!(
                    chat_id = %chat_id,
                    attempt,
                    max_attempts = ctx.stream_retry_policy.max_attempts,
                    retry = decision.retry,
                    retry_reason = decision.reason.as_str(),
                    failure_kind = failure.kind.as_str(),
                    terminal_event_seen,
                    visible_state_observed = state.visible_state_observed,
                    native_output_state_observed = state.native_output_state_observed,
                    tool_state_observed = state.tool_state_observed,
                    elapsed_ms = retry_elapsed.as_millis() as u64,
                    budget_ms = ctx.stream_retry_policy.budget.map(|budget| budget.as_millis() as u64),
                    delay_ms = delay.as_millis() as u64,
                    error = %failure.message,
                    "Responses stream retry decision",
                );
                if let Some((request_id, _)) = &direct_request {
                    if decision.retry && state.has_provisional_state() {
                        let body =
                            attempt_discarded_body(&ctx.args, request_id, attempt, &failure, state);
                        let _ = ctx.out_tx.send(PluginOutgoing::event(body)).await;
                    }
                    let body = retry_decision_body(
                        &ctx.args, request_id, attempt, delay, &failure, decision, state,
                    );
                    let _ = ctx.out_tx.send(PluginOutgoing::event(body)).await;
                }
                if decision.retry {
                    pre_output_stream_retries += 1;
                    operation_usage = Some(OperationUsage::record_iteration(
                        operation_usage,
                        iter_usage,
                    ));
                    iterations = iterations.saturating_sub(1);
                    if direct_request.is_none() {
                        let body = stream_retry_body(&ctx.args, &chat_id, attempt, delay, &failure);
                        let _ = ctx.out_tx.send(PluginOutgoing::event(body)).await;
                    }
                    continue;
                }
                iter_errored = Some(terminal_stream_failure_message(&failure, decision, attempt));
            }

            for formatted in reasoning_formatter.finish() {
                if formatted.is_empty() {
                    continue;
                }
                reasoning_text.push_str(&formatted);
                let body = if let Some((request_id, _)) = &direct_request {
                    completion_event_body(
                        &ctx.args,
                        request_id,
                        "reasoning_delta",
                        [("text", Value::String(formatted.clone()))],
                    )
                } else {
                    stream_reasoning_delta_body(
                        &ctx.args.event_prefix(),
                        &turn_id,
                        &chat_id,
                        &formatted,
                    )
                };
                let _ = ctx.out_tx.send(PluginOutgoing::event(body)).await;
            }

            // Emit per-turn reasoning_end if we accumulated any.
            if !reasoning_text.is_empty() {
                final_reasoning.push_str(&reasoning_text);
                let duration_ms = reasoning_started_at
                    .map(|s| s.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                let body = if let Some((request_id, _)) = &direct_request {
                    completion_event_body(
                        &ctx.args,
                        request_id,
                        "reasoning_end",
                        [
                            ("text", Value::String(reasoning_text.clone())),
                            ("duration_ms", Value::Number(duration_ms.into())),
                        ],
                    )
                } else {
                    stream_reasoning_end_body(
                        &ctx.args.event_prefix(),
                        &turn_id,
                        &chat_id,
                        &reasoning_text,
                        duration_ms,
                    )
                };
                let _ = ctx.out_tx.send(PluginOutgoing::event(body)).await;
            }

            operation_usage = Some(OperationUsage::record_iteration(
                operation_usage,
                iter_usage,
            ));

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
                final_error = Some(err_msg);
                break;
            }
            if let Err(error) = provider_tool_names.map_output_to_internal(&mut iter_native_output)
            {
                let message = error.to_string();
                let _ = ctx
                    .out_tx
                    .send(PluginOutgoing::event(turn_error_body(
                        &ctx.args,
                        Some(&chat_id),
                        &message,
                    )))
                    .await;
                errored = true;
                final_finish_reason = Some("error".into());
                final_error = Some(message);
                break;
            }
            if iter_interrupted {
                if !output_text.is_empty() {
                    let _ = ctx
                        .chats
                        .push_assistant(&chat_id, output_text.clone())
                        .await;
                }
                final_text = output_text;
                final_finish_reason = Some("interrupted".into());
                interrupted = true;
                break;
            }

            let tool_calls = match tool_buf.into_tool_calls(&provider_tool_names) {
                Ok(calls) => calls,
                Err(error) => {
                    errored = true;
                    final_finish_reason = Some("error".into());
                    final_error = Some(error.to_string());
                    let _ = ctx
                        .out_tx
                        .send(PluginOutgoing::event(turn_error_body(
                            &ctx.args,
                            Some(&chat_id),
                            &error.to_string(),
                        )))
                        .await;
                    break;
                }
            };
            if !tool_calls.is_empty() {
                if iter_native_output.is_empty() {
                    let _ = ctx
                        .chats
                        .push_assistant_tool_calls(
                            &chat_id,
                            output_text.clone(),
                            tool_calls.clone(),
                        )
                        .await;
                } else {
                    let _ = ctx
                        .chats
                        .append_native_history(&chat_id, iter_native_output.clone())
                        .await;
                }

                final_text = output_text;
                final_finish_reason = iter_finish_reason.or(Some("tool_calls".into()));
                final_tool_calls = tool_calls;
                final_native_output = iter_native_output;
                break;
            }

            // No tool calls → terminal turn.
            if !iter_native_output.is_empty() {
                let _ = ctx
                    .chats
                    .append_native_history(&chat_id, iter_native_output.clone())
                    .await;
            } else if !output_text.is_empty() {
                let _ = ctx
                    .chats
                    .push_assistant(&chat_id, output_text.clone())
                    .await;
            }
            final_text = output_text;
            final_finish_reason = iter_finish_reason.or(Some("stop".into()));
            final_native_output = iter_native_output;
            break;
        }

        // Suppressed hard-cancel: release the slot and return without
        // emitting stream.end / session.stats / turn.error /
        // chat.complete.result. No result is delivered for a cancelled
        // request — that is the honor side of graph.cancel. Any partial
        // stream.delta already put on the bus before the abort is
        // unavoidable (it was emitted live), but no terminal completion
        // lands.
        if cancel.is_suppressed() {
            ctx.chats.end_turn(&chat_id).await;
            if let Some((request_id, owner)) = &direct_request {
                ctx.direct_completions.finish(request_id, owner.owner).await;
            }
            return;
        }

        if let Some((request_id, owner)) = &direct_request {
            if !ctx.direct_completions.finish(request_id, owner.owner).await {
                ctx.chats.end_turn(&chat_id).await;
                return;
            }
            let elapsed_ms = started.elapsed().as_millis() as u64;

            if let Some(usage) = operation_usage {
                let body = completion_event_body(
                    &ctx.args,
                    request_id,
                    "usage",
                    usage.completion_fields(&ctx.args.provider_name, &active_model, elapsed_ms),
                );
                let _ = ctx.out_tx.send(PluginOutgoing::event(body)).await;
            }
            for call in &final_tool_calls {
                let arguments = serde_json::from_str(&call.function.arguments)
                    .unwrap_or_else(|_| Value::String(call.function.arguments.clone()));
                let body = completion_event_body(
                    &ctx.args,
                    request_id,
                    "tool_call",
                    [
                        ("id", Value::String(call.id.clone())),
                        ("name", Value::String(call.function.name.clone())),
                        ("arguments", arguments),
                    ],
                );
                let _ = ctx.out_tx.send(PluginOutgoing::event(body)).await;
            }
            let mut body = if errored {
                completion_event_body(
                    &ctx.args,
                    request_id,
                    "error",
                    [
                        (
                            "message",
                            Value::String(
                                final_error.unwrap_or_else(|| "completion failed".into()),
                            ),
                        ),
                        ("model", Value::String(active_model.clone())),
                        ("duration_ms", Value::Number(elapsed_ms.into())),
                        (
                            "stream_attempts",
                            Value::Number(final_stream_attempts.into()),
                        ),
                        (
                            "terminal_event_seen",
                            Value::Bool(final_terminal_event_seen),
                        ),
                    ],
                )
            } else {
                completion_event_body(
                    &ctx.args,
                    request_id,
                    "completed",
                    [
                        ("text", Value::String(final_text)),
                        ("reasoning", Value::String(final_reasoning)),
                        (
                            "finish_reason",
                            final_finish_reason
                                .map(Value::String)
                                .unwrap_or(Value::Null),
                        ),
                        ("model", Value::String(active_model.clone())),
                        ("duration_ms", Value::Number(elapsed_ms.into())),
                        (
                            "stream_attempts",
                            Value::Number(final_stream_attempts.into()),
                        ),
                        (
                            "terminal_event_seen",
                            Value::Bool(final_terminal_event_seen),
                        ),
                    ],
                )
            };
            if !errored && !interrupted && !final_native_output.is_empty() {
                body.insert(
                    "provider_context".into(),
                    response_output_context(
                        &ctx.args.provider_name,
                        &active_model,
                        &final_native_output,
                    ),
                );
            }
            let _ = ctx.out_tx.send(PluginOutgoing::event(body)).await;
            ctx.chats.end_turn(&chat_id).await;
            return;
        }

        let elapsed_ms = started.elapsed().as_millis() as u64;
        let _ = ctx
            .chats
            .record_turn(
                &chat_id,
                Some(&active_model),
                usage_to_record(
                    interrupted,
                    operation_usage.as_ref().and_then(OperationUsage::totals),
                ),
                elapsed_ms,
            )
            .await;

        let visible_final_text = if output_schema.is_some() {
            ""
        } else {
            &final_text
        };
        let body = stream_end_body(
            &ctx.args,
            &turn_id,
            &chat_id,
            visible_final_text,
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
            final_error.as_deref(),
        );
        let _ = ctx.out_tx.send(PluginOutgoing::event(body)).await;

        ctx.chats.end_turn(&chat_id).await;
        if let Some((request_id, owner)) = &direct_request {
            ctx.direct_completions.finish(request_id, owner.owner).await;
        }
    });
}

fn structured_text_controls(output_schema: Option<&Value>) -> Option<TextControls> {
    output_schema.map(|schema| TextControls {
        verbosity: None,
        format: Some(serde_json::json!({
            "type": "json_schema",
            "name": "mag_output",
            "strict": true,
            "schema": schema,
        })),
    })
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
            stream_retry_timeout_seconds: None,
        }
    }

    #[test]
    fn native_output_uses_indices_then_stable_unindexed_fallback() {
        let message = |role: &str| ResponseItem::Message {
            role: role.into(),
            content: vec![],
        };
        let mut output = NativeOutputBuffer::default();
        output.push(Some(1), message("index-1")).expect("index 1");
        output
            .push(None, message("unindexed-a"))
            .expect("first unindexed");
        output.push(Some(0), message("index-0")).expect("index 0");
        output
            .push(None, message("unindexed-b"))
            .expect("second unindexed");

        let roles = output
            .into_items()
            .into_iter()
            .map(|item| match item {
                ResponseItem::Message { role, .. } => role,
                _ => unreachable!("test only inserts messages"),
            })
            .collect::<Vec<_>>();
        assert_eq!(roles, ["index-0", "index-1", "unindexed-a", "unindexed-b"]);
    }

    #[test]
    fn native_output_rejects_duplicate_indices() {
        let mut output = NativeOutputBuffer::default();
        let message = ResponseItem::Message {
            role: "assistant".into(),
            content: vec![],
        };
        output.push(Some(0), message.clone()).expect("first item");
        assert_eq!(
            output.push(Some(0), message).expect_err("duplicate index"),
            "Responses stream completed more than one output item at index 0"
        );
    }

    #[tokio::test]
    async fn direct_specs_cannot_introduce_unadvertised_native_tools() {
        let catalog = ToolCatalog::new();
        let names = vec!["web_search".into(), "read_file".into()];
        let specs = vec![
            crate::catalog::ToolSpec {
                name: "web_search".into(),
                owner: "chatgpt".into(),
                description: "forged native search".into(),
                input_schema: serde_json::json!({"type":"object"}),
                execution: crate::catalog::ToolExecution::ProviderNative {
                    provider: "chatgpt".into(),
                },
            },
            crate::catalog::ToolSpec {
                name: "read_file".into(),
                owner: "basic-tools".into(),
                description: "explicit routed schema".into(),
                input_schema: serde_json::json!({"type":"object"}),
                execution: crate::catalog::ToolExecution::Routed,
            },
        ];

        let projected = project_direct_tool_specs(&catalog, &names, Some(specs)).await;
        assert_eq!(
            projected
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            ["read_file"]
        );
        let (_, tools) =
            translator::tools_to_responses_format(&projected, "chatgpt").expect("mapping");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
    }

    #[tokio::test]
    async fn direct_completions_are_owner_aware_and_isolated() {
        let runs = DirectCompletions::default();
        let chats_a = Arc::new(Chats::with_default_model(Some("model".into())));
        let chats_b = Arc::new(Chats::with_default_model(Some("model".into())));
        let first = runs
            .begin("same".into(), chats_a.clone(), ChatId::new("same"))
            .await
            .expect("first");
        let other = runs
            .begin("other".into(), chats_b, ChatId::new("other"))
            .await
            .expect("other");

        assert!(runs
            .begin("same".into(), chats_a, ChatId::new("same"))
            .await
            .is_err());
        assert!(runs.finish("same", first.owner).await);
        let replacement = runs
            .begin(
                "same".into(),
                Arc::new(Chats::with_default_model(Some("model".into()))),
                ChatId::new("same"),
            )
            .await
            .expect("safe reuse");
        assert!(
            !runs.finish("same", first.owner).await,
            "stale cleanup cannot remove replacement"
        );
        assert_eq!(
            runs.get("same").await.expect("replacement").owner,
            replacement.owner
        );
        assert_eq!(runs.get("other").await.expect("other").owner, other.owner);
    }

    #[tokio::test]
    async fn concurrent_requests_for_one_conversation_cancel_independently() {
        let runs = DirectCompletions::default();
        let conversation_id = "conversation-shared";
        let chats_a = Arc::new(Chats::with_default_model(Some("model".into())));
        let chats_b = Arc::new(Chats::with_default_model(Some("model".into())));
        let chat_a = ChatId::new("request-a");
        let chat_b = ChatId::new("request-b");
        for (chats, chat) in [(&chats_a, &chat_a), (&chats_b, &chat_b)] {
            chats
                .create(chat.clone(), None, None, None, None, None)
                .await
                .expect("request chat");
            chats
                .set_conversation_id(chat, conversation_id.into())
                .await
                .expect("shared conversation identity");
        }
        let token_a = chats_a.begin_turn(&chat_a).await.expect("turn a");
        let token_b = chats_b.begin_turn(&chat_b).await.expect("turn b");
        runs.begin("request-a".into(), chats_a, chat_a)
            .await
            .expect("request a");
        runs.begin("request-b".into(), chats_b, chat_b)
            .await
            .expect("request b");

        let cancelled = runs.cancel("request-a").await.expect("owned request a");
        assert!(cancelled.chats.cancel_turn(&cancelled.chat_id).await);
        assert!(token_a.is_cancelled());
        assert!(!token_b.is_cancelled());
        assert!(runs.get("request-a").await.is_none());
        assert!(runs.get("request-b").await.is_some());
    }

    #[test]
    fn conversation_identity_keeps_routing_and_cache_stable_across_requests() {
        let round_1 = ChatId::new("r7/worker.llm@r1");
        let round_2 = ChatId::new("r7/worker.llm@r2");
        let conversation_id = "conversation-stable";

        let route_1 = logical_routing_id(Some(conversation_id), &round_1);
        let route_2 = logical_routing_id(Some(conversation_id), &round_2);
        assert_eq!(route_1, conversation_id);
        assert_eq!(route_2, conversation_id);

        let provider_1 = provider_routing_identity(&route_1);
        let provider_2 = provider_routing_identity(&route_2);
        assert_eq!(provider_1, provider_2);
        let mut headers_1 = reqwest::header::HeaderMap::new();
        let mut headers_2 = reqwest::header::HeaderMap::new();
        crate::responses::headers::add_turn_headers(
            &mut headers_1,
            &provider_1.session_id,
            &provider_1.thread_id,
            None,
        )
        .expect("round 1 headers");
        crate::responses::headers::add_turn_headers(
            &mut headers_2,
            &provider_2.session_id,
            &provider_2.thread_id,
            None,
        )
        .expect("round 2 headers");
        assert_eq!(headers_1, headers_2);
        assert_eq!(
            provider_1.thread_id, provider_2.thread_id,
            "thread-id is also the stable prompt_cache_key"
        );
        assert_eq!(provider_1.session_id.len(), 36);
        assert_eq!(provider_1.thread_id.len(), 36);
        assert_ne!(provider_1.session_id, provider_1.thread_id);
        assert_eq!(
            uuid::Uuid::parse_str(&provider_1.session_id)
                .expect("provider session UUID")
                .get_version_num(),
            8
        );
        assert_eq!(
            uuid::Uuid::parse_str(&provider_1.thread_id)
                .expect("provider thread UUID")
                .get_version_num(),
            8
        );
        let sibling = provider_routing_identity("conversation-sibling");
        assert_ne!(provider_1.session_id, sibling.session_id);
        assert_ne!(provider_1.thread_id, sibling.thread_id);
    }

    #[test]
    fn provider_routing_is_bounded_for_long_logical_actor_ids() {
        let logical = format!("session-42/r7/{}.llm", "long-actor-name-".repeat(32));
        assert!(logical.len() > 64);

        let provider = provider_routing_identity(&logical);
        assert_eq!(provider.session_id.len(), 36);
        assert_eq!(provider.thread_id.len(), 36);
    }

    #[test]
    fn direct_provider_chats_fall_back_to_their_chat_id() {
        let chat_id = ChatId::new("direct-chat");
        assert_eq!(logical_routing_id(None, &chat_id), "direct-chat");
    }

    #[tokio::test]
    async fn chat_create_preserves_the_conversation_identity() {
        let chats = Arc::new(Chats::with_default_model(Some("test-model".into())));
        let (out_tx, mut out_rx) = mpsc::channel(4);
        let body = serde_json::json!({
            "chat_id": "r7/worker.llm@r2",
            "conversation_id": "conversation-stable"
        })
        .as_object()
        .expect("object")
        .clone();

        handle_chat_create(&args(), &chats, &out_tx, &body)
            .await
            .expect("create");
        let created = out_rx.recv().await.expect("created event").to_line();
        assert!(created.contains("chatgpt.chat.created"));

        let snapshot = chats
            .snapshot(&ChatId::new("r7/worker.llm@r2"))
            .await
            .expect("snapshot");
        assert_eq!(
            snapshot.conversation_id.as_deref(),
            Some("conversation-stable")
        );
    }

    #[test]
    fn mag_output_schema_uses_responses_native_text_format() {
        let schema = serde_json::json!({"type": "object"});
        let controls = structured_text_controls(Some(&schema)).unwrap();
        let format = controls.format.unwrap();
        assert_eq!(format["type"], "json_schema");
        assert_eq!(format["name"], "mag_output");
        assert_eq!(format["strict"], true);
        assert_eq!(format["schema"]["type"], "object");
        assert!(structured_text_controls(None).is_none());
    }

    #[test]
    fn chat_complete_result_threads_the_turn_error_detail() {
        let chat_id = ChatId::new("agent@r2");
        let body = chat_complete_result_body(
            &args(),
            &chat_id,
            "",
            &[],
            Some("error"),
            Some("HTTP 400: tool message without preceding tool_calls"),
        );
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("chatgpt.chat.complete.result")
        );
        assert_eq!(
            body.get("finish_reason").and_then(Value::as_str),
            Some("error")
        );
        // The detail rides in the output (what ProviderInput consumers read) and
        // top-level, so the single terminal result explains WHY the round died.
        let output = body
            .get("output")
            .and_then(Value::as_object)
            .expect("output");
        assert_eq!(
            output.get("error").and_then(Value::as_str),
            Some("HTTP 400: tool message without preceding tool_calls")
        );
        assert_eq!(
            body.get("error").and_then(Value::as_str),
            Some("HTTP 400: tool message without preceding tool_calls")
        );

        // A clean turn carries no error field at all.
        let clean = chat_complete_result_body(&args(), &chat_id, "hi", &[], Some("stop"), None);
        assert!(clean.get("error").is_none());
        let clean_output = clean
            .get("output")
            .and_then(Value::as_object)
            .expect("output");
        assert!(clean_output.get("error").is_none());
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
    fn auth_401_recovery_is_bounded_reload_then_refresh_then_fail() {
        assert_eq!(auth_401_action(0, true), Auth401Action::RetryReloaded);
        assert_eq!(auth_401_action(0, false), Auth401Action::ForceRefresh);
        assert_eq!(auth_401_action(1, false), Auth401Action::ForceRefresh);
        assert_eq!(auth_401_action(2, false), Auth401Action::Fail);
    }

    #[tokio::test]
    async fn auth_401_retry_rebuilds_headers_with_rotated_token() {
        use crate::auth::store::{
            save, AccessToken, AuthDotJson, ChatgptAccountId, RefreshToken, TokenData,
        };

        let server = tiny_http::Server::http("127.0.0.1:0").expect("server");
        let addr = server.server_addr().to_ip().expect("ip");
        let (headers_tx, headers_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            for step in 0..3 {
                let request = server.recv().expect("request");
                let authorization = request
                    .headers()
                    .iter()
                    .find(|header| header.field.equiv("authorization"))
                    .map(|header| header.value.as_str().to_owned());
                headers_tx.send(authorization).expect("record header");
                let response = match step {
                    0 => tiny_http::Response::from_string("unauthorized").with_status_code(401),
                    1 => tiny_http::Response::from_string(
                        r#"{"id_token":"h.e30.s","access_token":"fresh","refresh_token":"rotated"}"#,
                    )
                    .with_header(
                        "content-type: application/json"
                            .parse::<tiny_http::Header>()
                            .expect("header"),
                    ),
                    _ => tiny_http::Response::from_string("data: [DONE]\n\n").with_header(
                        "content-type: text/event-stream"
                            .parse::<tiny_http::Header>()
                            .expect("header"),
                    ),
                };
                request.respond(response).expect("respond");
            }
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        save(
            &auth_path,
            &AuthDotJson {
                tokens: TokenData {
                    id_token: "h.e30.s".into(),
                    access_token: AccessToken("stale".into()),
                    refresh_token: RefreshToken("refresh".into()),
                    account_id: Some(ChatgptAccountId("acct".into())),
                },
                last_refresh: chrono::Utc::now(),
            },
        )
        .expect("save");
        let auth = AuthStore::load_from_disk_with_refresh_url(
            &auth_path,
            format!("http://{addr}/oauth/token"),
        )
        .await
        .expect("auth");
        let client = ResponsesClient::with_http(
            reqwest::Client::builder().build().expect("HTTP client"),
            format!("http://{addr}"),
            "installation".into(),
            "test".into(),
        );
        let request = ResponsesApiRequest {
            model: "test".into(),
            instructions: String::new(),
            input: Vec::new(),
            tools: Vec::new(),
            tool_choice: "auto".into(),
            parallel_tool_calls: false,
            reasoning: None,
            store: false,
            stream: true,
            include: Vec::new(),
            service_tier: None,
            prompt_cache_key: None,
            text: None,
        };

        let first = auth.snapshot().await;
        assert!(matches!(
            client
                .stream(
                    &request,
                    &first,
                    &mut ResponsesTurnContext::new("test-session", "test-thread"),
                )
                .await,
            Err(ChatgptError::ResponsesEndpoint { status: 401, .. })
        ));
        let failed_token = first.tokens.expect("tokens").access_token;
        auth.force_refresh_after(&failed_token)
            .await
            .expect("refresh");
        client
            .stream(
                &request,
                &auth.snapshot().await,
                &mut ResponsesTurnContext::new("test-session", "test-thread"),
            )
            .await
            .expect("retry");
        server_thread.join().expect("server thread");

        assert_eq!(
            headers_rx.into_iter().collect::<Vec<_>>(),
            vec![
                Some("Bearer stale".into()),
                None,
                Some("Bearer fresh".into())
            ]
        );
    }

    #[test]
    fn media_tool_output_becomes_user_visible_error_for_text_model() {
        let output = serde_json::json!({
            "type": "media",
            "media_type": "image/png",
            "filename": "diagram.png",
            "data": "abc"
        });
        let text = tool_output_for_text_model(&output);
        assert_eq!(
            text,
            "ERROR: Cannot read \"diagram.png\" (this model does not support image input). Inform the user."
        );
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
                supports_parallel_tool_calls: true,
                context_length: None,
            },
            ModelEntry {
                slug: "gpt-5-codex".into(),
                display_name: Some("GPT-5 Codex".into()),
                description: Some("coding model".into()),
                priority: Some(20),
                supports_reasoning_summaries: false,
                supports_parallel_tool_calls: false,
                context_length: None,
            },
        ];
        let body = models_listed_body(&args(), &fetched);
        let models = body.get("models").and_then(Value::as_array).expect("array");
        let slugs: Vec<&str> = models.iter().filter_map(Value::as_str).collect();
        assert_eq!(slugs, vec!["gpt-5", "gpt-5-codex"]);
        let windows = body
            .get("context_windows")
            .and_then(Value::as_object)
            .expect("provider-owned context windows");
        assert_eq!(windows.get("gpt-5").and_then(Value::as_u64), Some(272_000));
        assert_eq!(
            windows.get("gpt-5-codex").and_then(Value::as_u64),
            Some(272_000)
        );
    }

    #[test]
    fn missing_sol_metadata_uses_the_full_provider_window() {
        let model = ModelEntry {
            slug: "gpt-5.6-sol".into(),
            display_name: None,
            description: None,
            priority: None,
            supports_reasoning_summaries: true,
            supports_parallel_tool_calls: true,
            context_length: None,
        };
        assert_eq!(provider_context_window(&model), Some(272_000));
    }

    #[test]
    fn explicit_upstream_context_window_takes_precedence() {
        let model = ModelEntry {
            slug: "gpt-5.6-sol".into(),
            display_name: None,
            description: None,
            priority: None,
            supports_reasoning_summaries: true,
            supports_parallel_tool_calls: true,
            context_length: Some(999_999),
        };
        assert_eq!(provider_context_window(&model), Some(999_999));
    }

    #[test]
    fn known_missing_metadata_uses_model_specific_provider_metadata() {
        let model = ModelEntry {
            slug: "gpt-oss-120b".into(),
            display_name: None,
            description: None,
            priority: None,
            supports_reasoning_summaries: false,
            supports_parallel_tool_calls: true,
            context_length: None,
        };
        assert_eq!(provider_context_window(&model), Some(128_000));
    }

    #[test]
    fn unknown_missing_metadata_is_not_advertised() {
        let unknown = ModelEntry {
            slug: "future-model".into(),
            display_name: None,
            description: None,
            priority: None,
            supports_reasoning_summaries: false,
            supports_parallel_tool_calls: true,
            context_length: None,
        };
        assert_eq!(provider_context_window(&unknown), None);

        let body = models_listed_body(&args(), &[unknown]);
        assert!(body
            .get("models")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty));
        assert!(body.get("context_windows").is_none());
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
    fn completion_checkpoint_is_provider_and_model_owned() {
        let context = serde_json::json!({
            "compaction": { "checkpoint": {
                "provider": "chatgpt",
                "format": "chatgpt.responses.compaction.v1",
                "model": "gpt-5.6-sol",
                "artifact": { "items": [{
                    "type": "compaction",
                    "encrypted_content": "sealed"
                }] }
            }}
        });
        assert_eq!(
            completion_checkpoint(Some(&context), "chatgpt", Some("gpt-5.6-sol"))
                .expect("compatible checkpoint")
                .len(),
            1
        );
        assert!(completion_checkpoint(Some(&context), "other", Some("gpt-5.6-sol")).is_none());
        assert!(completion_checkpoint(Some(&context), "chatgpt", Some("other-model")).is_none());
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
    fn tool_call_buffer_reverse_maps_streamed_provider_name() {
        let specs = vec![crate::catalog::ToolSpec {
            name: "process.exec".into(),
            owner: "basic-tools".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            execution: crate::catalog::ToolExecution::Routed,
        }];
        let names =
            crate::provider_tool_names::ProviderToolNames::from_specs(&specs).expect("mapping");
        let provider_name = names.to_provider("process.exec").expect("provider name");
        let mut b = ToolCallBuffer::default();
        b.on_item_added(
            "call_1".into(),
            "call_1".into(),
            provider_name.into(),
            String::new(),
        );
        b.on_args_delta(Some("call_1"), r#"{"pa"#);
        b.on_args_delta(Some("call_1"), r#"th":"/x"}"#);
        b.on_item_done(Some("call_1"), r#"{"path":"/x"}"#);
        let calls = b.into_tool_calls(&names).expect("known name");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "process.exec");
        assert_eq!(calls[0].function.arguments, r#"{"path":"/x"}"#);
    }

    #[test]
    fn tool_call_buffer_prefers_done_args_when_longer() {
        // Some models skip the deltas and send the full args on done.
        let mut b = ToolCallBuffer::default();
        b.on_item_added("call_1".into(), "call_1".into(), "x".into(), String::new());
        b.on_item_done(Some("call_1"), r#"{"a":1}"#);
        let specs = vec![crate::catalog::ToolSpec {
            name: "x".into(),
            owner: "basic-tools".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            execution: crate::catalog::ToolExecution::Routed,
        }];
        let names =
            crate::provider_tool_names::ProviderToolNames::from_specs(&specs).expect("mapping");
        let calls = b.into_tool_calls(&names).expect("known name");
        assert_eq!(calls[0].function.arguments, r#"{"a":1}"#);
    }

    #[test]
    fn reasoning_summary_formatter_formats_adjacent_titles_and_detail() {
        let mut f = ReasoningSummaryFormatter::default();
        let out = f.push_delta(
            "**Discussing design considerations**\
             **Exploring validation abstractions**\
             **Planning compiler validation flow**\
             I'm working through validated data structures.\
             **Designing checked value compilation flow**",
        );
        assert_eq!(
            out.concat(),
            concat!(
                "1. Discussing design considerations\n",
                "2. Exploring validation abstractions\n",
                "3. Planning compiler validation flow\n",
                "   I'm working through validated data structures.\n",
                "4. Designing checked value compilation flow\n",
            )
        );
    }

    #[test]
    fn reasoning_summary_formatter_buffers_split_title_markers() {
        let mut f = ReasoningSummaryFormatter::default();
        assert!(f.push_delta("**Clarifying validator").is_empty());
        assert_eq!(
            f.push_delta(" sequencing and safety**detail").concat(),
            "1. Clarifying validator sequencing and safety\n   detail"
        );
    }

    #[test]
    fn reasoning_summary_formatter_flushes_partial_streaming_title() {
        let mut f = ReasoningSummaryFormatter::default();
        assert_eq!(
            f.push_delta("**Done**body**Partial").concat(),
            "1. Done\n   body"
        );
        assert_eq!(f.finish().concat(), "\n2. Partial\n");
    }

    #[tokio::test]
    async fn graceful_interrupt_with_unmeasured_usage_preserves_prior_token_stats() {
        for response in [
            serde_json::json!({"usage": null}),
            serde_json::json!({"usage": {"input_tokens": "unknown", "output_tokens": []}}),
        ] {
            let chats = Chats::with_default_model(Some("fallback".into()));
            let chat_id = ChatId::new("interrupted");
            chats
                .create(chat_id.clone(), None, None, None, None, None)
                .await
                .expect("create");
            chats
                .record_turn(&chat_id, Some("gpt-5"), Some((17, 9)), 123)
                .await
                .expect("seed stats");

            let measured_usage = parsed_token_usage(&response)
                .map(|usage| (usage.input_tokens, usage.output_tokens));
            chats
                .record_turn(
                    &chat_id,
                    Some("gpt-5"),
                    usage_to_record(true, measured_usage),
                    456,
                )
                .await
                .expect("record interrupted turn");

            let stats = chats.stats_snapshot(&chat_id).await.expect("stats");
            assert_eq!(stats.turns_completed, 2);
            assert_eq!(stats.cumulative_input_tokens, 17);
            assert_eq!(stats.cumulative_output_tokens, 9);
            assert_eq!(stats.last_turn_input_tokens, 17);
            assert_eq!(stats.last_turn_output_tokens, 9);
            assert_eq!(stats.last_turn_duration_ms, Some(456));
            assert_eq!(stats.model.as_deref(), Some("gpt-5"));
        }
    }

    fn parsed_usage(
        input: u64,
        output: u64,
        cached: Option<u64>,
        cache_write: Option<u64>,
        reasoning: Option<u64>,
        service_tier: Option<&str>,
    ) -> ParsedTokenUsage {
        ParsedTokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: cached,
            cache_write_input_tokens: cache_write,
            reasoning_tokens: reasoning,
            service_tier: service_tier.map(str::to_owned),
        }
    }

    fn field<'a>(fields: &'a [(&str, Value)], name: &str) -> Option<&'a Value> {
        fields
            .iter()
            .find_map(|(key, value)| (*key == name).then_some(value))
    }

    #[test]
    fn operation_usage_preserves_request_pricing_evidence_and_final_context() {
        let first = parsed_usage(271_999, 7, Some(20), None, Some(2), Some("standard"));
        let second = parsed_usage(272_001, 11, Some(30), Some(0), Some(4), Some("priority"));
        let final_iteration = parsed_usage(105, 4, Some(0), Some(17), Some(1), Some("priority"));
        let usage = OperationUsage::record_iteration(None, Some(first));
        let usage = OperationUsage::record_iteration(Some(usage), Some(second));
        let usage = OperationUsage::record_iteration(Some(usage), Some(final_iteration));

        assert_eq!(usage.input_tokens, 544_105);
        assert_eq!(usage.output_tokens, 22);
        assert_eq!(usage.context_input_tokens, Some(105));
        assert_eq!(usage.totals(), Some((544_105, 22)));
        let fields = usage.completion_fields("chatgpt", "gpt-test", 42);
        assert_eq!(
            field(&fields, "input_tokens").and_then(Value::as_u64),
            Some(544_105)
        );
        assert_eq!(
            field(&fields, "context_input_tokens").and_then(Value::as_u64),
            Some(105)
        );
        assert_eq!(
            field(&fields, "cache_read_input_tokens").and_then(Value::as_u64),
            Some(50)
        );
        assert!(field(&fields, "cache_write_input_tokens").is_none());
        assert_eq!(
            field(&fields, "reasoning_tokens").and_then(Value::as_u64),
            Some(7)
        );
        assert!(field(&fields, "service_tier").is_none());

        let components = field(&fields, "billing_components")
            .and_then(Value::as_array)
            .expect("per-request billing evidence");
        assert_eq!(components.len(), 3);
        assert_eq!(components[0]["input_tokens"], 271_999);
        assert_eq!(components[0]["service_tier"], "standard");
        assert!(components[0].get("cache_write_input_tokens").is_none());
        assert_eq!(components[1]["input_tokens"], 272_001);
        assert_eq!(components[1]["service_tier"], "priority");
        assert_eq!(components[1]["cache_write_input_tokens"], 0);
        assert_eq!(components[2]["cache_write_input_tokens"], 17);
    }

    #[test]
    fn unmeasured_final_request_makes_totals_and_final_context_inexact() {
        let first = parsed_usage(271_999, 7, Some(20), None, Some(2), Some("standard"));
        let usage = OperationUsage::record_iteration(None, Some(first));
        let usage = OperationUsage::record_iteration(Some(usage), None);
        let fields = usage.completion_fields("chatgpt", "gpt-test", 42);

        assert_eq!(usage.totals(), None);
        assert_eq!(
            field(&fields, "billing_components_complete"),
            Some(&Value::Bool(false))
        );
        assert!(field(&fields, "input_tokens").is_none());
        assert!(field(&fields, "context_input_tokens").is_none());
        let components = field(&fields, "billing_components")
            .and_then(Value::as_array)
            .expect("request evidence");
        assert_eq!(components.len(), 2);
        assert_eq!(components[0]["usage_available"], true);
        assert_eq!(components[1]["usage_available"], false);
    }

    #[test]
    fn unmeasured_retry_keeps_measured_final_context_separate_from_billing_totals() {
        let usage = OperationUsage::record_iteration(None, None);
        let final_iteration = parsed_usage(105, 4, Some(0), Some(17), Some(1), Some("priority"));
        let usage = OperationUsage::record_iteration(Some(usage), Some(final_iteration));
        let fields = usage.completion_fields("chatgpt", "gpt-test", 42);

        assert_eq!(usage.totals(), None);
        assert!(field(&fields, "input_tokens").is_none());
        assert_eq!(
            field(&fields, "context_input_tokens").and_then(Value::as_u64),
            Some(105)
        );
        let components = field(&fields, "billing_components")
            .and_then(Value::as_array)
            .expect("request evidence");
        assert_eq!(components[0]["usage_available"], false);
        assert_eq!(components[1]["usage_available"], true);
    }

    #[test]
    fn operation_usage_omits_overflowed_aggregate_totals() {
        let first = OperationUsage::record_iteration(
            None,
            Some(parsed_usage(
                u64::MAX,
                u64::MAX,
                Some(u64::MAX),
                Some(u64::MAX),
                Some(u64::MAX),
                None,
            )),
        );
        let usage = OperationUsage::record_iteration(
            Some(first),
            Some(parsed_usage(1, 1, Some(1), Some(1), Some(1), None)),
        );
        let fields = usage.completion_fields("chatgpt", "gpt-test", 1);

        assert_eq!(usage.totals(), None);
        assert_eq!(
            field(&fields, "billing_components_complete"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            field(&fields, "aggregate_totals_exact"),
            Some(&Value::Bool(false))
        );
        assert!(field(&fields, "input_tokens").is_none());
        assert!(field(&fields, "total_tokens").is_none());
        let components = field(&fields, "billing_components")
            .and_then(Value::as_array)
            .expect("exact components survive aggregate overflow");
        assert_eq!(components[0]["input_tokens"], u64::MAX);
        assert_eq!(components[1]["input_tokens"], 1);
    }

    #[test]
    fn parsed_token_usage_preserves_zero_details_and_effective_tier() {
        let response = serde_json::json!({
            "service_tier": "priority",
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0}
            }
        });
        let measured = parsed_token_usage(&response).expect("measured usage");
        assert_eq!(measured.cache_read_input_tokens, Some(0));
        assert_eq!(measured.cache_write_input_tokens, Some(0));
        assert_eq!(measured.reasoning_tokens, Some(0));
        assert_eq!(measured.service_tier.as_deref(), Some("priority"));
        assert_eq!(
            usage_to_record(true, Some((measured.input_tokens, measured.output_tokens))),
            Some((0, 0))
        );
    }

    #[test]
    fn parsed_token_usage_preserves_nonzero_cache_write_tokens() {
        let response = serde_json::json!({
            "usage": {
                "input_tokens": 20,
                "output_tokens": 3,
                "input_tokens_details": {"cache_write_tokens": 17}
            }
        });
        let measured = parsed_token_usage(&response).expect("measured usage");
        assert_eq!(measured.cache_write_input_tokens, Some(17));
    }

    #[test]
    fn parsed_token_usage_omits_missing_or_malformed_details() {
        for response in [
            serde_json::json!({"usage": {"input_tokens": 1, "output_tokens": 2}}),
            serde_json::json!({
                "service_tier": 7,
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 2,
                    "input_tokens_details": {
                        "cached_tokens": "unknown", "cache_write_tokens": "unknown"
                    },
                    "output_tokens_details": []
                }
            }),
        ] {
            let usage = parsed_token_usage(&response).expect("aggregate usage remains valid");
            assert_eq!(usage.cache_read_input_tokens, None);
            assert_eq!(usage.cache_write_input_tokens, None);
            assert_eq!(usage.reasoning_tokens, None);
            assert_eq!(usage.service_tier, None);
        }
    }

    #[test]
    fn stream_retry_policy_replays_provisional_state_until_an_explicit_bound() {
        let failure = StreamFailure::from_error(ChatgptError::ResponsesStreamRead("reset".into()));
        assert_eq!(
            decide_pre_output_retry(
                &failure,
                1,
                Duration::ZERO,
                Duration::from_millis(200),
                StreamRetryPolicy::default(),
            ),
            RetryDecision {
                retry: true,
                reason: RetryDecisionReason::RetryableProvisionalFailure,
            }
        );
        assert_eq!(
            decide_pre_output_retry(
                &failure,
                3,
                Duration::ZERO,
                Duration::from_millis(200),
                StreamRetryPolicy {
                    max_attempts: Some(3),
                    budget: None,
                },
            )
            .reason,
            RetryDecisionReason::AttemptLimitReached,
        );
        assert_eq!(
            decide_pre_output_retry(
                &failure,
                1,
                Duration::from_secs(30),
                Duration::ZERO,
                StreamRetryPolicy {
                    max_attempts: None,
                    budget: Some(Duration::from_secs(30)),
                },
            )
            .reason,
            RetryDecisionReason::ElapsedBudgetExhausted,
        );

        let parse = StreamFailure::from_error(ChatgptError::ResponsesStreamParse("bad".into()));
        assert_eq!(
            decide_pre_output_retry(
                &parse,
                1,
                Duration::ZERO,
                Duration::ZERO,
                StreamRetryPolicy::default(),
            )
            .reason,
            RetryDecisionReason::FailureNotRetryable,
        );
    }

    #[test]
    fn semantic_retry_classification_is_explicit_and_backoff_is_short() {
        let overloaded = serde_json::json!({
            "error": {
                "message": "Our servers are currently overloaded. Please try again later.",
                "type": "server_error",
                "code": "service_unavailable"
            }
        });
        let failure = StreamFailure::semantic(&overloaded);
        assert_eq!(failure.kind, StreamFailureKind::Semantic);
        assert!(failure.retryable);
        assert_eq!(
            failure.message,
            "Our servers are currently overloaded. Please try again later."
        );

        let invalid = serde_json::json!({
            "error": {
                "message": "Unsupported parameter",
                "type": "invalid_request_error",
                "code": "unsupported_parameter"
            }
        });
        assert!(!StreamFailure::semantic(&invalid).retryable);

        assert_eq!(
            pre_output_retry_delay_ceiling(1),
            Duration::from_millis(PRE_OUTPUT_RETRY_BASE_DELAY_MS),
        );
        assert_eq!(
            pre_output_retry_delay_ceiling(2),
            Duration::from_millis(PRE_OUTPUT_RETRY_BASE_DELAY_MS * 2),
        );
        assert_eq!(
            pre_output_retry_delay_ceiling(99),
            Duration::from_millis(PRE_OUTPUT_RETRY_MAX_DELAY_MS),
        );
    }

    #[tokio::test]
    async fn provider_recovery_gate_admits_one_half_open_probe() {
        let gate = Arc::new(ProviderRecoveryGate::default());
        let cancel = TurnToken::new();
        gate.record_failure(Duration::from_millis(10)).await;

        assert!(gate.admit(&cancel).await, "first request becomes the probe");
        let waiting_gate = gate.clone();
        let waiting_cancel = TurnToken::new();
        let waiter = tokio::spawn(async move { waiting_gate.admit(&waiting_cancel).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), waiter)
                .await
                .is_err(),
            "a second request remains gated while the probe is in flight"
        );

        gate.record_success().await;
        assert!(
            gate.admit(&cancel).await,
            "success reopens normal admission"
        );
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

    #[test]
    fn compatible_message_provider_context_restores_native_output_items() {
        let message = serde_json::json!({
            "role": "assistant",
            "content": "visible summary",
            "provider_context": response_output_context(
                "chatgpt",
                "gpt-5.6-sol",
                &[
                    ResponseItem::Reasoning {
                        id: Some("rs_1".into()),
                        encrypted_content: Some("sealed-plan".into()),
                        summary: vec![],
                    },
                    ResponseItem::WebSearchCall {
                        id: Some("ws_1".into()),
                        status: Some("completed".into()),
                        action: Some(crate::responses::request::WebSearchAction {
                            kind: Some("search".into()),
                            query: Some("native search".into()),
                            queries: None,
                            url: None,
                            pattern: None,
                            extra: Map::new(),
                        }),
                        extra: Map::new(),
                    },
                    ResponseItem::Message {
                        role: "assistant".into(),
                        content: vec![],
                    },
                ],
            ),
        });

        let items = message_provider_context_items(&message, "chatgpt", Some("gpt-5.6-sol"))
            .expect("valid envelope")
            .expect("compatible artifact");
        assert!(matches!(
            &items[0],
            ResponseItem::Reasoning { encrypted_content: Some(content), .. }
                if content == "sealed-plan"
        ));
        assert!(matches!(
            &items[1],
            ResponseItem::WebSearchCall { id: Some(id), .. } if id == "ws_1"
        ));
        assert!(matches!(&items[2], ResponseItem::Message { .. }));
        assert_eq!(items.len(), 3);

        assert!(
            message_provider_context_items(&message, "other", Some("gpt-5.6-sol"))
                .expect("provider switch falls back")
                .is_none()
        );
        assert!(
            message_provider_context_items(&message, "chatgpt", Some("other-model"))
                .expect("model switch falls back")
                .is_none()
        );
    }

    #[test]
    fn malformed_compatible_provider_context_fails_instead_of_losing_reasoning() {
        let message = serde_json::json!({
            "role": "assistant",
            "provider_context": {
                "provider": "chatgpt",
                "format": RESPONSE_OUTPUT_CONTEXT_FORMAT,
                "model": "gpt-5.6-sol",
                "artifact": { "items": [{"type": "unknown_future_item"}] },
            },
        });
        let error = message_provider_context_items(&message, "chatgpt", Some("gpt-5.6-sol"))
            .expect_err("compatible corruption must be explicit");
        assert!(error.contains("invalid items"));
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
