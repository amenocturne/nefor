//! openai-provider — generic NCP v0.1 plugin for OpenAI-compatible
//! chat-completions endpoints. One binary, N spawns: NCP can launch
//! this same executable under several plugin names, each with its own
//! `--name` CLI flag so the per-instance event-kind prefix
//! (`ollama.*`, `groq.*`, `openrouter.*`, …) doesn't collide on the bus.
//!
//! Stage 1 reshape (per nefor-agent-and-reasoner-types §2 "Layer A —
//! Providers"): the singleton `SessionState` is gone. The provider now
//! manages **chats** as a chat-id-keyed map (`state::Chats`). Each chat
//! is `(model, message_history, in-flight slot, stats)`. The provider
//! can hold N concurrent chats — exactly the shape the parent spec
//! requires from a "dumb runner that speaks one wire protocol."
//!
//! ### Wire-level chat API (new)
//!
//! - `<prefix>.chat.create  { chat_id, model? }` — make a new chat;
//!   errors if `chat_id` already exists.
//! - `<prefix>.chat.append  { chat_id, message }` — append a message
//!   (typically the user's turn). No upstream call.
//! - `<prefix>.chat.complete { chat_id }` — send the chat's history to
//!   the upstream model, stream deltas, append the assistant message,
//!   reply with `<prefix>.chat.complete.result { chat_id, output }`
//!   where `output` follows `generic-provider.ProviderOut`'s shape.
//! - `<prefix>.chat.delete  { chat_id }` — drop the chat.
//!
//! ### Legacy wire API (compat for nefor-chat)
//!
//! - `<prefix>.prompt { text }` — operates on the per-prefix default
//!   chat (`<prefix>:default`); ensure-default + append-user +
//!   complete in one shot. The chat plugin keeps using this until T7
//!   rewires it to drive `chat.*` directly.
//! - `<prefix>.interrupt`, `<prefix>.reset` — operate on the default
//!   chat for the same reason.
//! - `<prefix>.auth.set`, `<prefix>.login_requested`,
//!   `<prefix>.logout_requested`, `<prefix>.model.set`,
//!   `<prefix>.models.list_requested` — provider-wide; not chat-scoped.
//!
//! ### Combinators registration
//!
//! On startup we declare two bare types (`RawRequest`, `RawResponse`)
//! and two `Into` conversions against `generic-provider`'s canonical
//! types (`ProviderIn`, `ProviderOut`). See the `register_body`
//! constructor for the literal shape and the spec gap that this plugin
//! intentionally exercises (`Into.in` cross-namespace).

mod error;
mod ncp;

use std::sync::Arc;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody};
use openai_provider::auth::{AuthSnapshot, AuthState, AuthStore, LogoutOutcome};
use openai_provider::broker::{ToolBroker, ToolResult};
use openai_provider::catalog::ToolCatalog;
use openai_provider::config::Config;
use openai_provider::openai::{Message, ModelInfo, ToolCall};
use openai_provider::state::{ChatId, ChatStats, Chats, ChatsError};
use openai_provider::stream::{list_models, run_chat_stream, ReasoningEvent, StreamError};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::error::LlmError;

const NO_LOGIN_FLOW_MESSAGE: &str =
    "openai-provider has no built-in login flow — wire up an auth plugin (e.g. anthropic-auth) and have it push <prefix>.auth.set events";

const LOGOUT_REFUSED_ENV_MESSAGE: &str =
    "no login to revoke — credentials come from --api-key (or OPENAI_PROVIDER_API_KEY env var); restart the plugin without it to clear";

const HTTP_401_MESSAGE: &str =
    "auth failed (HTTP 401) — re-login or check --api-key / OPENAI_PROVIDER_API_KEY";

const PROTOCOL_VERSION: &str = "0.1";
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Cap on tool-call iterations per turn. The model can in principle
/// loop forever asking for tools; this prevents one buggy or stuck
/// session from becoming a runaway. 20 matches a typical agent
/// budget — generous for legit work, tight enough to fail fast.
const TOOL_LOOP_MAX_ITERATIONS: u32 = 20;

/// Hard cap on how long we'll wait for a `tool.result` before giving up
/// on the in-flight tool call. Keeps a hung tool plugin from wedging
/// the provider's turn slot indefinitely. The token-based cancel
/// (`<prefix>.interrupt`) still fires earlier when the user hits Esc.
const TOOL_RESULT_TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(e) = run().await {
        tracing::error!(error = %e, "openai-provider exited with error");
        eprintln!("openai-provider: {e}");
        std::process::exit(1);
    }
    // Match mock-plugin / nefor-tui: force-exit so the parked stdin
    // reader doesn't hang the process on shutdown.
    std::process::exit(0);
}

async fn run() -> Result<(), LlmError> {
    let config = Config::from_args();
    let client = reqwest::Client::builder()
        .build()
        .expect("reqwest client build");

    let (out_tx, _writer_handle) = ncp::spawn_stdout_writer();
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, LlmError>>(ncp::CHANNEL_CAP);
    let _reader_handle = ncp::spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = ncp::await_ready_ok(&mut in_rx).await?;
    tracing::info!(
        engine_version = %engine_version,
        provider = %config.provider_name,
        model = ?config.model,
        base_url = %config.base_url,
        "ready"
    );

    send_event(&out_tx, hello_body(&config)).await?;
    // Combinators registration — declare our bare types and Into entries
    // against generic-provider's canonical types. See `register_body` for
    // the spec gap this exercises intentionally.
    send_event(&out_tx, register_body()).await?;
    send_event(&out_tx, ready_body(&config)).await?;

    let auth = Arc::new(AuthStore::from_env_key(config.api_key.clone()));
    let initial_snap = auth.snapshot().await;
    send_event(&out_tx, auth_status_body(&config, &initial_snap)).await?;

    let chats = Arc::new(Chats::with_default_model(config.model.clone()));
    let catalog = Arc::new(ToolCatalog::new());
    let broker = Arc::new(ToolBroker::new());

    run_dispatch_loop(
        &chats, &auth, &catalog, &broker, &config, &client, &out_tx, &mut in_rx,
    )
    .await?;

    let _ = out_tx
        .send(PluginOutgoing::event(goodbye_body(&config)))
        .await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_dispatch_loop(
    chats: &Arc<Chats>,
    auth: &Arc<AuthStore>,
    catalog: &Arc<ToolCatalog>,
    broker: &Arc<ToolBroker>,
    config: &Config,
    client: &reqwest::Client,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    in_rx: &mut mpsc::Receiver<Result<Envelope, LlmError>>,
) -> Result<(), LlmError> {
    loop {
        tokio::select! {
            maybe = in_rx.recv() => {
                match maybe {
                    Some(Ok(env)) => match &env.body {
                        Body::System(SystemBody::Shutdown { .. }) => {
                            tracing::info!("shutdown received");
                            chats.interrupt_all().await;
                            return Ok(());
                        }
                        Body::System(_) => {
                            tracing::warn!(?env, "unexpected system envelope after handshake");
                        }
                        Body::Event(map) => {
                            dispatch_event(
                                chats, auth, catalog, broker, config, client, out_tx,
                                &env.from, map,
                            )
                            .await?;
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
                chats.interrupt_all().await;
                return Ok(());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_event(
    chats: &Arc<Chats>,
    auth: &Arc<AuthStore>,
    catalog: &Arc<ToolCatalog>,
    broker: &Arc<ToolBroker>,
    config: &Config,
    client: &reqwest::Client,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    from: &PluginName,
    body: &Map<String, Value>,
) -> Result<(), LlmError> {
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(k) => k,
        None => return Ok(()),
    };

    // Non-prefixed cross-plugin events first: tool.register and
    // tool.result. These don't carry our provider-prefix because they
    // are part of the plugin-layer chat-contract, not provider-internal.
    match kind {
        "tool.register" => {
            let tools = body
                .get("tools")
                .map(ToolCatalog::parse_tools)
                .unwrap_or_default();
            let from_str = from.as_str().to_owned();
            tracing::info!(plugin = %from_str, count = tools.len(), "tool.register");
            catalog.register_from(&from_str, tools).await;
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
            let delivered = broker
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

    let prefix = config.event_prefix();
    let suffix = match kind.strip_prefix(&prefix) {
        Some(s) => s,
        None => return Ok(()),
    };
    match suffix {
        // ---- new explicit chat.* API -----------------------------------
        "chat.create" => {
            let chat_id = match read_chat_id(body) {
                Some(id) => id,
                None => {
                    send_event(
                        out_tx,
                        turn_error_body(config, "chat.create missing `chat_id`"),
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
            // `tools` accepts two shapes:
            //   - bool      → on/off switch (existing semantics).
            //                 false = omit tools array entirely.
            //   - [string]  → per-chat name allowlist. The catalog stays
            //                 process-wide; this filters which entries
            //                 the chat sees in its per-turn `tools` array.
            //                 Used by the lead orchestrator (limited to
            //                 orchestration tools) and the agent reasoner
            //                 (limited to per-role tool surface).
            // The two are independent: an array implicitly means "tools
            // on" (we don't accept the array AND tools_enabled=false on
            // the same envelope; the array wins). Anything else is
            // ignored — same as before.
            let tools_field = body.get("tools");
            let tools_enabled = tools_field.and_then(Value::as_bool);
            let tool_allowlist: Option<Vec<String>> =
                tools_field.and_then(Value::as_array).map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                });
            tracing::info!(
                target: "openai_provider::chat",
                chat_id = %chat_id,
                model = ?model,
                tools_enabled = ?tools_enabled,
                tool_allowlist_len = tool_allowlist.as_ref().map(Vec::len),
                "chat.create",
            );
            match chats
                .create(chat_id.clone(), model, tools_enabled, tool_allowlist)
                .await
            {
                Ok(()) => {
                    send_event(out_tx, chat_created_body(config, &chat_id)).await?;
                }
                Err(e) => {
                    send_event(out_tx, chat_error_body(config, &chat_id, &e)).await?;
                }
            }
        }
        "chat.append" => {
            let chat_id = match read_chat_id(body) {
                Some(id) => id,
                None => {
                    send_event(
                        out_tx,
                        turn_error_body(config, "chat.append missing `chat_id`"),
                    )
                    .await?;
                    return Ok(());
                }
            };
            let message = match parse_provider_message(body.get("message")) {
                Ok(m) => m,
                Err(msg) => {
                    send_event(out_tx, chat_error_body_msg(config, &chat_id, msg)).await?;
                    return Ok(());
                }
            };
            tracing::info!(
                target: "openai_provider::chat",
                chat_id = %chat_id,
                role = %message.role,
                content_len = message.content.as_deref().map(str::len).unwrap_or(0),
                content_preview = %message
                    .content
                    .as_deref()
                    .map(|s| s.chars().take(80).collect::<String>())
                    .unwrap_or_default(),
                "chat.append",
            );
            if let Err(e) = chats.append(&chat_id, message).await {
                send_event(out_tx, chat_error_body(config, &chat_id, &e)).await?;
            } else {
                send_event(out_tx, chat_appended_body(config, &chat_id)).await?;
            }
        }
        "chat.complete" => {
            let chat_id = match read_chat_id(body) {
                Some(id) => id,
                None => {
                    send_event(
                        out_tx,
                        turn_error_body(config, "chat.complete missing `chat_id`"),
                    )
                    .await?;
                    return Ok(());
                }
            };
            // Per-firing schemas appended to the global ToolCatalog tools
            // for this turn only. The agent reasoner uses this to inject
            // its synthetic `finalize` terminator without polluting the
            // catalog. Each entry must already be in the OpenAI tool
            // wire shape: `{type:"function", function:{name,description,parameters}}`.
            // Non-array / malformed payloads are silently dropped so a
            // misshaped emit can't crash the dispatch.
            let extra_tools: Vec<Value> = body
                .get("extra_tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            tracing::info!(
                target: "openai_provider::chat",
                chat_id = %chat_id,
                extra_tools_count = extra_tools.len(),
                "chat.complete",
            );
            start_completion_turn(
                chats,
                auth,
                catalog,
                broker,
                config,
                client,
                out_tx,
                chat_id,
                false,
                extra_tools,
            )
            .await?;
        }
        "chat.delete" => {
            let chat_id = match read_chat_id(body) {
                Some(id) => id,
                None => {
                    send_event(
                        out_tx,
                        turn_error_body(config, "chat.delete missing `chat_id`"),
                    )
                    .await?;
                    return Ok(());
                }
            };
            // Cancel any in-flight turn before drop so its background
            // task notices and exits cleanly.
            chats.interrupt(&chat_id).await;
            match chats.delete(&chat_id).await {
                Ok(()) => {
                    send_event(out_tx, chat_deleted_body(config, &chat_id)).await?;
                }
                Err(e) => {
                    send_event(out_tx, chat_error_body(config, &chat_id, &e)).await?;
                }
            }
        }

        // ---- legacy default-chat compat path --------------------------
        "prompt" => {
            let text = body
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if text.is_empty() {
                send_event(
                    out_tx,
                    turn_error_body(config, "openai-provider: prompt text must be non-empty"),
                )
                .await?;
                return Ok(());
            }
            let chat_id = ChatId::default_for_prefix(&prefix);
            if let Err(e) = chats.ensure(chat_id.clone()).await {
                send_event(
                    out_tx,
                    turn_error_body(config, &format!("openai-provider: {e}")),
                )
                .await?;
                return Ok(());
            }
            chats.push_user(&chat_id, text).await?;
            start_completion_turn(
                chats,
                auth,
                catalog,
                broker,
                config,
                client,
                out_tx,
                chat_id,
                true,
                Vec::new(),
            )
            .await?;
        }
        "interrupt" => {
            // Per-chat when `chat_id` is present and known; fall back to
            // global cancel-all when omitted (preserves the original
            // chat-side cancel-all UX from `ef260cd`). Unknown chat_id
            // is a logged no-op — neither fanout shape is correct.
            //
            // The agent reasoner emits `<provider>.interrupt { chat_id }`
            // per cancelled sub-graph firing (see `ebea3b8`); pre-fix
            // this handler treated every interrupt as global and the
            // sub-graph cancel nuked the lead's chat too.
            match read_chat_id(body) {
                Some(chat_id) => {
                    if chats.exists(&chat_id).await {
                        chats.interrupt(&chat_id).await;
                    } else {
                        tracing::warn!(
                            target: "openai_provider::interrupt",
                            chat_id = %chat_id,
                            "interrupt for unknown chat_id; ignoring",
                        );
                    }
                }
                None => {
                    chats.interrupt_all().await;
                }
            }
        }
        "reset" => {
            chats.reset_all().await;
            tracing::info!("reset_all: every chat history cleared");
        }
        "auth.set" => {
            let token = match body.get("token").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => t.to_owned(),
                _ => {
                    tracing::warn!("auth.set without non-empty token; ignoring");
                    return Ok(());
                }
            };
            let snap = auth.apply_auth_set(token).await;
            send_event(out_tx, auth_status_body(config, &snap)).await?;
        }
        "login_requested" => {
            let snap = auth
                .apply_login_requested(NO_LOGIN_FLOW_MESSAGE.to_owned())
                .await;
            send_event(out_tx, auth_status_body(config, &snap)).await?;
        }
        "models.list_requested" => {
            let token = auth.token().await;
            match list_models(
                client,
                &config.base_url,
                token.as_deref(),
                &config.auth_header,
            )
            .await
            {
                Ok(models) => {
                    send_event(out_tx, models_listed_body(config, &models)).await?;
                }
                Err(e) => {
                    let msg = match &e {
                        StreamError::Unauthorized { body } => {
                            format!("HTTP 401: {}", snippet(body))
                        }
                        StreamError::Http { status, body } => {
                            format!("HTTP {status}: {}", snippet(body))
                        }
                        StreamError::Request(s) => format!("request failed: {s}"),
                        StreamError::Body(s) => format!("read error: {s}"),
                        // list_models doesn't send `tools`; the variant
                        // can't fire here, but match exhaustively.
                        StreamError::ToolsUnsupported { body } => {
                            format!("HTTP 400: {}", snippet(body))
                        }
                    };
                    if matches!(e, StreamError::Unauthorized { .. }) {
                        let snap = auth.mark_auth_error(HTTP_401_MESSAGE.to_owned()).await;
                        send_event(out_tx, auth_status_body(config, &snap)).await?;
                    }
                    send_event(out_tx, turn_error_body(config, &msg)).await?;
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
            // Update the default-model seed so freshly-created chats
            // pick it up. Also retarget the legacy default chat (the
            // one nefor-chat is talking to via `<prefix>.prompt`) so
            // the next turn uses the new model — matching the v1
            // singleton's "set_active_model" behaviour.
            //
            // If the request carries `chat_id`, retarget that chat too
            // — the orchestrator passes its active conversation id so
            // mid-session model switches actually flip the live chat
            // (without this, /model only affects new chats and the
            // active one keeps its original model).
            chats.set_default_model(model.clone()).await;
            let default_id = ChatId::default_for_prefix(&prefix);
            if chats.exists(&default_id).await {
                let _ = chats.set_chat_model(&default_id, model.clone()).await;
            }
            if let Some(active_id) = body
                .get("chat_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(ChatId::new)
            {
                if active_id != default_id && chats.exists(&active_id).await {
                    let _ = chats.set_chat_model(&active_id, model.clone()).await;
                }
            }
            send_event(out_tx, model_set_ack_body(config, &model)).await?;
        }
        "logout_requested" => match auth.apply_logout().await {
            LogoutOutcome::Cleared => {
                let snap = auth.snapshot().await;
                send_event(out_tx, auth_status_body(config, &snap)).await?;
            }
            LogoutOutcome::RefusedEnv => {
                let snap = AuthSnapshot {
                    token: auth.token().await,
                    state: AuthState::Error(LOGOUT_REFUSED_ENV_MESSAGE.to_owned()),
                    source: None,
                };
                send_event(out_tx, auth_status_body(config, &snap)).await?;
            }
        },
        _ => {}
    }
    Ok(())
}

/// Begin a completion turn for `chat_id`. Routes both the explicit
/// `<prefix>.chat.complete` path and the legacy default-chat
/// `<prefix>.prompt` path.
///
/// `legacy_default_chat = true` selects the original wire shape used by
/// nefor-chat (`<prefix>.stream.delta` / `<prefix>.stream.end` /
/// `<prefix>.session.stats` / `<prefix>.turn.error`). The new
/// `chat.complete` path emits the same delta/end events plus a
/// `<prefix>.chat.complete.result` reply at the end so reasoner-graph
/// (T5) can correlate output to the originating chat.
#[allow(clippy::too_many_arguments)]
async fn start_completion_turn(
    chats: &Arc<Chats>,
    auth: &Arc<AuthStore>,
    catalog: &Arc<ToolCatalog>,
    broker: &Arc<ToolBroker>,
    config: &Config,
    client: &reqwest::Client,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    chat_id: ChatId,
    legacy_default_chat: bool,
    extra_tools: Vec<Value>,
) -> Result<(), LlmError> {
    let cancel = match chats.begin_turn(&chat_id).await {
        Ok(t) => t,
        Err(ChatsError::Busy(_)) => {
            send_event(out_tx, turn_error_body(config, "busy")).await?;
            return Ok(());
        }
        Err(e) => {
            send_event(out_tx, chat_error_body(config, &chat_id, &e)).await?;
            return Ok(());
        }
    };
    spawn_turn(
        chats.clone(),
        auth.clone(),
        catalog.clone(),
        broker.clone(),
        config.clone(),
        client.clone(),
        out_tx.clone(),
        chat_id,
        cancel,
        legacy_default_chat,
        extra_tools,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn spawn_turn(
    chats: Arc<Chats>,
    auth: Arc<AuthStore>,
    catalog: Arc<ToolCatalog>,
    broker: Arc<ToolBroker>,
    config: Config,
    client: reqwest::Client,
    out_tx: mpsc::Sender<PluginOutgoing>,
    chat_id: ChatId,
    cancel: tokio_util::sync::CancellationToken,
    legacy_default_chat: bool,
    extra_tools: Vec<Value>,
) {
    tokio::spawn(async move {
        let turn_id = uuid::Uuid::new_v4().to_string();
        let active_model = match chats.model(&chat_id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(chat_id = %chat_id, error = %e, "chat vanished before turn started");
                let _ = out_tx
                    .send(PluginOutgoing::event(chat_error_body(
                        &config, &chat_id, &e,
                    )))
                    .await;
                return;
            }
        };
        let started = std::time::Instant::now();
        let mut total_prompt_tokens: u64 = 0;
        let mut total_completion_tokens: u64 = 0;
        let mut iterations: u32 = 0;

        // Final outcome of the *last* HTTP call — what we emit
        // `chat.stream.end` from. Filled by the loop below.
        let mut final_text = String::new();
        #[allow(unused_assignments)]
        let mut final_finish_reason: Option<String> = None;
        let mut final_tool_calls: Vec<ToolCall> = Vec::new();
        // Reasoning trace from the last firing only — reasoning is
        // per-message, not accumulated across tool-loop iterations
        // (each firing produces its own thinking trace; we surface the
        // most recent one on `chat.complete.result`).
        let mut final_reasoning = String::new();
        let mut interrupted = false;
        let mut errored = false;

        loop {
            iterations += 1;
            let history = match chats.history_snapshot(&chat_id).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(chat_id = %chat_id, error = %e, "chat vanished mid-turn");
                    errored = true;
                    final_finish_reason = Some("error".to_string());
                    let _ = out_tx
                        .send(PluginOutgoing::event(chat_error_body(
                            &config, &chat_id, &e,
                        )))
                        .await;
                    break;
                }
            };
            tracing::info!(
                target: "openai_provider::turn",
                chat_id = %chat_id,
                iteration = iterations,
                history_len = history.len(),
                roles = ?history.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(),
                "history snapshot for turn iteration",
            );
            let chat_tools_on = chats.tools_enabled(&chat_id).await.unwrap_or(true);
            // Per-model cache: a model the upstream previously rejected
            // with the "does not support tools" signature stays disabled
            // for the rest of this process — even on a fresh chat. The
            // chat-level flag stays the discriminator for explicit opt-
            // outs (sub-graph responder, etc); the model-level cache is
            // a lazy capability cache populated reactively on the first
            // 400 we see for each model.
            let model_tools_supported = chats.model_supports_tools(&active_model).await;
            // Per-chat tool allowlist (set on chat.create via `tools` as
            // a string array). When `Some`, restrict catalog + extra_tools
            // to entries whose function.name is in the list. None = no
            // filter (the chat sees the full set). Used by the lead
            // orchestrator and the agent reasoner to scope each chat's
            // tool surface; the catalog itself remains process-wide.
            let chat_tool_allowlist = chats.tool_allowlist(&chat_id).await.unwrap_or(None);
            let mut tools_array = if chat_tools_on && model_tools_supported {
                catalog.to_openai_tools().await
            } else {
                Vec::new()
            };
            // Per-firing extra_tools (e.g. agent reasoner's `finalize`
            // synthetic terminator). Appended AFTER the catalog so a
            // catalog entry of the same name still wins on iteration
            // (the agent reasoner intercepts `finalize` Lua-side before
            // any catalog routing, so collisions are not a concern in
            // practice). Skipped when the chat is in tools-off mode
            // (matches catalog suppression — the model can't use tools
            // at all in that case).
            if chat_tools_on && model_tools_supported && !extra_tools.is_empty() {
                tools_array.extend(extra_tools.iter().cloned());
            }
            // Apply the per-chat allowlist to the assembled tools_array.
            // Filtering happens AFTER extra_tools are appended so a
            // caller that wanted `finalize` injected per firing must
            // also include `finalize` in the chat's allowlist. The
            // agent reasoner already does this (the Lua side appends
            // FINALIZE_NAME to the advertised list before sending
            // chat.create); the lead orchestrator's allowlist
            // intentionally excludes finalize because the lead
            // terminates by stopping tool calls, not by calling a
            // synthetic terminator.
            if let Some(names) = &chat_tool_allowlist {
                tools_array.retain(|t| {
                    let name = t
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str);
                    matches!(name, Some(n) if names.iter().any(|allowed| allowed == n))
                });
            }
            let tools_slice: Option<&[serde_json::Value]> = if tools_array.is_empty() {
                None
            } else {
                Some(tools_array.as_slice())
            };

            let endpoint = config.chat_endpoint();
            let id_for_delta = turn_id.clone();
            let chat_id_for_delta = chat_id.clone();
            let out_tx_for_delta = out_tx.clone();
            let prefix_for_delta = config.event_prefix();
            let id_for_reason = turn_id.clone();
            let chat_id_for_reason = chat_id.clone();
            let out_tx_for_reason = out_tx.clone();
            let prefix_for_reason = config.event_prefix();
            let token = auth.token().await;
            // Stamp the reasoning duration from first reasoning chunk
            // → ReasoningEvent::End. Captured in the closure so each
            // firing in a tool loop gets its own timer (per-firing
            // reasoning, never accumulated across firings).
            let mut reasoning_started_at: Option<std::time::Instant> = None;
            let result = run_chat_stream(
                &client,
                &endpoint,
                token.as_deref(),
                &config.auth_header,
                &active_model,
                &history,
                tools_slice,
                cancel.clone(),
                |delta| {
                    let body = stream_delta_body(
                        &prefix_for_delta,
                        &id_for_delta,
                        &chat_id_for_delta,
                        delta,
                    );
                    let _ = out_tx_for_delta.try_send(PluginOutgoing::event(body));
                },
                |ev| match ev {
                    ReasoningEvent::Delta(text) => {
                        if reasoning_started_at.is_none() {
                            reasoning_started_at = Some(std::time::Instant::now());
                        }
                        let body = stream_reasoning_delta_body(
                            &prefix_for_reason,
                            &id_for_reason,
                            &chat_id_for_reason,
                            text,
                        );
                        let _ = out_tx_for_reason.try_send(PluginOutgoing::event(body));
                    }
                    ReasoningEvent::End { text } => {
                        let duration_ms = reasoning_started_at
                            .map(|s| s.elapsed().as_millis() as u64)
                            .unwrap_or(0);
                        let body = stream_reasoning_end_body(
                            &prefix_for_reason,
                            &id_for_reason,
                            &chat_id_for_reason,
                            text,
                            duration_ms,
                        );
                        let _ = out_tx_for_reason.try_send(PluginOutgoing::event(body));
                    }
                },
            )
            .await;

            match result {
                Ok(outcome) => {
                    if let Some(u) = outcome.usage {
                        total_prompt_tokens = total_prompt_tokens.saturating_add(u.prompt_tokens);
                        total_completion_tokens =
                            total_completion_tokens.saturating_add(u.completion_tokens);
                    }

                    if outcome.interrupted {
                        // Treat partial deltas (if any) as the assistant's
                        // last word; same shape as the v1 path.
                        if !outcome.full_text.is_empty() {
                            let _ = chats
                                .push_assistant(&chat_id, outcome.full_text.clone())
                                .await;
                        }
                        final_text = outcome.full_text;
                        final_reasoning = outcome.reasoning_text;
                        final_finish_reason = Some("interrupted".to_string());
                        interrupted = true;
                        break;
                    }

                    if !outcome.tool_calls.is_empty() {
                        // Persist the assistant turn (text + tool calls)
                        // so the next request shows the same shape OpenAI
                        // expects: assistant message → tool messages.
                        let _ = chats
                            .push_assistant_tool_calls(
                                &chat_id,
                                outcome.full_text.clone(),
                                outcome.tool_calls.clone(),
                            )
                            .await;

                        // Stage 1+ (chat.complete API): defer the tool
                        // loop to the caller. reasoner-graph dispatches
                        // tools via tool-executor + tool-gate; the
                        // adapter reasoner translates ToolResults back
                        // into chat.append messages on the next firing.
                        // Running our own tool loop here would race
                        // those external dispatches and our internal
                        // tool-id broker would wait
                        // TOOL_RESULT_TIMEOUT (120s) for a tool.result
                        // that never matches because the gate-side ids
                        // are minted independently. Yield by returning
                        // the tool_calls in chat.complete.result.
                        if !legacy_default_chat {
                            final_text = outcome.full_text;
                            final_reasoning = outcome.reasoning_text;
                            final_finish_reason = outcome.finish_reason;
                            final_tool_calls = outcome.tool_calls;
                            break;
                        }

                        // Legacy `<prefix>.prompt` API: run each tool
                        // call (sequentially — the API requires every
                        // call's tool message to be present before the
                        // next chat-completions request, so there's no
                        // win in parallelism for a single round trip).
                        //
                        // Capture the call ids upfront so an interrupt
                        // partway through can synthesise tool_result
                        // messages for the cancelled tool AND any
                        // unstarted ones — the assistant turn pushed
                        // earlier carried every tool_call, and the
                        // OpenAI history shape requires a matching
                        // tool message per call. Without this, the
                        // next user submit would fail validation with
                        // "tool_call has no tool message".
                        let tool_call_ids: Vec<String> =
                            outcome.tool_calls.iter().map(|tc| tc.id.clone()).collect();
                        let mut tool_loop_failed = false;
                        let mut cancelled_idx: Option<usize> = None;
                        for (idx, tc) in outcome.tool_calls.into_iter().enumerate() {
                            let tool_step =
                                run_one_tool_call(&catalog, &broker, &out_tx, &cancel, tc).await;
                            match tool_step {
                                ToolStepOutcome::Result { id, content } => {
                                    let _ = chats.push_tool_result(&chat_id, id, content).await;
                                }
                                ToolStepOutcome::Cancelled { id } => {
                                    let _ = chats
                                        .push_tool_result(
                                            &chat_id,
                                            id,
                                            "(tool was interrupted by the user)".to_owned(),
                                        )
                                        .await;
                                    interrupted = true;
                                    tool_loop_failed = true;
                                    cancelled_idx = Some(idx);
                                    break;
                                }
                            }
                        }
                        if let Some(c_idx) = cancelled_idx {
                            for unstarted_id in tool_call_ids.iter().skip(c_idx + 1) {
                                let _ = chats
                                    .push_tool_result(
                                        &chat_id,
                                        unstarted_id.clone(),
                                        "(tool not run; previous tool call in this turn was interrupted)"
                                            .to_owned(),
                                    )
                                    .await;
                            }
                        }

                        if tool_loop_failed {
                            final_finish_reason = Some("interrupted".to_string());
                            break;
                        }

                        if iterations >= TOOL_LOOP_MAX_ITERATIONS {
                            tracing::warn!(
                                cap = TOOL_LOOP_MAX_ITERATIONS,
                                "tool-loop iteration cap hit; aborting turn"
                            );
                            errored = true;
                            final_finish_reason = Some("error".to_string());
                            // Emit the cap diagnostic via turn.error
                            // *after* the stream.end — same pattern as
                            // existing failure paths.
                            let _ = out_tx
                                .send(PluginOutgoing::event(turn_error_body(
                                    &config,
                                    &format!(
                                        "tool-loop iteration cap hit ({} iterations); aborting",
                                        TOOL_LOOP_MAX_ITERATIONS
                                    ),
                                )))
                                .await;
                            break;
                        }
                        // Loop back: another chat-completions call with the
                        // tool result(s) appended to history.
                        continue;
                    }

                    // No tool calls — the turn is done.
                    if !outcome.full_text.is_empty() {
                        let _ = chats
                            .push_assistant(&chat_id, outcome.full_text.clone())
                            .await;
                    }
                    final_text = outcome.full_text;
                    final_reasoning = outcome.reasoning_text;
                    final_finish_reason = outcome.finish_reason;
                    final_tool_calls = outcome.tool_calls;
                    break;
                }
                Err(StreamError::ToolsUnsupported { body }) => {
                    // Reactive fallback: the upstream rejected the request
                    // because the active model lacks the `tools` capability
                    // (e.g. ollama against `translategemma`). User's mental
                    // model is "I sent a message, the model should reply" —
                    // surfacing the raw 400 fails that. Mark the model as
                    // tools-incapable for the rest of this process and the
                    // chat as tools-off so subsequent iterations + future
                    // turns skip the round-trip, then `continue` to retry
                    // *this* iteration with no tools array. The retry can't
                    // re-enter this arm because the next iteration's
                    // `tools_array` is empty (chat flag flipped + model-
                    // cache populated).
                    tracing::info!(
                        target: "openai_provider::tools",
                        chat_id = %chat_id,
                        model = %active_model,
                        body = %body,
                        "model rejected tools — falling back to chat-only mode for this model",
                    );
                    chats.mark_model_tools_unsupported(&active_model).await;
                    let _ = chats.set_tools_enabled(&chat_id, false).await;
                    iterations = iterations.saturating_sub(1);
                    continue;
                }
                Err(e) => {
                    let msg = match &e {
                        StreamError::Unauthorized { body } => {
                            format!("HTTP 401: {}", extract_error_message(body))
                        }
                        StreamError::Http { status, body } => {
                            format!("HTTP {status}: {}", extract_error_message(body))
                        }
                        StreamError::Request(s) => format!("request failed: {s}"),
                        StreamError::Body(s) => format!("stream read error: {s}"),
                        // Handled above in its own arm — unreachable here,
                        // listed for exhaustiveness.
                        StreamError::ToolsUnsupported { body } => {
                            format!("HTTP 400: {}", extract_error_message(body))
                        }
                    };
                    tracing::warn!(error = %e, "turn failed");
                    if matches!(e, StreamError::Unauthorized { .. }) {
                        let snap = auth.mark_auth_error(HTTP_401_MESSAGE.to_owned()).await;
                        let _ = out_tx
                            .send(PluginOutgoing::event(auth_status_body(&config, &snap)))
                            .await;
                    }
                    errored = true;
                    final_finish_reason = Some("error".to_string());
                    let _ = out_tx
                        .send(PluginOutgoing::event(turn_error_body(&config, &msg)))
                        .await;
                    break;
                }
            }
        }

        let elapsed_ms = started.elapsed().as_millis() as u64;
        // Record the *aggregate* tokens for the turn. last_turn_*
        // captures the union of every chat-completions call (initial
        // + N follow-ups after tool calls); cumulative accumulates the
        // same aggregate.
        let _ = chats
            .record_turn(
                &chat_id,
                Some(&active_model),
                total_prompt_tokens,
                total_completion_tokens,
                elapsed_ms,
            )
            .await;

        let stream_end = stream_end_body(
            &config,
            &turn_id,
            &chat_id,
            &final_text,
            &active_model,
            elapsed_ms,
            final_finish_reason.as_deref(),
        );
        let _ = out_tx.send(PluginOutgoing::event(stream_end)).await;
        if let Ok(stats) = chats.stats_snapshot(&chat_id).await {
            let _ = out_tx
                .send(PluginOutgoing::event(session_stats_body(
                    &config, &chat_id, &stats,
                )))
                .await;
        }
        if interrupted && !errored {
            let _ = out_tx
                .send(PluginOutgoing::event(turn_error_body(
                    &config,
                    "interrupted",
                )))
                .await;
        }

        // Explicit `chat.complete` path: emit a closing
        // `<prefix>.chat.complete.result` carrying the
        // generic-provider.ProviderOut-shaped output. Legacy
        // `<prefix>.prompt` path skips it — the chat plugin reads
        // stream.end directly and doesn't speak the chat.* protocol
        // yet.
        if !legacy_default_chat {
            let body = chat_complete_result_body(
                &config,
                &chat_id,
                &final_text,
                &final_tool_calls,
                final_finish_reason.as_deref(),
                total_prompt_tokens,
                total_completion_tokens,
                &active_model,
                &final_reasoning,
            );
            let _ = out_tx.send(PluginOutgoing::event(body)).await;
        }

        chats.end_turn(&chat_id).await;
    });
}

/// Outcome of running a single tool call inside the agent loop. The
/// content carries either the tool's `output` (success) or its `error`
/// message (failure) — both are fed back to the model verbatim, since
/// from the model's POV "the tool said X" is the same shape regardless
/// of who labelled it an error.
enum ToolStepOutcome {
    Result { id: String, content: String },
    Cancelled { id: String },
}

/// Run a single tool call: emit `chat.tool.start`, route the
/// `<plugin>.tool.invoke`, await the matching `tool.result`, emit
/// `chat.tool.end`. Returns the content the model should see in the
/// follow-up request.
async fn run_one_tool_call(
    catalog: &Arc<ToolCatalog>,
    broker: &Arc<ToolBroker>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    cancel: &tokio_util::sync::CancellationToken,
    tc: ToolCall,
) -> ToolStepOutcome {
    let id = tc.id.clone();
    let name = tc.function.name.clone();
    let args_str = tc.function.arguments.clone();

    // Parse the args string into a JSON Value for the wire — fall back
    // to a raw-string carrier if parsing fails (the tool plugin is
    // welcome to reject it). The chat-contract calls for `args` to be
    // an object; we send whatever we can parse, defaulting to {}.
    let args_value: Value =
        serde_json::from_str(&args_str).unwrap_or_else(|_| Value::Object(Map::new()));

    // chat.tool.start — emit `input` (the chat-contract's optional
    // events section field name). The new tool-calling section in
    // chat-contract.md uses `args`; we follow the existing nefor-chat
    // consumer which reads `input`.
    let _ = out_tx
        .send(PluginOutgoing::event(chat_tool_start_body(
            &id,
            &name,
            &args_value,
        )))
        .await;

    // Resolve the owning plugin. If unknown — model hallucinated a
    // tool that wasn't in the catalog — synthesize an immediate error.
    let owner = match catalog.owner_of(&name).await {
        Some(o) => o,
        None => {
            let err = format!("no tool plugin registered tool `{}`", name);
            let _ = out_tx
                .send(PluginOutgoing::event(chat_tool_end_body(&id, &err, true)))
                .await;
            return ToolStepOutcome::Result {
                id: id.clone(),
                content: err,
            };
        }
    };

    // Register the pending invocation BEFORE emitting the invoke,
    // otherwise a fast-replying tool plugin can race us and
    // `broker.deliver` would drop on the floor.
    let rx = broker.register(id.clone()).await;
    let _ = out_tx
        .send(PluginOutgoing::event(tool_invoke_body(
            &owner, &id, &name, args_value,
        )))
        .await;

    // Await the matching tool.result OR cancellation OR timeout. The
    // broker's oneshot fires on result; `cancel.cancelled()` fires on
    // user interrupt; the sleep is the safety-net.
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            broker.cancel(&id).await;
            return ToolStepOutcome::Cancelled { id: id.clone() };
        }
        r = rx => r.ok(),
        _ = tokio::time::sleep(TOOL_RESULT_TIMEOUT) => {
            broker.cancel(&id).await;
            let err = format!(
                "tool `{}` did not reply within {}s",
                name,
                TOOL_RESULT_TIMEOUT.as_secs()
            );
            let _ = out_tx
                .send(PluginOutgoing::event(chat_tool_end_body(
                    &id, &err, true,
                )))
                .await;
            return ToolStepOutcome::Result {
                id,
                content: err,
            };
        }
    };

    let (content, is_error) = match result {
        Some(ToolResult {
            output: Some(out), ..
        }) => (out, false),
        Some(ToolResult {
            error: Some(err), ..
        }) => (err, true),
        Some(_) => ("tool replied without output or error".into(), true),
        // Receiver dropped without a result — broker contract violation
        // OR the broker was cancelled. Treat as error.
        None => ("tool reply channel closed".into(), true),
    };

    let _ = out_tx
        .send(PluginOutgoing::event(chat_tool_end_body(
            &id, &content, is_error,
        )))
        .await;
    ToolStepOutcome::Result { id, content }
}

fn snippet(s: &str) -> String {
    if s.len() <= 200 {
        s.to_owned()
    } else {
        format!("{}…", &s[..200])
    }
}

/// Extract a human-friendly message from an HTTP error body. The
/// OpenAI / Ollama / Groq / OpenRouter shape is
/// `{"error":{"message":"…", "type":"…", …}}`; surface just the
/// `message` field when present so the chat.message.append the user
/// eventually sees reads as a sentence rather than a wall of JSON.
/// Falls back to the truncated raw body.
fn extract_error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(msg) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return msg.to_owned();
        }
    }
    snippet(body)
}

/// Read the `chat_id` field from a body. Returns `None` when missing or
/// empty — the dispatcher then emits a generic error.
fn read_chat_id(body: &Map<String, Value>) -> Option<ChatId> {
    body.get("chat_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ChatId::new)
}

/// Parse a `generic-provider`-shaped message object into our internal
/// `Message`. The wire shape (per generic-provider's docstring) is
/// `{ role, content, tool_calls?, tool_name? }`. We accept that shape
/// and the openai-native variants too. Returns a string error message
/// when the shape is wrong (used by `chat.append` to reply with a
/// `chat.error`).
fn parse_provider_message(value: Option<&Value>) -> Result<Message, String> {
    let obj = value
        .and_then(Value::as_object)
        .ok_or_else(|| "chat.append `message` must be an object".to_owned())?;
    let role = obj
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| "chat.append message missing `role`".to_owned())?
        .to_owned();
    let content = obj.get("content").and_then(|v| match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    });
    let tool_calls = obj
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<ToolCall>(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let tool_call_id = obj
        .get("tool_call_id")
        .or_else(|| obj.get("tool_name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Message {
        role,
        content,
        tool_calls,
        tool_call_id,
    })
}

fn stream_delta_body(prefix: &str, id: &str, chat_id: &ChatId, text: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{prefix}stream.delta")),
    );
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("text".into(), Value::String(text.to_owned()));
    m
}

/// `<prefix>.stream.reasoning_delta { id, chat_id, text }` — one event
/// per chunk of `delta.reasoning` (Ollama's thinking trace for Qwen 3 /
/// Gemma 3). Mirrors `stream_delta_body`'s field shape so the chat-side
/// adapter can translate it the same way.
fn stream_reasoning_delta_body(
    prefix: &str,
    id: &str,
    chat_id: &ChatId,
    text: &str,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{prefix}stream.reasoning_delta")),
    );
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("text".into(), Value::String(text.to_owned()));
    m
}

/// `<prefix>.stream.reasoning_end { id, chat_id, text, duration_ms }`
/// — one event per turn at the moment reasoning stops streaming
/// (either content takes over, or `finish_reason` arrives without
/// content). Carries the FULL accumulated reasoning text so the chat
/// plugin can stamp the collapsed row without holding its own buffer.
fn stream_reasoning_end_body(
    prefix: &str,
    id: &str,
    chat_id: &ChatId,
    text: &str,
    duration_ms: u64,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{prefix}stream.reasoning_end")),
    );
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("text".into(), Value::String(text.to_owned()));
    m.insert("duration_ms".into(), Value::Number(duration_ms.into()));
    m
}

#[allow(clippy::too_many_arguments)]
fn stream_end_body(
    config: &Config,
    id: &str,
    chat_id: &ChatId,
    text: &str,
    model: &str,
    duration_ms: u64,
    finish_reason: Option<&str>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}stream.end", config.event_prefix())),
    );
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("text".into(), Value::String(text.to_owned()));
    m.insert("model".into(), Value::String(model.to_owned()));
    m.insert("duration_ms".into(), Value::Number(duration_ms.into()));
    if let Some(r) = finish_reason {
        m.insert("finish_reason".into(), Value::String(r.to_owned()));
    }
    m
}

fn session_stats_body(config: &Config, chat_id: &ChatId, stats: &ChatStats) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}session.stats", config.event_prefix())),
    );
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
    m.insert(
        "last_turn_context_tokens".into(),
        Value::Number(stats.last_turn_context_tokens.into()),
    );
    if let Some(d) = stats.last_turn_duration_ms {
        m.insert("last_turn_duration_ms".into(), Value::Number(d.into()));
    }
    m
}

fn auth_status_body(config: &Config, snap: &AuthSnapshot) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}auth.status", config.event_prefix())),
    );
    m.insert("state".into(), Value::String(snap.state.wire_str().into()));
    if let AuthState::Error(message) = &snap.state {
        m.insert("message".into(), Value::String(message.clone()));
    }
    m
}

fn models_listed_body(config: &Config, models: &[ModelInfo]) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}models.listed", config.event_prefix())),
    );
    m.insert(
        "models".into(),
        Value::Array(models.iter().map(|mi| Value::String(mi.id.clone())).collect()),
    );
    let ctx_map: Map<String, Value> = models
        .iter()
        .filter_map(|mi| {
            mi.context_window
                .map(|cw| (mi.id.clone(), Value::Number(cw.into())))
        })
        .collect();
    if !ctx_map.is_empty() {
        m.insert("context_windows".into(), Value::Object(ctx_map));
    }
    m
}

fn model_set_ack_body(config: &Config, model: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}model.set_ack", config.event_prefix())),
    );
    m.insert("model".into(), Value::String(model.to_owned()));
    m
}

fn turn_error_body(config: &Config, message: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}turn.error", config.event_prefix())),
    );
    m.insert("message".into(), Value::String(message.to_owned()));
    m
}

fn chat_created_body(config: &Config, chat_id: &ChatId) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}chat.created", config.event_prefix())),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m
}

fn chat_appended_body(config: &Config, chat_id: &ChatId) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}chat.appended", config.event_prefix())),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m
}

fn chat_deleted_body(config: &Config, chat_id: &ChatId) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}chat.deleted", config.event_prefix())),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m
}

fn chat_error_body(config: &Config, chat_id: &ChatId, e: &ChatsError) -> Map<String, Value> {
    chat_error_body_msg(config, chat_id, e.to_string())
}

fn chat_error_body_msg(config: &Config, chat_id: &ChatId, message: String) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}chat.error", config.event_prefix())),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("message".into(), Value::String(message));
    m
}

/// Emit a `<prefix>.chat.complete.result` body.
///
/// `output` follows the `generic-provider.ProviderOut` shape (per the
/// Schelling-point docstring on generic-provider/main.rs):
/// `{ text, tool_calls?, finish_reason?, usage?, reasoning? }`.
///
/// The optional `reasoning` field carries the model's full thinking
/// trace for the final firing. It rides on `chat.complete.result`
/// (control-plane only) so non-streaming consumers (sub-graph node
/// outputs, replay tooling, audit logs) can see it without subscribing
/// to per-chunk `stream.reasoning_delta` events. It is NEVER fed back
/// into the next request's history — `push_assistant` only stores the
/// content text.
#[allow(clippy::too_many_arguments)]
fn chat_complete_result_body(
    config: &Config,
    chat_id: &ChatId,
    text: &str,
    tool_calls: &[ToolCall],
    finish_reason: Option<&str>,
    prompt_tokens: u64,
    completion_tokens: u64,
    model: &str,
    reasoning: &str,
) -> Map<String, Value> {
    let mut output = Map::new();
    output.insert("text".into(), Value::String(text.to_owned()));
    if !reasoning.is_empty() {
        output.insert("reasoning".into(), Value::String(reasoning.to_owned()));
    }
    if !tool_calls.is_empty() {
        let arr: Vec<Value> = tool_calls
            .iter()
            .map(|tc| {
                let args = serde_json::from_str::<Value>(&tc.function.arguments)
                    .unwrap_or_else(|_| Value::String(tc.function.arguments.clone()));
                let mut entry = Map::new();
                entry.insert("id".into(), Value::String(tc.id.clone()));
                entry.insert("name".into(), Value::String(tc.function.name.clone()));
                entry.insert("arguments".into(), args);
                Value::Object(entry)
            })
            .collect();
        output.insert("tool_calls".into(), Value::Array(arr));
    }
    if let Some(r) = finish_reason {
        output.insert("finish_reason".into(), Value::String(r.to_owned()));
    }
    let mut usage = Map::new();
    usage.insert("prompt_tokens".into(), Value::Number(prompt_tokens.into()));
    usage.insert(
        "completion_tokens".into(),
        Value::Number(completion_tokens.into()),
    );
    usage.insert("model".into(), Value::String(model.to_owned()));
    output.insert("usage".into(), Value::Object(usage));

    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}chat.complete.result", config.event_prefix())),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    m.insert("output".into(), Value::Object(output));
    m
}

fn hello_body(config: &Config) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}hello", config.event_prefix())),
    );
    m.insert("version".into(), Value::String(PLUGIN_VERSION.into()));
    m.insert(
        "provider".into(),
        Value::String(config.provider_name.clone()),
    );
    if let Some(model) = config.model.as_ref() {
        m.insert("model".into(), Value::String(model.clone()));
    }
    m.insert("base_url".into(), Value::String(config.base_url.clone()));
    m
}

fn ready_body(config: &Config) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}ready", config.event_prefix())),
    );
    m
}

fn goodbye_body(config: &Config) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}goodbye", config.event_prefix())),
    );
    m.insert("reason".into(), Value::String("stream closed".into()));
    m
}

/// Build the `combinators.register` body announcing our two bare types
/// (`RawRequest`, `RawResponse`) plus four `Into` conversions against
/// `generic-provider`'s canonical `ProviderIn`/`ProviderOut`.
///
/// ## Spec gap (intentional)
///
/// The architecture (parent spec §2 + §5 decoupling table) requires
/// concrete provider plugins to declare:
///
/// ```text
/// Into<generic-provider.ProviderIn,  openai-provider.RawRequest>   -- canonical → upstream
/// Into<openai-provider.RawResponse,  generic-provider.ProviderOut> -- upstream → canonical
/// ```
///
/// The combinators-spec §4.1 currently states `Into.in` must be a bare
/// name in the sender's namespace — which would force the canonical-→-
/// upstream direction to be flipped, violating the architectural intent
/// (the LSP-shape only works if conversions FROM the canonical type
/// can be declared by the implementer).
///
/// We emit the architecturally-correct shape (`Into.in` cross-namespace
/// when targeting the canonical hub). The current `nefor-combinators`
/// registry will reject these entries with `MalformedEntry` until the
/// spec gap is resolved (see writeup in T3 deliverable). The
/// orchestrator harmonizes between the two specs/implementations.
///
/// We *also* include the safe (always-valid) direction
/// `Into<openai-provider.RawResponse, generic-provider.ProviderOut>`
/// (bare in → cross-namespace out) which both specs allow today, so a
/// half-working state is still useful.
fn register_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("combinators.register".into()));
    m.insert(
        "types".into(),
        Value::Array(vec![
            Value::String("RawRequest".into()),
            Value::String("RawResponse".into()),
        ]),
    );

    let impls = vec![
        // canonical → openai's wire (the architecturally-required direction).
        // NOTE: `in` is cross-namespace here. Today's registry will
        // reject this; intentional. See docstring above.
        register_into_entry(
            "generic-provider.ProviderIn",
            "RawRequest",
            "into.provider_in_to_raw_request",
        ),
        // openai's wire → canonical (allowed by current spec; bare in,
        // cross-namespace out).
        register_into_entry(
            "RawResponse",
            "generic-provider.ProviderOut",
            "into.raw_response_to_provider_out",
        ),
    ];
    m.insert("implementations".into(), Value::Array(impls));
    m
}

/// Build one `implementations[]` entry for an `Into<in, out>` declaration.
fn register_into_entry(in_type: &str, out_type: &str, handler: &str) -> Value {
    let mut entry = Map::new();
    entry.insert("trait".into(), Value::String("Into".into()));
    entry.insert("in".into(), Value::String(in_type.into()));
    entry.insert("out".into(), Value::String(out_type.into()));
    entry.insert("handler".into(), Value::String(handler.into()));
    Value::Object(entry)
}

/// Build a `chat.tool.start` body. Fields match the `chat.*` section of
/// `docs/chat-contract.md` (the optional events list — `id`, `name`,
/// `input`). The new tool-calling section uses `args`; we send `input`
/// to match the existing nefor-chat UI consumer (which reads `input`).
fn chat_tool_start_body(id: &str, name: &str, input: &Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("chat.tool.start".into()));
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("name".into(), Value::String(name.to_owned()));
    m.insert("input".into(), input.clone());
    m
}

/// Build a `chat.tool.end` body. `error: bool` matches what nefor-chat
/// expects (red-tints the row when true).
fn chat_tool_end_body(id: &str, output: &str, error: bool) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("chat.tool.end".into()));
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("output".into(), Value::String(output.to_owned()));
    m.insert("error".into(), Value::Bool(error));
    m
}

/// Build a `<plugin>.tool.invoke` body. The kind is prefix-routed by
/// the engine to deliver only to the named plugin (see
/// `starter/ncp.lua` `handle_event`).
fn tool_invoke_body(plugin: &str, id: &str, name: &str, args: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{plugin}.tool.invoke")),
    );
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("name".into(), Value::String(name.to_owned()));
    m.insert("args".into(), args);
    m
}

async fn send_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: Map<String, Value>,
) -> Result<(), LlmError> {
    out_tx
        .send(PluginOutgoing::event(body))
        .await
        .map_err(|_| LlmError::WriterClosed)
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), LlmError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| LlmError::WriterClosed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openai_provider::openai::{ToolCall, ToolCallFunction};

    fn cfg(name: &str) -> Config {
        Config {
            provider_name: name.into(),
            base_url: "http://localhost:11434".into(),
            model: Some("qwen2.5-coder:7b".into()),
            api_key: None,
            auth_header: "Authorization".into(),
        }
    }

    fn from_plugin(name: &str) -> PluginName {
        PluginName::new(name).expect("valid plugin name")
    }

    #[test]
    fn hello_body_carries_version_provider_model_base_url() {
        let c = cfg("ollama");
        let b = hello_body(&c);
        assert_eq!(b.get("kind").unwrap().as_str(), Some("ollama.hello"));
        assert_eq!(b.get("version").unwrap().as_str(), Some(PLUGIN_VERSION));
        assert_eq!(b.get("provider").unwrap().as_str(), Some("ollama"));
        assert_eq!(b.get("model").unwrap().as_str(), Some("qwen2.5-coder:7b"));
        assert_eq!(
            b.get("base_url").unwrap().as_str(),
            Some("http://localhost:11434")
        );
    }

    #[test]
    fn hello_body_uses_groq_prefix_when_configured() {
        let c = cfg("groq");
        let b = hello_body(&c);
        assert_eq!(b.get("kind").unwrap().as_str(), Some("groq.hello"));
        assert_eq!(b.get("provider").unwrap().as_str(), Some("groq"));
    }

    #[test]
    fn ready_body_kind_only_with_prefix() {
        let b = ready_body(&cfg("ollama"));
        assert_eq!(b.get("kind").unwrap().as_str(), Some("ollama.ready"));
        assert_eq!(b.len(), 1);

        let b2 = ready_body(&cfg("openrouter"));
        assert_eq!(b2.get("kind").unwrap().as_str(), Some("openrouter.ready"));
    }

    #[test]
    fn turn_error_body_has_message_and_prefix() {
        let b = turn_error_body(&cfg("ollama"), "busy");
        assert_eq!(b.get("kind").unwrap().as_str(), Some("ollama.turn.error"));
        assert_eq!(b.get("message").unwrap().as_str(), Some("busy"));

        let b2 = turn_error_body(&cfg("groq"), "boom");
        assert_eq!(b2.get("kind").unwrap().as_str(), Some("groq.turn.error"));
    }

    #[test]
    fn stream_delta_body_carries_text_id_chat_id_and_prefix() {
        let cid = ChatId::new("c1");
        let b = stream_delta_body("ollama.", "turn-1", &cid, "hi");
        assert_eq!(b.get("kind").unwrap().as_str(), Some("ollama.stream.delta"));
        assert_eq!(b.get("id").unwrap().as_str(), Some("turn-1"));
        assert_eq!(b.get("chat_id").unwrap().as_str(), Some("c1"));
        assert_eq!(b.get("text").unwrap().as_str(), Some("hi"));

        let b2 = stream_delta_body("groq.", "turn-1", &cid, "hi");
        assert_eq!(b2.get("kind").unwrap().as_str(), Some("groq.stream.delta"));
    }

    #[test]
    fn stream_end_body_includes_chat_id_model_duration_finish_with_prefix() {
        let cid = ChatId::new("c1");
        let b = stream_end_body(
            &cfg("ollama"),
            "turn-2",
            &cid,
            "Hello.",
            "qwen",
            1200,
            Some("stop"),
        );
        assert_eq!(b.get("kind").unwrap().as_str(), Some("ollama.stream.end"));
        assert_eq!(b.get("chat_id").unwrap().as_str(), Some("c1"));
        assert_eq!(b.get("text").unwrap().as_str(), Some("Hello."));
        assert_eq!(b.get("model").unwrap().as_str(), Some("qwen"));
        assert_eq!(b.get("duration_ms").unwrap().as_u64(), Some(1200));
        assert_eq!(b.get("finish_reason").unwrap().as_str(), Some("stop"));
    }

    #[test]
    fn stream_end_body_omits_finish_reason_when_absent() {
        let cid = ChatId::new("c");
        let b = stream_end_body(&cfg("ollama"), "turn-3", &cid, "", "qwen", 0, None);
        assert!(!b.contains_key("finish_reason"));
    }

    #[test]
    fn session_stats_body_shape_with_prefix_and_chat_id() {
        let stats = ChatStats {
            model: Some("qwen".into()),
            turns_completed: 2,
            cumulative_input_tokens: 100,
            cumulative_output_tokens: 50,
            last_turn_input_tokens: 60,
            last_turn_output_tokens: 25,
            last_turn_context_tokens: 60,
            last_turn_duration_ms: Some(1234),
        };
        let cid = ChatId::new("c1");
        let b = session_stats_body(&cfg("ollama"), &cid, &stats);
        assert_eq!(
            b.get("kind").unwrap().as_str(),
            Some("ollama.session.stats")
        );
        assert_eq!(b.get("chat_id").unwrap().as_str(), Some("c1"));
        assert_eq!(b.get("model").unwrap().as_str(), Some("qwen"));
        assert_eq!(b.get("turns").unwrap().as_u64(), Some(2));
        assert_eq!(
            b.get("cumulative_input_tokens").unwrap().as_u64(),
            Some(100)
        );
        assert_eq!(
            b.get("last_turn_context_tokens").unwrap().as_u64(),
            Some(60)
        );
        assert_eq!(b.get("last_turn_duration_ms").unwrap().as_u64(), Some(1234));
    }

    #[test]
    fn snippet_truncates_long_strings() {
        let long = "a".repeat(500);
        let s = snippet(&long);
        assert!(s.ends_with('…'));
        assert!(s.len() < long.len());
    }

    #[test]
    fn auth_status_body_connected_omits_message() {
        let snap = AuthSnapshot {
            token: Some("tok".into()),
            state: AuthState::Connected,
            source: None,
        };
        let b = auth_status_body(&cfg("ollama"), &snap);
        assert_eq!(b.get("kind").unwrap().as_str(), Some("ollama.auth.status"));
        assert_eq!(b.get("state").unwrap().as_str(), Some("connected"));
        assert!(!b.contains_key("message"));
    }

    #[test]
    fn auth_status_body_login_required_omits_message() {
        let snap = AuthSnapshot {
            token: None,
            state: AuthState::LoginRequired,
            source: None,
        };
        let b = auth_status_body(&cfg("ollama"), &snap);
        assert_eq!(b.get("state").unwrap().as_str(), Some("login_required"));
        assert!(!b.contains_key("message"));
    }

    #[test]
    fn auth_status_body_error_includes_message() {
        let snap = AuthSnapshot {
            token: None,
            state: AuthState::Error("nope".into()),
            source: None,
        };
        let b = auth_status_body(&cfg("groq"), &snap);
        assert_eq!(b.get("kind").unwrap().as_str(), Some("groq.auth.status"));
        assert_eq!(b.get("state").unwrap().as_str(), Some("error"));
        assert_eq!(b.get("message").unwrap().as_str(), Some("nope"));
    }

    /// Build the harness pieces a unit test needs: an AuthStore, a small
    /// stdout channel, and the matching receiver.
    fn auth_test_rig(
        env_key: Option<&str>,
    ) -> (
        Arc<AuthStore>,
        mpsc::Sender<PluginOutgoing>,
        mpsc::Receiver<PluginOutgoing>,
    ) {
        let auth = Arc::new(AuthStore::from_env_key(env_key.map(|s| s.to_string())));
        let (tx, rx) = mpsc::channel::<PluginOutgoing>(16);
        (auth, tx, rx)
    }

    fn make_event_body(kind: &str, extra: &[(&str, Value)]) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("kind".into(), Value::String(kind.into()));
        for (k, v) in extra {
            m.insert((*k).to_owned(), v.clone());
        }
        m
    }

    /// Drain the writer channel into a vec of bodies (events only).
    async fn drain(rx: &mut mpsc::Receiver<PluginOutgoing>) -> Vec<Map<String, Value>> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            // PluginOutgoing serializes through to_line; for tests we can
            // round-trip through JSON to recover the body map.
            let line = msg.to_line();
            let v: Value = serde_json::from_str(&line).expect("plugin outgoing json");
            if v.get("type").and_then(Value::as_str) == Some("event") {
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    out.push(body.clone());
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn auth_state_starts_connected_when_env_key_present() {
        let auth = AuthStore::from_env_key(Some("envkey".into()));
        let snap = auth.snapshot().await;
        assert_eq!(snap.state, AuthState::Connected);
        let body = auth_status_body(&cfg("ollama"), &snap);
        assert_eq!(body.get("state").unwrap().as_str(), Some("connected"));
    }

    #[tokio::test]
    async fn auth_state_starts_login_required_when_no_env_key() {
        let auth = AuthStore::from_env_key(None);
        let snap = auth.snapshot().await;
        assert_eq!(snap.state, AuthState::LoginRequired);
        let body = auth_status_body(&cfg("ollama"), &snap);
        assert_eq!(body.get("state").unwrap().as_str(), Some("login_required"));
        assert!(!body.contains_key("message"));
    }

    fn fresh_chats(default_model: &str) -> Arc<Chats> {
        Arc::new(Chats::with_default_model(Some(default_model.to_owned())))
    }

    #[tokio::test]
    async fn auth_set_event_updates_token_and_emits_connected_status() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body(
            "ollama.auth.set",
            &[("token", Value::String("new-tok".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").unwrap().as_str(),
            Some("ollama.auth.status")
        );
        assert_eq!(emitted[0].get("state").unwrap().as_str(), Some("connected"));
        assert_eq!(auth.token().await.as_deref(), Some("new-tok"));
    }

    #[tokio::test]
    async fn login_requested_emits_error_status_for_openai_provider() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body("ollama.login_requested", &[]);
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].get("state").unwrap().as_str(), Some("error"));
        let msg = emitted[0].get("message").unwrap().as_str().unwrap();
        assert!(msg.contains("no built-in login flow"), "message was: {msg}");
    }

    #[tokio::test]
    async fn logout_requested_with_env_token_emits_error_status_no_clear() {
        let (auth, tx, mut rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body("ollama.logout_requested", &[]);
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].get("state").unwrap().as_str(), Some("error"));
        let msg = emitted[0].get("message").unwrap().as_str().unwrap();
        assert!(msg.contains("--api-key"), "message was: {msg}");

        // Token must remain.
        assert_eq!(auth.token().await.as_deref(), Some("envkey"));
        let snap = auth.snapshot().await;
        // Stored state still Connected — the error went out on the wire
        // but didn't mutate the in-memory state.
        assert_eq!(snap.state, AuthState::Connected);
    }

    #[tokio::test]
    async fn logout_requested_after_auth_set_clears_token_emits_login_required() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        // First, auth.set so the token source is AuthSet.
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &make_event_body(
                "ollama.auth.set",
                &[("token", Value::String("acquired".into()))],
            ),
        )
        .await
        .expect("dispatch ok");
        // Drain the connected status.
        let _ = drain(&mut rx).await;

        // Now logout.
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &make_event_body("ollama.logout_requested", &[]),
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("state").unwrap().as_str(),
            Some("login_required")
        );
        assert!(auth.token().await.is_none());
    }

    #[tokio::test]
    async fn http_401_response_transitions_to_error_state() {
        // We can't drive a real HTTP request from a unit test cleanly,
        // but mark_auth_error is the same path the dispatcher takes on
        // StreamError::Unauthorized.
        let (auth, _tx, _rx) = auth_test_rig(Some("badkey"));
        let snap = auth.mark_auth_error(HTTP_401_MESSAGE.to_owned()).await;
        assert_eq!(snap.state.wire_str(), "error");
        let body = auth_status_body(&cfg("ollama"), &snap);
        assert_eq!(body.get("state").unwrap().as_str(), Some("error"));
        let msg = body.get("message").unwrap().as_str().unwrap();
        assert!(msg.contains("401"), "message was: {msg}");
    }

    #[tokio::test]
    async fn models_list_requested_emits_models_listed_event() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 4096];
            let mut acc = String::new();
            while !acc.contains("\r\n\r\n") {
                let n = s.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            let body = r#"{"object":"list","data":[{"id":"qwen2.5-coder:7b"},{"id":"llama3:8b"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(response.as_bytes()).await;
            let _ = s.shutdown().await;
        });

        let (auth, tx, mut rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("any");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let mut config = cfg("ollama");
        config.base_url = format!("http://{}", addr);
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body("ollama.models.list_requested", &[]);
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let _ = server.await;

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").unwrap().as_str(),
            Some("ollama.models.listed")
        );
        let arr = emitted[0].get("models").unwrap().as_array().expect("array");
        // Sorted alphabetically.
        assert_eq!(arr[0].as_str(), Some("llama3:8b"));
        assert_eq!(arr[1].as_str(), Some("qwen2.5-coder:7b"));
    }

    #[tokio::test]
    async fn model_set_updates_default_and_emits_ack() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("initial-model");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body(
            "ollama.model.set",
            &[("model", Value::String("new-model".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").unwrap().as_str(),
            Some("ollama.model.set_ack")
        );
        assert_eq!(emitted[0].get("model").unwrap().as_str(), Some("new-model"));
        assert_eq!(chats.default_model().await.as_deref(), Some("new-model"));
    }

    #[tokio::test]
    async fn model_set_with_empty_model_is_ignored() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("seed");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body("ollama.model.set", &[("model", Value::String("".into()))]);
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert!(emitted.is_empty());
        assert_eq!(chats.default_model().await.as_deref(), Some("seed"));
    }

    #[test]
    fn models_listed_body_carries_models_with_prefix() {
        let models = vec![
            ModelInfo { id: "a".into(), context_window: None },
            ModelInfo { id: "b".into(), context_window: Some(128000) },
        ];
        let b = models_listed_body(&cfg("ollama"), &models);
        assert_eq!(
            b.get("kind").unwrap().as_str(),
            Some("ollama.models.listed")
        );
        let arr = b.get("models").unwrap().as_array().expect("array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0].as_str(), Some("a"));
        assert_eq!(arr[1].as_str(), Some("b"));
        let cw = b.get("context_windows").unwrap().as_object().expect("map");
        assert_eq!(cw.len(), 1);
        assert_eq!(cw.get("b").unwrap().as_u64(), Some(128000));
    }

    #[test]
    fn model_set_ack_body_carries_model_with_prefix() {
        let b = model_set_ack_body(&cfg("groq"), "llama-3.3");
        assert_eq!(b.get("kind").unwrap().as_str(), Some("groq.model.set_ack"));
        assert_eq!(b.get("model").unwrap().as_str(), Some("llama-3.3"));
    }

    #[tokio::test]
    async fn auth_set_with_empty_token_is_ignored() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body("ollama.auth.set", &[("token", Value::String("".into()))]);
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert!(emitted.is_empty(), "no status should be emitted");
        assert!(auth.token().await.is_none());
    }

    // --- Tool-calling event shapes -----------------------------------

    #[test]
    fn chat_tool_start_body_uses_input_field_per_chat_contract() {
        let args = serde_json::json!({"path": "/tmp/x"});
        let b = chat_tool_start_body("call_1", "read_file", &args);
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("chat.tool.start")
        );
        assert_eq!(b.get("id").and_then(Value::as_str), Some("call_1"));
        assert_eq!(b.get("name").and_then(Value::as_str), Some("read_file"));
        assert_eq!(b.get("input"), Some(&args));
    }

    #[test]
    fn chat_tool_end_body_carries_output_and_error_bool() {
        let b = chat_tool_end_body("call_1", "result text", false);
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("chat.tool.end"));
        assert_eq!(b.get("id").and_then(Value::as_str), Some("call_1"));
        assert_eq!(b.get("output").and_then(Value::as_str), Some("result text"));
        assert_eq!(b.get("error").and_then(Value::as_bool), Some(false));

        let err_body = chat_tool_end_body("call_2", "boom", true);
        assert_eq!(err_body.get("error").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn tool_invoke_body_uses_plugin_prefix_routing() {
        let args = serde_json::json!({"path": "/tmp/x"});
        let b = tool_invoke_body("basic-tools", "call_1", "read_file", args.clone());
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("basic-tools.tool.invoke")
        );
        assert_eq!(b.get("id").and_then(Value::as_str), Some("call_1"));
        assert_eq!(b.get("name").and_then(Value::as_str), Some("read_file"));
        assert_eq!(b.get("args"), Some(&args));
    }

    // --- Catalog wiring through dispatch ----------------------------

    #[tokio::test]
    async fn dispatch_tool_register_populates_catalog_for_sender() {
        let (auth, tx, _rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body(
            "tool.register",
            &[(
                "tools",
                serde_json::json!([{
                    "name": "read_file",
                    "description": "Read a file.",
                    "parameters": {"type": "object"}
                }]),
            )],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("basic-tools"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let tools = catalog.to_openai_tools().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(
            catalog.owner_of("read_file").await.as_deref(),
            Some("basic-tools")
        );
    }

    #[tokio::test]
    async fn dispatch_tool_result_delivers_to_pending_invocation() {
        let (auth, tx, _rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        // Register a pending invocation, then deliver a tool.result via
        // dispatch_event.
        let rx_pending = broker.register("call_xyz".into()).await;
        let body = make_event_body(
            "tool.result",
            &[
                ("id", Value::String("call_xyz".into())),
                ("output", Value::String("file contents".into())),
            ],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("basic-tools"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let result = rx_pending.await.expect("oneshot resolved");
        assert_eq!(result.id, "call_xyz");
        assert_eq!(result.output.as_deref(), Some("file contents"));
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn dispatch_tool_result_with_error_string_routes_through() {
        let (auth, tx, _rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let rx_pending = broker.register("call_err".into()).await;
        let body = make_event_body(
            "tool.result",
            &[
                ("id", Value::String("call_err".into())),
                ("error", Value::String("file not found".into())),
            ],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("basic-tools"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let result = rx_pending.await.expect("resolved");
        assert!(result.output.is_none());
        assert_eq!(result.error.as_deref(), Some("file not found"));
    }

    #[tokio::test]
    async fn dispatch_tool_result_for_unknown_id_is_silently_dropped() {
        let (auth, tx, _rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body(
            "tool.result",
            &[
                ("id", Value::String("never-registered".into())),
                ("output", Value::String("x".into())),
            ],
        );
        // Just must not error. There's no caller to address.
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("basic-tools"),
            &body,
        )
        .await
        .expect("dispatch ok");
    }

    /// End-to-end: queue a tool call, simulate a fast tool reply via
    /// the broker, and verify run_one_tool_call emits the right
    /// chat.tool.start / <plugin>.tool.invoke / chat.tool.end sequence.
    #[tokio::test]
    async fn run_one_tool_call_routes_invoke_then_emits_end_on_success() {
        let catalog = Arc::new(ToolCatalog::new());
        catalog
            .register_from(
                "basic-tools",
                vec![openai_provider::catalog::ToolSpec {
                    name: "read_file".into(),
                    description: "Read a file.".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }],
            )
            .await;
        let broker = Arc::new(ToolBroker::new());
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(16);
        let cancel = tokio_util::sync::CancellationToken::new();

        let tc = ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{\"path\":\"/tmp/x\"}".into(),
            },
        };

        // Spawn the tool runner; meanwhile deliver a result via the
        // broker as if the tool plugin had replied.
        let broker_clone = broker.clone();
        let runner = tokio::spawn(async move {
            run_one_tool_call(&catalog, &broker_clone, &tx, &cancel, tc).await
        });

        // Give the runner a tick to register pending + emit invoke.
        tokio::time::sleep(Duration::from_millis(20)).await;
        broker
            .deliver(ToolResult {
                id: "call_1".into(),
                output: Some("file body".into()),
                error: None,
            })
            .await;

        let outcome = runner.await.expect("runner");
        match outcome {
            ToolStepOutcome::Result { id, content } => {
                assert_eq!(id, "call_1");
                assert_eq!(content, "file body");
            }
            ToolStepOutcome::Cancelled { .. } => panic!("unexpected cancel"),
        }

        // Inspect the event sequence.
        let mut events = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            let line = msg.to_line();
            let v: Value = serde_json::from_str(&line).expect("json");
            if v.get("type").and_then(Value::as_str) == Some("event") {
                events.push(v.get("body").unwrap().clone());
            }
        }
        assert_eq!(events.len(), 3, "start + invoke + end");
        assert_eq!(
            events[0].get("kind").and_then(Value::as_str),
            Some("chat.tool.start")
        );
        assert_eq!(
            events[1].get("kind").and_then(Value::as_str),
            Some("basic-tools.tool.invoke")
        );
        assert_eq!(
            events[2].get("kind").and_then(Value::as_str),
            Some("chat.tool.end")
        );
        assert_eq!(
            events[2].get("output").and_then(Value::as_str),
            Some("file body")
        );
        assert_eq!(events[2].get("error").and_then(Value::as_bool), Some(false));
    }

    #[tokio::test]
    async fn run_one_tool_call_emits_error_end_on_unknown_tool() {
        let catalog = Arc::new(ToolCatalog::new()); // empty
        let broker = Arc::new(ToolBroker::new());
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(16);
        let cancel = tokio_util::sync::CancellationToken::new();

        let tc = ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "nonexistent".into(),
                arguments: "{}".into(),
            },
        };
        let outcome = run_one_tool_call(&catalog, &broker, &tx, &cancel, tc).await;
        match outcome {
            ToolStepOutcome::Result { id, content } => {
                assert_eq!(id, "call_1");
                assert!(content.contains("nonexistent"));
            }
            ToolStepOutcome::Cancelled { .. } => panic!("unexpected cancel"),
        }
        // chat.tool.start, then chat.tool.end with error=true. No
        // tool.invoke (no owner).
        let mut events = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            let line = msg.to_line();
            let v: Value = serde_json::from_str(&line).expect("json");
            if v.get("type").and_then(Value::as_str) == Some("event") {
                events.push(v.get("body").unwrap().clone());
            }
        }
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].get("kind").and_then(Value::as_str),
            Some("chat.tool.start")
        );
        assert_eq!(
            events[1].get("kind").and_then(Value::as_str),
            Some("chat.tool.end")
        );
        assert_eq!(events[1].get("error").and_then(Value::as_bool), Some(true));
    }

    // --- New explicit chat.* API -------------------------------------

    #[tokio::test]
    async fn chat_create_emits_chat_created_then_state_holds() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body(
            "ollama.chat.create",
            &[("chat_id", Value::String("c-1".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").and_then(Value::as_str),
            Some("ollama.chat.created")
        );
        assert_eq!(
            emitted[0].get("chat_id").and_then(Value::as_str),
            Some("c-1")
        );
        assert!(chats.exists(&ChatId::new("c-1")).await);
    }

    #[tokio::test]
    async fn chat_create_duplicate_emits_chat_error() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        chats
            .create(ChatId::new("c-1"), None, None, None)
            .await
            .expect("seed");

        let body = make_event_body(
            "ollama.chat.create",
            &[("chat_id", Value::String("c-1".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").and_then(Value::as_str),
            Some("ollama.chat.error")
        );
        assert_eq!(
            emitted[0].get("chat_id").and_then(Value::as_str),
            Some("c-1")
        );
    }

    #[tokio::test]
    async fn chat_append_appends_message_to_chat_history() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        chats
            .create(ChatId::new("c-1"), None, None, None)
            .await
            .expect("seed");

        let msg = serde_json::json!({"role": "user", "content": "hello"});
        let body = make_event_body(
            "ollama.chat.append",
            &[("chat_id", Value::String("c-1".into())), ("message", msg)],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").and_then(Value::as_str),
            Some("ollama.chat.appended")
        );

        let history = chats
            .history_snapshot(&ChatId::new("c-1"))
            .await
            .expect("snap");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn chat_append_to_unknown_chat_emits_chat_error() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let msg = serde_json::json!({"role": "user", "content": "hello"});
        let body = make_event_body(
            "ollama.chat.append",
            &[("chat_id", Value::String("ghost".into())), ("message", msg)],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").and_then(Value::as_str),
            Some("ollama.chat.error")
        );
    }

    /// Regression for the cancel-mid-stream contract: when a `/cancel`
    /// fires while the model is mid-response, the partial assistant
    /// text the model has already emitted MUST land in the chat's
    /// history table on the provider binary side. The user-facing
    /// motivation: "you started thinking wrong, reconsider" only works
    /// if the next turn's request includes what the model just said
    /// before being cut off.
    ///
    /// Drive: spin up an SSE server that streams 5 deltas with a 30ms
    /// pause between each (well past the watchdog floor of any path
    /// here, well below the test's 5s timeout); fire chat.create →
    /// chat.append (user) → chat.complete; wait for at least 2 deltas
    /// to reach the writer channel; call chats.interrupt(&chat_id)
    /// directly (the bus-side path is `<prefix>.interrupt` →
    /// `chats.interrupt_all()`, which is functionally identical for
    /// the per-chat case here); wait for chat.complete.result; assert
    /// chats.history_snapshot's last message is an assistant message
    /// with non-empty content equal to a prefix of the deltas the
    /// server actually wrote.
    #[tokio::test]
    async fn chat_complete_persists_partial_assistant_to_history_on_interrupt_midstream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        // Slow-stream SSE: send each delta line, flush, wait 30ms,
        // repeat. Five deltas → ~150ms total before [DONE]; the test
        // interrupts after the second delta lands in the writer
        // channel, so the server may still be mid-write when the
        // cancel fires. That's the production shape — the cancel
        // token lives inside `run_chat_stream`'s `tokio::select!` and
        // races the `byte_stream.next()` arm.
        let _server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");
            // Drain request headers + body before responding.
            let mut buf = vec![0u8; 4096];
            let mut acc = String::new();
            while !acc.contains("\r\n\r\n") {
                let n = s.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            // Headers + chunked transfer (Content-Length unknown for a
            // paced stream).
            let _ = s
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\n\
                      Connection: close\r\n\r\n",
                )
                .await;
            for word in ["alpha", "beta", "gamma", "delta", "epsilon"] {
                let frame = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{word} \"}}}}]}}\n\n",
                );
                let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
                if s.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            let finish = "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n";
            let usage = "data: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":5}}\n\n";
            let done = "data: [DONE]\n\n";
            for frame in [finish, usage, done] {
                let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
                if s.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
            }
            let _ = s.write_all(b"0\r\n\r\n").await;
            let _ = s.shutdown().await;
        });

        let (auth, tx, mut rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("test-model");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let mut config = cfg("ollama");
        config.base_url = format!("http://{}", addr);
        let client = reqwest::Client::builder().build().expect("client");

        let chat_id = ChatId::new("c-interrupt");

        // 1. chat.create.
        let create_body = make_event_body(
            "ollama.chat.create",
            &[("chat_id", Value::String("c-interrupt".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &create_body,
        )
        .await
        .expect("create");

        // 2. chat.append { role=user }.
        let append_body = make_event_body(
            "ollama.chat.append",
            &[
                ("chat_id", Value::String("c-interrupt".into())),
                (
                    "message",
                    serde_json::json!({"role": "user", "content": "hi"}),
                ),
            ],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &append_body,
        )
        .await
        .expect("append");

        // 3. chat.complete — kicks off the spawned turn.
        let complete_body = make_event_body(
            "ollama.chat.complete",
            &[("chat_id", Value::String("c-interrupt".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &complete_body,
        )
        .await
        .expect("complete");

        // 4. Wait for at least 2 stream.delta envelopes to reach the
        //    writer channel — confirms we're mid-stream when the
        //    cancel fires.
        let mut delta_count = 0;
        let mut wire_partial = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while delta_count < 2 && std::time::Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                let line = msg.to_line();
                let v: Value = serde_json::from_str(&line).expect("plugin out json");
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    if body.get("kind").and_then(Value::as_str) == Some("ollama.stream.delta") {
                        delta_count += 1;
                        if let Some(t) = body.get("text").and_then(Value::as_str) {
                            wire_partial.push_str(t);
                        }
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        assert!(
            delta_count >= 2,
            "expected at least 2 stream.delta envelopes before timeout; got {delta_count}",
        );
        assert!(
            !wire_partial.is_empty(),
            "wire_partial must accumulate the delta text",
        );

        // 5. Interrupt the in-flight turn directly. The bus-side path
        //    is `<prefix>.interrupt` → chats.interrupt_all(); the
        //    per-chat shape calls chats.interrupt(&chat_id).
        let was_in_flight = chats.interrupt(&chat_id).await;
        assert!(was_in_flight, "interrupt should land on an in-flight turn");

        // 6. Wait for chat.complete.result on the writer channel —
        //    that's the marker the spawned turn finished its cleanup.
        let mut saw_complete_result = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                let line = msg.to_line();
                let v: Value = serde_json::from_str(&line).expect("json");
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    if body.get("kind").and_then(Value::as_str)
                        == Some("ollama.chat.complete.result")
                    {
                        saw_complete_result = true;
                        break;
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        assert!(
            saw_complete_result,
            "chat.complete.result must arrive after interrupt"
        );

        // 7. The persistence assertion. History should be:
        //    [user="hi", assistant=<partial>]. The partial assistant
        //    content must be non-empty (regression: pre-fix it would
        //    not be pushed when the path was wrong).
        let history = chats
            .history_snapshot(&chat_id)
            .await
            .expect("history snapshot");
        assert_eq!(
            history.len(),
            2,
            "expected [user, assistant] in history, got {history:?}",
        );
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content.as_deref(), Some("hi"));
        assert_eq!(history[1].role, "assistant");
        let assistant_content = history[1]
            .content
            .as_deref()
            .expect("assistant content set");
        assert!(
            !assistant_content.is_empty(),
            "assistant content must hold the partial text streamed before interrupt; \
             history was {history:?}; wire_partial = {wire_partial:?}",
        );
        // The persisted partial is whatever the stream loop accumulated
        // into `outcome.full_text` at the moment the cancel token fired
        // — usually equal to or a 1-2-chunk superset of what the writer
        // channel had at the moment we called `chats.interrupt`. Either
        // direction (wire is a prefix of stored, or stored is a prefix
        // of wire) is acceptable; both indicate the same underlying
        // delta accumulator.
        assert!(
            assistant_content.starts_with(wire_partial.trim_end())
                || wire_partial.starts_with(assistant_content),
            "stored partial and wire-observed partial must share a prefix; \
             stored={assistant_content:?} wire={wire_partial:?}",
        );
    }

    /// Fix #1 regression: when `<prefix>.chat.complete` carries an
    /// `extra_tools` array, every iteration's upstream HTTP request body
    /// MUST include those tools alongside whatever the global
    /// ToolCatalog provided. The agent reasoner relies on this to inject
    /// its synthetic `finalize` schema per-firing without polluting the
    /// catalog. Pre-fix the chat.complete dispatcher ignored
    /// `extra_tools` and the tools array was catalog-only — the model
    /// never saw `finalize` in its advertised set.
    ///
    /// Drive: bind an SSE server that captures the first request body
    /// it receives and replies with a single delta + finish + DONE.
    /// Fire chat.create → chat.append (user) → chat.complete (with
    /// extra_tools = [finalize_schema]). Wait for
    /// chat.complete.result. Assert the captured request body's
    /// `tools` array contains an entry with name == "finalize".
    #[tokio::test]
    async fn chat_complete_extra_tools_lands_in_upstream_request_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let captured: std::sync::Arc<tokio::sync::Mutex<String>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let captured_clone = captured.clone();

        let _server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 8192];
            let mut acc = String::new();
            loop {
                let n = s.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                if let Some(headers_end) = acc.find("\r\n\r\n") {
                    let cl: usize = acc
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length:")
                                .or_else(|| l.strip_prefix("Content-Length:"))
                        })
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    let body_so_far = acc.len() - (headers_end + 4);
                    if body_so_far >= cl {
                        break;
                    }
                }
            }
            if let Some(idx) = acc.find("\r\n\r\n") {
                *captured_clone.lock().await = acc[idx + 4..].to_owned();
            }
            // One delta + finish + DONE — terminates the loop cleanly so
            // the test doesn't hang waiting for chat.complete.result.
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
                        data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                        data: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(response.as_bytes()).await;
            let _ = s.shutdown().await;
        });

        let (auth, tx, mut rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("test-model");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let mut config = cfg("ollama");
        config.base_url = format!("http://{}", addr);
        let client = reqwest::Client::builder().build().expect("client");

        // 1. chat.create.
        let create_body = make_event_body(
            "ollama.chat.create",
            &[("chat_id", Value::String("c-extra".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &create_body,
        )
        .await
        .expect("create");

        // 2. chat.append { role=user }.
        let append_body = make_event_body(
            "ollama.chat.append",
            &[
                ("chat_id", Value::String("c-extra".into())),
                (
                    "message",
                    serde_json::json!({"role": "user", "content": "go"}),
                ),
            ],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &append_body,
        )
        .await
        .expect("append");

        // 3. chat.complete with extra_tools carrying the finalize
        // schema. Mirrors the agent reasoner's emit shape:
        // `extra_tools = [FINALIZE_SCHEMA]`.
        let finalize_schema = serde_json::json!({
            "type": "function",
            "function": {
                "name": "finalize",
                "description": "Terminate this agent's run.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "answer": {"type": "string"}
                    },
                    "required": ["answer"],
                    "additionalProperties": true,
                },
            },
        });
        let complete_body = make_event_body(
            "ollama.chat.complete",
            &[
                ("chat_id", Value::String("c-extra".into())),
                ("extra_tools", Value::Array(vec![finalize_schema])),
            ],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &complete_body,
        )
        .await
        .expect("complete");

        // 4. Wait for chat.complete.result on the writer channel —
        //    that's the marker the spawned turn finished its HTTP call.
        let mut saw_complete_result = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                let line = msg.to_line();
                let v: Value = serde_json::from_str(&line).expect("json");
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    if body.get("kind").and_then(Value::as_str)
                        == Some("ollama.chat.complete.result")
                    {
                        saw_complete_result = true;
                        break;
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        assert!(
            saw_complete_result,
            "chat.complete.result must arrive after the SSE server closes",
        );

        // 5. Assert the captured upstream request body included the
        //    finalize tool in its `tools` array.
        let body = captured.lock().await.clone();
        assert!(!body.is_empty(), "server must have captured a request body",);
        let v: serde_json::Value = serde_json::from_str(&body).expect("upstream request body json");
        let tools = v
            .get("tools")
            .and_then(|t| t.as_array())
            .expect("upstream request body must include `tools` when extra_tools provided");
        let saw_finalize = tools.iter().any(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some("finalize")
        });
        assert!(
            saw_finalize,
            "tools array MUST include an entry whose function.name == \"finalize\"; \
             body was {body}",
        );
    }

    /// Per-chat tool-name allowlist regression: when `chat.create.tools`
    /// is an array of strings, the per-turn upstream request body's
    /// `tools` array MUST be filtered to entries whose function.name is
    /// in the list. Catalog entries outside the list are dropped.
    ///
    /// Drives the lead-orchestrator runtime fix — pre-fix the lead's
    /// chat advertised the full catalog (including reasoner-graph
    /// internals like `spawn_graph`), so the model would happily call
    /// `spawn_graph` directly and bottom out in `reasoner '<role>' not
    /// connected`. The allowlist is the substrate that prevents the
    /// model from ever seeing names it shouldn't call.
    ///
    /// Drive: register two tools in the catalog (`read_file`,
    /// `spawn_graph`); chat.create with `tools = ["read_file"]`;
    /// chat.complete. Capture the upstream request body and assert
    /// `read_file` is present and `spawn_graph` is absent.
    #[tokio::test]
    async fn chat_create_tools_string_array_filters_per_turn_tools_array() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let captured: std::sync::Arc<tokio::sync::Mutex<String>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let captured_clone = captured.clone();

        let _server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 8192];
            let mut acc = String::new();
            loop {
                let n = s.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                if let Some(headers_end) = acc.find("\r\n\r\n") {
                    let cl: usize = acc
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length:")
                                .or_else(|| l.strip_prefix("Content-Length:"))
                        })
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    let body_so_far = acc.len() - (headers_end + 4);
                    if body_so_far >= cl {
                        break;
                    }
                }
            }
            if let Some(idx) = acc.find("\r\n\r\n") {
                *captured_clone.lock().await = acc[idx + 4..].to_owned();
            }
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
                        data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                        data: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(response.as_bytes()).await;
            let _ = s.shutdown().await;
        });

        let (auth, tx, mut rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("test-model");
        let catalog = Arc::new(ToolCatalog::new());
        // Seed two tools: only `read_file` is in the chat's allowlist;
        // `spawn_graph` is in the catalog but must be filtered out.
        catalog
            .register_from(
                "basic-tools",
                vec![
                    openai_provider::catalog::ToolSpec {
                        name: "read_file".into(),
                        description: "Read a file.".into(),
                        parameters: serde_json::json!({"type":"object"}),
                    },
                    openai_provider::catalog::ToolSpec {
                        name: "spawn_graph".into(),
                        description: "Reasoner-graph internal.".into(),
                        parameters: serde_json::json!({"type":"object"}),
                    },
                ],
            )
            .await;
        let broker = Arc::new(ToolBroker::new());
        let mut config = cfg("ollama");
        config.base_url = format!("http://{}", addr);
        let client = reqwest::Client::builder().build().expect("client");

        // 1. chat.create with `tools = ["read_file"]`.
        let create_body = make_event_body(
            "ollama.chat.create",
            &[
                ("chat_id", Value::String("c-allow".into())),
                (
                    "tools",
                    Value::Array(vec![Value::String("read_file".into())]),
                ),
            ],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &create_body,
        )
        .await
        .expect("create");

        // 2. chat.append { role=user }.
        let append_body = make_event_body(
            "ollama.chat.append",
            &[
                ("chat_id", Value::String("c-allow".into())),
                (
                    "message",
                    serde_json::json!({"role": "user", "content": "go"}),
                ),
            ],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &append_body,
        )
        .await
        .expect("append");

        // 3. chat.complete (no extra_tools).
        let complete_body = make_event_body(
            "ollama.chat.complete",
            &[("chat_id", Value::String("c-allow".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &complete_body,
        )
        .await
        .expect("complete");

        // 4. Wait for chat.complete.result.
        let mut saw_complete_result = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                let line = msg.to_line();
                let v: Value = serde_json::from_str(&line).expect("json");
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    if body.get("kind").and_then(Value::as_str)
                        == Some("ollama.chat.complete.result")
                    {
                        saw_complete_result = true;
                        break;
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        assert!(
            saw_complete_result,
            "chat.complete.result must arrive after the SSE server closes",
        );

        // 5. Inspect the captured upstream body — only `read_file`
        //    should be present in the tools array. `spawn_graph` MUST
        //    have been filtered.
        let body = captured.lock().await.clone();
        assert!(!body.is_empty(), "server must have captured a request body",);
        let v: serde_json::Value = serde_json::from_str(&body).expect("upstream request body json");
        let tools = v
            .get("tools")
            .and_then(|t| t.as_array())
            .expect("upstream request body must include `tools`");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();
        assert!(
            names.contains(&"read_file"),
            "allowed tool `read_file` must be present in upstream tools; saw {names:?}",
        );
        assert!(
            !names.contains(&"spawn_graph"),
            "filtered tool `spawn_graph` MUST be absent from upstream tools; saw {names:?}",
        );
    }

    #[tokio::test]
    async fn chat_delete_removes_chat_and_emits_deleted() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        chats
            .create(ChatId::new("c-1"), None, None, None)
            .await
            .expect("seed");

        let body = make_event_body(
            "ollama.chat.delete",
            &[("chat_id", Value::String("c-1".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").and_then(Value::as_str),
            Some("ollama.chat.deleted")
        );
        assert!(!chats.exists(&ChatId::new("c-1")).await);
    }

    #[tokio::test]
    async fn legacy_prompt_creates_default_chat_lazily() {
        // The chat-app flow: nefor-chat sends `<prefix>.prompt`. Before
        // any chat exists, this must seed the per-prefix default chat
        // and append the user's text to it. We can't drive a real HTTP
        // turn from a unit test (no upstream), so we observe the
        // pre-turn state mutations by creating the chat ourselves with
        // a non-default model and asserting the prompt's text landed in
        // the default-chat history. Here we just check that after
        // dispatch the default chat exists.
        let (auth, tx, _rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        // Point at a localhost address that won't accept — the spawned
        // turn task hits a request error and exits cleanly. The
        // dispatcher itself still returns Ok, which is what we assert.
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body("ollama.prompt", &[("text", Value::String("hi".into()))]);
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let default_id = ChatId::default_for_prefix(&config.event_prefix());
        assert!(chats.exists(&default_id).await);
        let history = chats.history_snapshot(&default_id).await.expect("h");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn two_concurrent_chats_have_independent_histories() {
        let chats = fresh_chats("m");
        chats
            .create(ChatId::new("a"), None, None, None)
            .await
            .expect("a");
        chats
            .create(ChatId::new("b"), None, None, None)
            .await
            .expect("b");
        chats
            .push_user(&ChatId::new("a"), "alpha".into())
            .await
            .unwrap();
        chats
            .push_user(&ChatId::new("b"), "beta".into())
            .await
            .unwrap();
        let ha = chats.history_snapshot(&ChatId::new("a")).await.unwrap();
        let hb = chats.history_snapshot(&ChatId::new("b")).await.unwrap();
        assert_eq!(ha.len(), 1);
        assert_eq!(hb.len(), 1);
        assert_eq!(ha[0].content.as_deref(), Some("alpha"));
        assert_eq!(hb[0].content.as_deref(), Some("beta"));
    }

    // --- combinators.register on startup -----------------------------

    #[test]
    fn register_body_kind_is_combinators_register() {
        let b = register_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("combinators.register")
        );
    }

    #[test]
    fn register_body_declares_raw_request_and_raw_response_bare() {
        let b = register_body();
        let types = b
            .get("types")
            .and_then(Value::as_array)
            .expect("types array");
        let names: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
        assert!(names.contains(&"RawRequest"));
        assert!(names.contains(&"RawResponse"));
        // Bare names — no dots.
        for n in &names {
            assert!(!n.contains('.'), "type entry `{n}` must be bare");
        }
    }

    #[test]
    fn register_body_carries_two_into_entries_against_generic_provider() {
        let b = register_body();
        let impls = b
            .get("implementations")
            .and_then(Value::as_array)
            .expect("impls array");
        assert_eq!(impls.len(), 2);

        // Entry 0: Into<generic-provider.ProviderIn -> RawRequest>
        let e0 = impls[0].as_object().expect("obj");
        assert_eq!(e0.get("trait").and_then(Value::as_str), Some("Into"));
        assert_eq!(
            e0.get("in").and_then(Value::as_str),
            Some("generic-provider.ProviderIn")
        );
        assert_eq!(e0.get("out").and_then(Value::as_str), Some("RawRequest"));
        assert!(e0
            .get("handler")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false));

        // Entry 1: Into<RawResponse -> generic-provider.ProviderOut>
        let e1 = impls[1].as_object().expect("obj");
        assert_eq!(e1.get("trait").and_then(Value::as_str), Some("Into"));
        assert_eq!(e1.get("in").and_then(Value::as_str), Some("RawResponse"));
        assert_eq!(
            e1.get("out").and_then(Value::as_str),
            Some("generic-provider.ProviderOut")
        );
    }

    #[test]
    fn parse_provider_message_accepts_role_content_object() {
        let v = serde_json::json!({"role": "user", "content": "hi"});
        let m = parse_provider_message(Some(&v)).expect("ok");
        assert_eq!(m.role, "user");
        assert_eq!(m.content.as_deref(), Some("hi"));
    }

    #[test]
    fn parse_provider_message_round_trips_assistant_with_tool_calls() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":\"/x\"}"}
            }]
        });
        let m = parse_provider_message(Some(&v)).expect("ok");
        assert_eq!(m.role, "assistant");
        assert!(m.content.is_none());
        assert_eq!(m.tool_calls.len(), 1);
        assert_eq!(m.tool_calls[0].id, "call_1");
    }

    #[test]
    fn chat_complete_result_body_carries_provider_out_shaped_output() {
        let cid = ChatId::new("c-1");
        let calls = vec![ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{\"path\":\"/x\"}".into(),
            },
        }];
        let b = chat_complete_result_body(
            &cfg("ollama"),
            &cid,
            "Done.",
            &calls,
            Some("tool_calls"),
            10,
            5,
            "qwen",
            "",
        );
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("ollama.chat.complete.result")
        );
        assert_eq!(b.get("chat_id").and_then(Value::as_str), Some("c-1"));
        let out = b.get("output").and_then(Value::as_object).expect("output");
        assert_eq!(out.get("text").and_then(Value::as_str), Some("Done."));
        // Empty reasoning is dropped from the wire shape (back-compat).
        assert!(out.get("reasoning").is_none());
        assert_eq!(
            out.get("finish_reason").and_then(Value::as_str),
            Some("tool_calls")
        );
        let tcs = out
            .get("tool_calls")
            .and_then(Value::as_array)
            .expect("tool_calls");
        assert_eq!(tcs.len(), 1);
        let entry = tcs[0].as_object().expect("entry");
        assert_eq!(entry.get("id").and_then(Value::as_str), Some("call_1"));
        assert_eq!(entry.get("name").and_then(Value::as_str), Some("read_file"));
        let usage = out.get("usage").and_then(Value::as_object).expect("usage");
        assert_eq!(usage.get("prompt_tokens").and_then(Value::as_u64), Some(10));
        assert_eq!(
            usage.get("completion_tokens").and_then(Value::as_u64),
            Some(5)
        );
        assert_eq!(usage.get("model").and_then(Value::as_str), Some("qwen"));
    }

    /// Per-chat interrupt fix (companion to `ebea3b8`): `<prefix>.interrupt`
    /// with a `chat_id` MUST cancel only that chat's in-flight turn.
    /// Pre-fix the handler called `chats.interrupt_all()` regardless,
    /// so the agent reasoner's per-firing fanout up-converted into a
    /// global cancel that nuked the lead's chat too.
    #[tokio::test]
    async fn interrupt_with_chat_id_targets_only_that_chat() {
        let (auth, tx, _rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let id_a = ChatId::new("chat-1");
        let id_b = ChatId::new("chat-2");
        chats
            .create(id_a.clone(), None, None, None)
            .await
            .expect("a");
        chats
            .create(id_b.clone(), None, None, None)
            .await
            .expect("b");
        let tok_a = chats.begin_turn(&id_a).await.expect("begin a");
        let tok_b = chats.begin_turn(&id_b).await.expect("begin b");

        let body = make_event_body(
            "ollama.interrupt",
            &[("chat_id", Value::String("chat-1".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &body,
        )
        .await
        .expect("dispatch ok");

        assert!(
            tok_a.is_cancelled(),
            "chat-1 cancel token must fire for the targeted interrupt",
        );
        assert!(
            !tok_b.is_cancelled(),
            "chat-2 cancel token MUST NOT fire when interrupt targets chat-1; \
             pre-fix this would be true (interrupt_all up-converted the fanout)",
        );
    }

    /// Backwards-compat: bare `<prefix>.interrupt` (no chat_id) keeps
    /// the original `ef260cd` shape — cancel every in-flight turn. The
    /// chat-side `/cancel` path emits the bare envelope.
    #[tokio::test]
    async fn interrupt_without_chat_id_falls_back_to_interrupt_all() {
        let (auth, tx, _rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let id_a = ChatId::new("chat-1");
        let id_b = ChatId::new("chat-2");
        chats
            .create(id_a.clone(), None, None, None)
            .await
            .expect("a");
        chats
            .create(id_b.clone(), None, None, None)
            .await
            .expect("b");
        let tok_a = chats.begin_turn(&id_a).await.expect("begin a");
        let tok_b = chats.begin_turn(&id_b).await.expect("begin b");

        let body = make_event_body("ollama.interrupt", &[]);
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &body,
        )
        .await
        .expect("dispatch ok");

        assert!(tok_a.is_cancelled(), "bare interrupt cancels chat-1");
        assert!(tok_b.is_cancelled(), "bare interrupt cancels chat-2");
    }

    /// Unknown chat_id is a no-op: neither `interrupt_all` (would punish
    /// every live chat for a misrouted envelope) nor an error (the
    /// firing might already have closed and de-registered).
    #[tokio::test]
    async fn interrupt_with_unknown_chat_id_is_noop() {
        let (auth, tx, _rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let id_a = ChatId::new("chat-1");
        let id_b = ChatId::new("chat-2");
        chats
            .create(id_a.clone(), None, None, None)
            .await
            .expect("a");
        chats
            .create(id_b.clone(), None, None, None)
            .await
            .expect("b");
        let tok_a = chats.begin_turn(&id_a).await.expect("begin a");
        let tok_b = chats.begin_turn(&id_b).await.expect("begin b");

        let body = make_event_body(
            "ollama.interrupt",
            &[("chat_id", Value::String("chat-nonexistent".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &body,
        )
        .await
        .expect("dispatch ok");

        assert!(
            !tok_a.is_cancelled(),
            "unknown chat_id MUST NOT cancel chat-1 (would imply interrupt_all fallback)",
        );
        assert!(
            !tok_b.is_cancelled(),
            "unknown chat_id MUST NOT cancel chat-2",
        );
    }
}
