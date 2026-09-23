// openai-provider — generic NCP v0.1 plugin for OpenAI-compatible
// chat-completions endpoints. One binary, N spawns: NCP can launch
// this same executable under several plugin names, each with its own
// `--name` CLI flag so the per-instance event-kind prefix
// (`ollama.*`, `groq.*`, `openrouter.*`, …) doesn't collide on the bus.
//
// Stage 1 reshape (per nefor-agent-and-reasoner-types §2 "Layer A —
// Providers"): the singleton `SessionState` is gone. The provider now
// manages **chats** as a chat-id-keyed map (`state::Chats`). Each chat
// is `(model, message_history, in-flight slot, stats)`. The provider
// can hold N concurrent chats — exactly the shape the parent spec
// requires from a "dumb runner that speaks one wire protocol."
//
// ### Wire-level chat API (new)
//
// - `<prefix>.chat.create  { chat_id, model?, system? }` — make a new
//   chat; errors if `chat_id` already exists.
// - `<prefix>.chat.append  { chat_id, message }` — append a message
//   (typically the user's turn). No upstream call.
// - `<prefix>.chat.complete { chat_id }` — send the chat's history to
//   the upstream model, stream deltas, append the assistant message,
//   reply with `<prefix>.chat.complete.result { chat_id, output }`
//   where `output` follows `generic-provider.ProviderOut`'s shape.
// - `<prefix>.chat.delete  { chat_id }` — drop the chat.
//
// ### Legacy wire API (compat for nefor-chat)
//
// - `<prefix>.prompt { text }` — operates on the per-prefix default
//   chat (`<prefix>:default`); ensure-default + append-user +
//   complete in one shot. The chat plugin keeps using this until T7
//   rewires it to drive `chat.*` directly.
// - `<prefix>.interrupt`, `<prefix>.reset` — operate on the default
//   chat for the same reason.
// - `<prefix>.auth.set`, `<prefix>.login_requested`,
//   `<prefix>.logout_requested`, `<prefix>.model.set`,
//   `<prefix>.models.list_requested` — provider-wide; not chat-scoped.
//
// ### Combinators registration
//
// On startup we declare two bare types (`RawRequest`, `RawResponse`)
// and two `Into` conversions against `generic-provider`'s canonical
// types (`ProviderRequest`, `ProviderInput`). See the `register_body`
// constructor for the literal shape and the spec gap that this plugin
// intentionally exercises (`Into.in` cross-namespace).

mod error;

use std::sync::Arc;
use std::time::{Duration, Instant};

use nefor_plugin_sdk::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, TransportError};
use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody};

const CHANNEL_CAP: usize = 256;
use openai_provider::auth::{AuthSnapshot, AuthState, AuthStore, LogoutOutcome};
use openai_provider::broker::{ToolBroker, ToolResult};
use openai_provider::catalog::ToolCatalog;
use openai_provider::config::Config;
use openai_provider::openai::{
    Message, ModelInfo, ReasoningContinuation, ToolCall, REASONING_CONTEXT_FORMAT,
};
use openai_provider::state::{
    ChatId, ChatRestore, ChatStats, Chats, ChatsError, CompletionRuns, TurnToken,
};
use openai_provider::stream::{
    list_models, run_chat_stream_with_retry_progress_and_format,
    run_chat_stream_with_retry_progress_and_format_and_additions, ReasoningEvent, RetryProgress,
    StreamError, StreamOutcome,
};
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::error::LlmError;

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
    let (client_builder, roots) = nefor_provider_http::client_builder()?;
    tracing::info!(
        native_roots = roots.loaded,
        rejected_native_roots = roots.rejected,
        "provider HTTPS trust initialized"
    );
    let client = client_builder
        .connect_timeout(Duration::from_secs(10))
        .build()?;

    let (out_tx, _writer_handle) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let _reader_handle = spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = await_ready_ok(&mut in_rx).await?;
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
    let completions = Arc::new(CompletionRuns::new());
    let catalog = Arc::new(ToolCatalog::new());
    let broker = Arc::new(ToolBroker::new());

    run_dispatch_loop(
        &chats,
        &completions,
        &auth,
        &catalog,
        &broker,
        &config,
        &client,
        &out_tx,
        &mut in_rx,
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
    completions: &Arc<CompletionRuns>,
    auth: &Arc<AuthStore>,
    catalog: &Arc<ToolCatalog>,
    broker: &Arc<ToolBroker>,
    config: &Config,
    client: &reqwest::Client,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    in_rx: &mut mpsc::Receiver<Result<Envelope, TransportError>>,
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
                            dispatch_event_with_completions(
                                chats,
                                completions,
                                auth,
                                catalog,
                                broker,
                                config,
                                client,
                                out_tx,
                                &env.from,
                                map,
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
async fn dispatch_event_with_completions(
    chats: &Arc<Chats>,
    completions: &Arc<CompletionRuns>,
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

    let prefix = config.event_prefix();
    if kind == format!("{prefix}completion.request") {
        dispatch_completion_request(completions, auth, catalog, config, client, out_tx, body)
            .await?;
        return Ok(());
    }
    if kind == format!("{prefix}completion.cancel") {
        if let Some(request_id) = body.get("request_id").and_then(Value::as_str) {
            completions.cancel(request_id).await;
        }
        return Ok(());
    }

    dispatch_event(
        chats, auth, catalog, broker, config, client, out_tx, from, body,
    )
    .await
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
            let output = body.get("output").map(tool_output_for_text_model);
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
            let system = body
                .get("system")
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
            let reasoning_effort = body
                .get("reasoning_effort")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            tracing::info!(
                target: "openai_provider::chat",
                chat_id = %chat_id,
                model = ?model,
                tools_enabled = ?tools_enabled,
                tool_allowlist_len = tool_allowlist.as_ref().map(Vec::len),
                reasoning_effort = ?reasoning_effort,
                system_len = system.as_ref().map(String::len),
                "chat.create",
            );
            match chats
                .create(
                    chat_id.clone(),
                    model,
                    tools_enabled,
                    tool_allowlist,
                    reasoning_effort,
                    system,
                )
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
        "chat.restore" => {
            let chat_id = match read_chat_id(body) {
                Some(id) => id,
                None => {
                    send_event(
                        out_tx,
                        turn_error_body(config, "chat.restore missing `chat_id`"),
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
            let tools_field = body.get("tools");
            let tools_enabled = tools_field.and_then(Value::as_bool);
            let tool_allowlist: Option<Vec<String>> =
                tools_field.and_then(Value::as_array).map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                });
            let reasoning_effort = body
                .get("reasoning_effort")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let mut history = Vec::new();
            let model = match model.or(chats.default_model().await) {
                Some(model) => model,
                None => {
                    send_event(
                        out_tx,
                        chat_error_body(config, &chat_id, &ChatsError::NoModelConfigured),
                    )
                    .await?;
                    return Ok(());
                }
            };
            if let Some(items) = body.get("history").and_then(Value::as_array) {
                for item in items {
                    let parsed = match parse_provider_message(
                        Some(item),
                        &config.provider_name,
                        &config.base_url,
                        Some(&model),
                    ) {
                        Ok(m) => m,
                        Err(msg) => {
                            send_event(out_tx, chat_error_body_msg(config, &chat_id, msg)).await?;
                            return Ok(());
                        }
                    };
                    history.push(parsed.message);
                    for failure in &parsed.tool_call_failures {
                        if let Some(id) = &failure.id {
                            history.push(Message::tool_result(
                                id.clone(),
                                format!(
                                    "Failed to parse tool call: {}. Raw: {}",
                                    failure.error, failure.raw
                                ),
                            ));
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
            tracing::info!(
                target: "openai_provider::chat",
                chat_id = %chat_id,
                model = ?model,
                history_len = history.len(),
                "chat.restore",
            );
            match chats
                .restore(ChatRestore {
                    id: chat_id.clone(),
                    model: Some(model),
                    tools_enabled,
                    tool_allowlist,
                    reasoning_effort,
                    system,
                    history,
                })
                .await
            {
                Ok(()) => {
                    send_event(out_tx, chat_appended_body(config, &chat_id)).await?;
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
            let active_model = match chats.model(&chat_id).await {
                Ok(model) => model,
                Err(error) => {
                    send_event(out_tx, chat_error_body(config, &chat_id, &error)).await?;
                    return Ok(());
                }
            };
            let parsed = match parse_provider_message(
                body.get("message"),
                &config.provider_name,
                &config.base_url,
                Some(&active_model),
            ) {
                Ok(m) => m,
                Err(msg) => {
                    send_event(out_tx, chat_error_body_msg(config, &chat_id, msg)).await?;
                    return Ok(());
                }
            };
            tracing::info!(
                target: "openai_provider::chat",
                chat_id = %chat_id,
                role = %parsed.message.role(),
                content_len = parsed.message.content().map(str::len).unwrap_or(0),
                content_preview = %parsed.message
                    .content()
                    .map(|s| s.chars().take(80).collect::<String>())
                    .unwrap_or_default(),
                "chat.append",
            );
            if let Err(e) = chats
                .append_for_model(&chat_id, &active_model, parsed.message)
                .await
            {
                send_event(out_tx, chat_error_body(config, &chat_id, &e)).await?;
            } else {
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
            let response_format = body.get("output_schema").map(|schema| {
                serde_json::json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": "mag_output",
                        "strict": true,
                        "schema": schema,
                    }
                })
            });
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
                response_format,
            )
            .await?;
        }
        "chat.compact" => {
            let chat_id = match read_chat_id(body) {
                Some(id) => id,
                None => {
                    send_event(
                        out_tx,
                        turn_error_body(config, "chat.compact missing `chat_id`"),
                    )
                    .await?;
                    return Ok(());
                }
            };
            send_event(
                out_tx,
                chat_error_body_msg(
                    config,
                    &chat_id,
                    "native compaction is not supported by openai-provider".into(),
                ),
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
        // Hard cancel of an in-flight completion, keyed by the caller's
        // request id (`chat_id` — one in-flight completion per chat).
        // Aborts the streaming HTTP call and suppresses the terminal
        // result. Idempotent: unknown or already-finished ids are a
        // logged no-op, never an error. Pure capability-surface
        // completion — this handler knows nothing about actors, runs, or
        // graphs.
        "chat.cancel" => match read_chat_id(body) {
            Some(cid) => {
                if !chats.cancel_turn(&cid).await {
                    tracing::debug!(
                        chat_id = %cid,
                        "chat.cancel for unknown or finished request; no-op"
                    );
                }
            }
            None => {
                tracing::debug!("chat.cancel without chat_id; no-op");
            }
        },

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
                None,
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
                        StreamError::Malformed(message) => {
                            format!("malformed streamed response: {message}")
                        }
                        StreamError::Provider { message, .. } => {
                            format!("provider stream error: {message}")
                        }
                        StreamError::Refusal(message) => {
                            format!("provider refused the request: {message}")
                        }
                        StreamError::IncompleteToolCall { index, missing } => {
                            format!(
                                "incomplete streamed tool call at index {index}: missing {missing}"
                            )
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
        "reasoning.set" => {
            let effort = match body
                .get("effort")
                .or_else(|| body.get("reasoning_effort"))
                .and_then(Value::as_str)
            {
                Some(e) if !e.is_empty() => e.to_owned(),
                _ => {
                    tracing::warn!("reasoning.set without non-empty effort; ignoring");
                    return Ok(());
                }
            };
            let active_id = body
                .get("chat_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(ChatId::new);
            if let Some(chat_id) = &active_id {
                if chats.exists(chat_id).await {
                    let _ = chats
                        .set_chat_reasoning_effort(chat_id, effort.clone())
                        .await;
                }
            }
            send_event(
                out_tx,
                reasoning_set_ack_body(config, &effort, active_id.as_ref()),
            )
            .await?;
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

async fn dispatch_completion_request(
    completions: &Arc<CompletionRuns>,
    auth: &Arc<AuthStore>,
    catalog: &Arc<ToolCatalog>,
    config: &Config,
    client: &reqwest::Client,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
) -> Result<(), LlmError> {
    let Some(request_id) = body
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
    else {
        send_event(
            out_tx,
            completion_error_body(config, "", "completion.request missing `request_id`"),
        )
        .await?;
        return Ok(());
    };

    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
        .or_else(|| config.model.clone());
    let messages = match body.get("messages").and_then(Value::as_array) {
        Some(items) => {
            let mut messages = Vec::with_capacity(items.len());
            for item in items {
                match parse_provider_message(
                    Some(item),
                    &config.provider_name,
                    &config.base_url,
                    requested_model.as_deref(),
                ) {
                    Ok(parsed) if parsed.tool_call_failures.is_empty() => {
                        messages.push(parsed.message)
                    }
                    Ok(_) => {
                        send_event(
                            out_tx,
                            completion_error_body(
                                config,
                                &request_id,
                                "invalid tool call in completion history",
                            ),
                        )
                        .await?;
                        return Ok(());
                    }
                    Err(message) => {
                        send_event(out_tx, completion_error_body(config, &request_id, &message))
                            .await?;
                        return Ok(());
                    }
                }
            }
            messages
        }
        None => {
            send_event(
                out_tx,
                completion_error_body(config, &request_id, "completion.request missing `messages`"),
            )
            .await?;
            return Ok(());
        }
    };
    if !completion_request_has_model_input(&messages) {
        send_event(
            out_tx,
            completion_error_body(
                config,
                &request_id,
                "completion request needs at least one non-empty user message or tool result",
            ),
        )
        .await?;
        return Ok(());
    }
    let messages = completion_request_messages(body, messages);

    let run = match completions.begin(request_id.clone()).await {
        Ok(run) => run,
        Err(message) => {
            send_event(out_tx, completion_error_body(config, &request_id, &message)).await?;
            return Ok(());
        }
    };
    let cancel = run.cancellation_token();
    let Some(model) = requested_model else {
        completions.finish(&request_id, &run).await;
        send_event(
            out_tx,
            completion_error_body(config, &request_id, "no model configured"),
        )
        .await?;
        return Ok(());
    };
    let reasoning_effort = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let response_format = body.get("output_schema").map(|schema| {
        serde_json::json!({
            "type": "json_schema",
            "json_schema": {"name": "mag_output", "strict": true, "schema": schema},
        })
    });
    let authored_tools = body.get("tools").and_then(Value::as_array);
    let authored_names = authored_tools
        .map(|tools| {
            tools
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let direct_specs = match body.get("tool_specs") {
        Some(value @ Value::Array(_)) => ToolCatalog::parse_tools(value),
        None => Vec::new(),
        Some(_) => {
            completions.finish(&request_id, &run).await;
            send_event(
                out_tx,
                completion_error_body(
                    config,
                    &request_id,
                    "completion.request `tool_specs` must be an array",
                ),
            )
            .await?;
            return Ok(());
        }
    };
    let advertised = catalog.project_names(&authored_names).await;
    let conflicts = direct_specs.iter().any(|direct| {
        !direct.owner.is_empty()
            && advertised
                .iter()
                .any(|spec| spec.name == direct.name && spec.owner != direct.owner)
    });
    if conflicts {
        completions.finish(&request_id, &run).await;
        send_event(
            out_tx,
            completion_error_body(
                config,
                &request_id,
                "request-local tool owner conflicts with an advertised tool",
            ),
        )
        .await?;
        return Ok(());
    }
    let projected_specs = if authored_names.is_empty() {
        authored_tools
            .map(|tools| ToolCatalog::parse_tools(&Value::Array(tools.clone())))
            .unwrap_or_default()
    } else {
        authored_names
            .iter()
            .filter_map(|name| {
                let known = advertised.iter().find(|spec| &spec.name == name);
                if known.is_some_and(|spec| !spec.execution.is_routed()) {
                    return known.cloned();
                }
                direct_specs
                    .iter()
                    .find(|spec| &spec.name == name && spec.execution.is_routed())
                    .or(known)
                    .cloned()
            })
            .collect()
    };
    let projected_names = projected_specs
        .iter()
        .map(|spec| spec.name.clone())
        .collect::<Vec<_>>();
    let missing = authored_names
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
    let mut tools = ToolCatalog::format_openai_tools(projected_specs.iter());
    if let Some(extra) = body.get("extra_tools").and_then(Value::as_array) {
        tools.extend(extra.iter().cloned());
    }
    let tools = if tools.is_empty() { None } else { Some(tools) };
    let request_additions = match body.get("request_additions") {
        Some(Value::Object(additions)) => Some(additions.clone()),
        Some(_) => {
            completions.finish(&request_id, &run).await;
            send_event(
                out_tx,
                completion_error_body(
                    config,
                    &request_id,
                    "completion.request `request_additions` must be an object",
                ),
            )
            .await?;
            return Ok(());
        }
        None => None,
    };

    let completions = Arc::clone(completions);
    let auth = Arc::clone(auth);
    let config = config.clone();
    let client = client.clone();
    let out_tx = out_tx.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        let event_prefix = config.event_prefix();
        let token = auth.token().await;
        let tools_supported = completions.model_supports_tools(&model).await;
        let attempted_tools = tools_supported && tools.is_some();
        let mut result = run_completion_attempt(
            &client,
            &config,
            token.as_deref(),
            &model,
            &messages,
            if tools_supported {
                tools.as_deref()
            } else {
                None
            },
            reasoning_effort.as_deref(),
            response_format.as_ref(),
            request_additions.as_ref(),
            cancel.clone(),
            &out_tx,
            &request_id,
        )
        .await;

        if attempted_tools
            && matches!(result, Err(StreamError::ToolsUnsupported { .. }))
            && !cancel.is_cancelled()
        {
            completions.mark_model_tools_unsupported(&model).await;
            result = run_completion_attempt(
                &client,
                &config,
                token.as_deref(),
                &model,
                &messages,
                None,
                reasoning_effort.as_deref(),
                response_format.as_ref(),
                request_additions.as_ref(),
                cancel.clone(),
                &out_tx,
                &request_id,
            )
            .await;
        }

        completions.finish(&request_id, &run).await;
        let cancelled = cancel.is_cancelled();
        if cancelled && !matches!(&result, Ok(outcome) if outcome.usage.is_some()) {
            return;
        }
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(outcome) => {
                if let Some(usage) = outcome.usage.clone() {
                    let mut fields = vec![
                        ("usage", usage.into_ncp_value()),
                        ("model", Value::String(model.clone())),
                        ("duration_ms", Value::Number(elapsed_ms.into())),
                    ];
                    if let Some(completion_id) = &outcome.completion_id {
                        fields.push(("completion_id", Value::String(completion_id.clone())));
                    }
                    let _ = out_tx
                        .send(PluginOutgoing::event(completion_event_body_from_iter(
                            &event_prefix,
                            &request_id,
                            "usage",
                            fields,
                        )))
                        .await;
                }
                if cancelled {
                    return;
                }
                for call in &outcome.tool_calls {
                    let arguments = serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| Value::String(call.function.arguments.clone()));
                    let _ = out_tx
                        .send(PluginOutgoing::event(completion_event_body(
                            &event_prefix,
                            &request_id,
                            "tool_call",
                            [
                                ("id", Value::String(call.id.clone())),
                                ("name", Value::String(call.function.name.clone())),
                                ("arguments", arguments),
                            ],
                        )))
                        .await;
                }
                let provider_context = reasoning_provider_context(
                    &config,
                    &model,
                    outcome.reasoning_continuation.as_ref(),
                );
                let mut fields = vec![
                    ("text", Value::String(outcome.full_text)),
                    ("reasoning", Value::String(outcome.reasoning_text)),
                    (
                        "finish_reason",
                        outcome
                            .finish_reason
                            .map(Value::String)
                            .unwrap_or(Value::Null),
                    ),
                    ("model", Value::String(model.clone())),
                    ("duration_ms", Value::Number(elapsed_ms.into())),
                ];
                if let Some(provider_context) = provider_context {
                    fields.push(("provider_context", provider_context));
                }
                if let Some(completion_id) = outcome.completion_id {
                    fields.push(("completion_id", Value::String(completion_id)));
                }
                let _ = out_tx
                    .send(PluginOutgoing::event(completion_event_body_from_iter(
                        &event_prefix,
                        &request_id,
                        "completed",
                        fields,
                    )))
                    .await;
            }
            Err(error) => {
                let _ = out_tx
                    .send(PluginOutgoing::event(completion_event_body(
                        &event_prefix,
                        &request_id,
                        "error",
                        [
                            ("message", Value::String(error.to_string())),
                            ("model", Value::String(model)),
                            ("duration_ms", Value::Number(elapsed_ms.into())),
                        ],
                    )))
                    .await;
            }
        }
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_completion_attempt(
    client: &reqwest::Client,
    config: &Config,
    token: Option<&str>,
    model: &str,
    messages: &[Message],
    tools: Option<&[Value]>,
    reasoning_effort: Option<&str>,
    response_format: Option<&Value>,
    request_additions: Option<&Map<String, Value>>,
    cancel: CancellationToken,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    request_id: &str,
) -> Result<StreamOutcome, StreamError> {
    let event_prefix = config.event_prefix();
    let text_event_prefix = event_prefix.clone();
    let reasoning_event_prefix = event_prefix.clone();
    let retry_event_prefix = event_prefix;
    let id_for_text = request_id.to_owned();
    let tx_for_text = out_tx.clone();
    let suppress_text_deltas = response_format.is_some();
    let id_for_reasoning = request_id.to_owned();
    let tx_for_reasoning = out_tx.clone();
    let id_for_retry = request_id.to_owned();
    let tx_for_retry = out_tx.clone();
    let mut reasoning_started_at: Option<Instant> = None;

    run_chat_stream_with_retry_progress_and_format_and_additions(
        client,
        &config.chat_endpoint(),
        token,
        &config.auth_header,
        model,
        messages,
        tools,
        reasoning_effort,
        response_format,
        request_additions,
        cancel,
        move |text| {
            if !suppress_text_deltas {
                let _ = tx_for_text.try_send(PluginOutgoing::event(completion_event_body(
                    &text_event_prefix,
                    &id_for_text,
                    "text_delta",
                    [("text", Value::String(text.to_owned()))],
                )));
            }
        },
        move |event| match event {
            ReasoningEvent::Delta(text) => {
                if reasoning_started_at.is_none() {
                    reasoning_started_at = Some(Instant::now());
                }
                let _ = tx_for_reasoning.try_send(PluginOutgoing::event(completion_event_body(
                    &reasoning_event_prefix,
                    &id_for_reasoning,
                    "reasoning_delta",
                    [("text", Value::String(text.to_owned()))],
                )));
            }
            ReasoningEvent::End { text } => {
                let duration_ms = reasoning_started_at
                    .map(|started| started.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                let _ = tx_for_reasoning.try_send(PluginOutgoing::event(completion_event_body(
                    &reasoning_event_prefix,
                    &id_for_reasoning,
                    "reasoning_end",
                    [
                        ("text", Value::String(text.to_owned())),
                        ("duration_ms", Value::Number(duration_ms.into())),
                    ],
                )));
            }
        },
        move |progress| {
            let _ = tx_for_retry.try_send(PluginOutgoing::event(completion_event_body(
                &retry_event_prefix,
                &id_for_retry,
                "retry",
                [
                    ("attempt", Value::Number(progress.retry_index().into())),
                    (
                        "delay_ms",
                        Value::Number((progress.next_delay.as_millis() as u64).into()),
                    ),
                    (
                        "error",
                        progress
                            .status
                            .map(|status| Value::String(format!("HTTP {status}")))
                            .unwrap_or(Value::String("transient provider error".into())),
                    ),
                ],
            )));
        },
    )
    .await
}

fn completion_event_body<const N: usize>(
    prefix: &str,
    request_id: &str,
    event: &str,
    fields: [(&str, Value); N],
) -> Map<String, Value> {
    completion_event_body_from_iter(prefix, request_id, event, fields)
}

fn completion_event_body_from_iter<'a>(
    prefix: &str,
    request_id: &str,
    event: &str,
    fields: impl IntoIterator<Item = (&'a str, Value)>,
) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert(
        "kind".into(),
        Value::String(format!("{prefix}completion.event")),
    );
    body.insert("request_id".into(), Value::String(request_id.to_owned()));
    body.insert("event".into(), Value::String(event.to_owned()));
    for (name, value) in fields {
        body.insert(name.to_owned(), value);
    }
    body
}

fn completion_error_body(config: &Config, request_id: &str, message: &str) -> Map<String, Value> {
    completion_event_body(
        &config.event_prefix(),
        request_id,
        "error",
        [("message", Value::String(message.to_owned()))],
    )
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
    response_format: Option<Value>,
) -> Result<(), LlmError> {
    let history = match chats.request_history_snapshot(&chat_id).await {
        Ok(h) => h,
        Err(e) => {
            send_event(out_tx, chat_error_body(config, &chat_id, &e)).await?;
            return Ok(());
        }
    };
    if !request_history_has_model_input(&history) {
        let message =
            "openai-provider: chat.complete needs at least one non-empty user message or tool result";
        let body = if legacy_default_chat {
            turn_error_body(config, message)
        } else {
            chat_error_body_msg(config, &chat_id, message.to_owned())
        };
        send_event(out_tx, body).await?;
        return Ok(());
    }

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
        response_format,
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
    cancel: TurnToken,
    legacy_default_chat: bool,
    extra_tools: Vec<Value>,
    response_format: Option<Value>,
) {
    tokio::spawn(async move {
        // The streaming/tool helpers only need the abort signal; the
        // suppress flag stays with `cancel` for this task's own
        // terminal-emission decisions.
        let cancel_token = cancel.cancellation_token();
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
        let mut observed_usage = false;
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
        let mut final_reasoning_continuation: Option<ReasoningContinuation> = None;
        let mut final_error: Option<String> = None;
        let mut interrupted = false;
        let mut errored = false;

        loop {
            iterations += 1;
            let history = match chats
                .request_history_snapshot_for_model(&chat_id, &active_model)
                .await
            {
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
            if tracing::enabled!(tracing::Level::INFO) {
                tracing::info!(
                    target: "openai_provider::turn",
                    chat_id = %chat_id,
                    iteration = iterations,
                    history_len = history.len(),
                    roles = ?history.iter().map(|m| m.role()).collect::<Vec<_>>(),
                    "history snapshot for turn iteration",
                );
            }
            // Per-model cache: a model the upstream previously rejected
            // with the "does not support tools" signature stays disabled
            // for the rest of this process — even on a fresh chat.
            let model_tools_supported = chats.model_supports_tools(&active_model).await;
            // Per-chat tool allowlist (sole mechanism for tool control):
            //   None          → all tools (no filter)
            //   Some(vec![])  → no tools (disabled)
            //   Some(names)   → filtered to those tool names
            let chat_tool_allowlist = chats.tool_allowlist(&chat_id).await.unwrap_or(None);
            let tools_disabled = !model_tools_supported
                || matches!(&chat_tool_allowlist, Some(names) if names.is_empty());
            let mut tools_array = if tools_disabled {
                Vec::new()
            } else {
                catalog.to_openai_tools().await
            };
            // Per-firing extra_tools (e.g. agent reasoner's `finalize`
            // synthetic terminator). Appended AFTER the catalog so a
            // catalog entry of the same name still wins on iteration.
            // Skipped when tools are disabled entirely.
            if !tools_disabled && !extra_tools.is_empty() {
                tools_array.extend(extra_tools.iter().cloned());
            }
            // Apply the per-chat name filter. Entries not in the list
            // are dropped; the caller that wanted `finalize` injected
            // per firing must also include `finalize` in the chat's
            // allowlist.
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
            let reasoning_effort = chats.reasoning_effort(&chat_id).await.unwrap_or(None);

            let endpoint = config.chat_endpoint();
            let id_for_delta = turn_id.clone();
            let chat_id_for_delta = chat_id.clone();
            let out_tx_for_delta = out_tx.clone();
            let prefix_for_delta = config.event_prefix();
            let id_for_reason = turn_id.clone();
            let chat_id_for_reason = chat_id.clone();
            let out_tx_for_reason = out_tx.clone();
            let prefix_for_reason = config.event_prefix();
            let id_for_retry = turn_id.clone();
            let chat_id_for_retry = chat_id.clone();
            let out_tx_for_retry = out_tx.clone();
            let prefix_for_retry = config.event_prefix();
            let token = auth.token().await;
            // Stamp the reasoning duration from first reasoning chunk
            // → ReasoningEvent::End. Captured in the closure so each
            // firing in a tool loop gets its own timer (per-firing
            // reasoning, never accumulated across firings).
            let mut reasoning_started_at: Option<std::time::Instant> = None;
            let result = run_chat_stream_with_retry_progress_and_format(
                &client,
                &endpoint,
                token.as_deref(),
                &config.auth_header,
                &active_model,
                &history,
                tools_slice,
                reasoning_effort.as_deref(),
                response_format.as_ref(),
                cancel_token.clone(),
                |delta| {
                    if response_format.is_some() {
                        return;
                    }
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
                |progress| {
                    let body = retry_progress_body(
                        &prefix_for_retry,
                        &id_for_retry,
                        &chat_id_for_retry,
                        progress,
                    );
                    let _ = out_tx_for_retry.try_send(PluginOutgoing::event(body));
                },
            )
            .await;
            drop(history);

            // Hard cancel arrived mid-stream: drop this turn entirely.
            // The HTTP stream was already aborted when
            // `run_chat_stream`'s select took the `cancelled()` branch;
            // suppress every terminal emission by breaking out before
            // any result is built or partial text is persisted. A
            // graceful `interrupt` is NOT suppressed and falls through
            // to the interrupted-result path below.
            if cancel.is_suppressed() {
                break;
            }

            match result {
                Ok(outcome) => {
                    if let Some(u) = outcome.usage {
                        observed_usage = true;
                        total_prompt_tokens =
                            total_prompt_tokens.saturating_add(u.prompt_tokens.unwrap_or_default());
                        total_completion_tokens = total_completion_tokens
                            .saturating_add(u.completion_tokens.unwrap_or_default());
                    }

                    if outcome.interrupted {
                        // A provider-native reasoning artifact is valid only for a
                        // completed assistant response. Do not retain a partial
                        // assistant without the continuation its provider requires.
                        if !outcome.full_text.is_empty() && outcome.reasoning_continuation.is_none()
                        {
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
                        // Persist the raw assistant turn. Provider-bound serialization
                        // normalizes malformed arguments without changing what tool
                        // execution receives below.
                        let assistant = if outcome.full_text.is_empty() {
                            Message::assistant_tool_calls(outcome.tool_calls.clone())
                        } else {
                            Message::assistant_with_tool_calls(
                                outcome.full_text.clone(),
                                outcome.tool_calls.clone(),
                            )
                        }
                        .with_reasoning(outcome.reasoning_continuation.clone());
                        let _ = chats
                            .append_for_model(&chat_id, &active_model, assistant)
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
                            final_reasoning_continuation = outcome.reasoning_continuation;
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
                                run_one_tool_call(&catalog, &broker, &out_tx, &cancel_token, tc)
                                    .await;
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
                        let assistant = Message::assistant(outcome.full_text.clone())
                            .with_reasoning(outcome.reasoning_continuation.clone());
                        let _ = chats
                            .append_for_model(&chat_id, &active_model, assistant)
                            .await;
                    }
                    final_text = outcome.full_text;
                    final_reasoning = outcome.reasoning_text;
                    final_reasoning_continuation = outcome.reasoning_continuation;
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
                    let _ = chats.set_tool_allowlist(&chat_id, Some(vec![])).await;
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
                        StreamError::Malformed(message) => {
                            format!("malformed streamed response: {message}")
                        }
                        StreamError::Provider { message, .. } => {
                            format!("provider stream error: {message}")
                        }
                        StreamError::Refusal(message) => {
                            format!("provider refused the request: {message}")
                        }
                        StreamError::IncompleteToolCall { index, missing } => {
                            format!(
                                "incomplete streamed tool call at index {index}: missing {missing}"
                            )
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
                    final_error = Some(msg.clone());
                    let _ = out_tx
                        .send(PluginOutgoing::event(turn_error_body(&config, &msg)))
                        .await;
                    break;
                }
            }
        }

        // Suppressed hard-cancel: release the slot and return without
        // emitting stream.end / session.stats / turn.error /
        // chat.complete.result. No result is delivered for a cancelled
        // request — that is the honor side of the kernel's kill flush.
        // Any partial stream.delta already put on the bus before the
        // abort is unavoidable (it was emitted live), but no terminal
        // completion lands.
        if cancel.is_suppressed() {
            chats.end_turn(&chat_id).await;
            return;
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
                if observed_usage {
                    Some((total_prompt_tokens, total_completion_tokens))
                } else {
                    None
                },
                elapsed_ms,
            )
            .await;

        let visible_final_text = if response_format.is_some() {
            ""
        } else {
            &final_text
        };
        let stream_end = stream_end_body(
            &config,
            &turn_id,
            &chat_id,
            visible_final_text,
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
        if !legacy_default_chat && errored {
            let message = final_error
                .as_deref()
                .unwrap_or("provider turn failed")
                .to_owned();
            let body = chat_error_body_msg(&config, &chat_id, message);
            let _ = out_tx.send(PluginOutgoing::event(body)).await;
        } else if !legacy_default_chat {
            let body = chat_complete_result_body(
                &config,
                &chat_id,
                &final_text,
                &final_tool_calls,
                final_finish_reason.as_deref(),
                observed_usage.then_some((total_prompt_tokens, total_completion_tokens)),
                &active_model,
                &final_reasoning,
                final_reasoning_continuation.as_ref(),
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

    let args_value: Value = match serde_json::from_str(&args_str) {
        Ok(v) => v,
        Err(e) => {
            let err = format!(
                "tool `{name}` arguments are not valid JSON: {e}. Raw arguments: {}",
                snippet(&args_str)
            );
            let _ = out_tx
                .send(PluginOutgoing::event(chat_tool_start_body(
                    &id,
                    &name,
                    &Value::String(args_str),
                )))
                .await;
            let _ = out_tx
                .send(PluginOutgoing::event(chat_tool_end_body(&id, &err, true)))
                .await;
            return ToolStepOutcome::Result {
                id: id.clone(),
                content: err,
            };
        }
    };
    if !args_value.is_object() {
        let err = format!(
            "tool `{name}` arguments must be a JSON object; got {}. Raw arguments: {}",
            json_type_name(&args_value),
            snippet(&args_str)
        );
        let _ = out_tx
            .send(PluginOutgoing::event(chat_tool_start_body(
                &id,
                &name,
                &args_value,
            )))
            .await;
        let _ = out_tx
            .send(PluginOutgoing::event(chat_tool_end_body(&id, &err, true)))
            .await;
        return ToolStepOutcome::Result {
            id: id.clone(),
            content: err,
        };
    }

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

fn valid_tool_call_arguments(call: &ToolCall) -> bool {
    matches!(
        serde_json::from_str::<Value>(&call.function.arguments),
        Ok(Value::Object(_))
    )
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
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

/// Parse a `generic-provider`-shaped message object into our internal
/// `Message`. The wire shape (per generic-provider's docstring) is
/// `{ role, content, tool_calls?, tool_name? }`. We accept that shape
/// and the openai-native variants too. Returns a string error message
/// when the shape is wrong (used by `chat.append` to reply with a
/// `chat.error`).
fn provider_message_reasoning(
    message: &Map<String, Value>,
    provider: &str,
    base_url: &str,
    model: Option<&str>,
) -> Result<Option<ReasoningContinuation>, String> {
    let Some(context) = message.get("provider_context").and_then(Value::as_object) else {
        return Ok(None);
    };
    if context.get("provider").and_then(Value::as_str) != Some(provider)
        || context.get("base_url").and_then(Value::as_str) != Some(base_url)
        || context.get("format").and_then(Value::as_str) != Some(REASONING_CONTEXT_FORMAT)
    {
        return Ok(None);
    }
    let context_model = context.get("model").and_then(Value::as_str);
    if context_model != model {
        return Ok(None);
    }
    let artifact = context
        .get("artifact")
        .and_then(Value::as_object)
        .ok_or_else(|| "compatible provider_context has invalid reasoning artifact".to_owned())?;
    ReasoningContinuation::from_object(artifact)?
        .ok_or_else(|| "compatible provider_context contains no reasoning continuation".to_owned())
        .map(Some)
}

fn parse_provider_message(
    value: Option<&Value>,
    provider: &str,
    base_url: &str,
    model: Option<&str>,
) -> Result<ParsedMessage, String> {
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
        "user" => {
            let text = non_empty_message_content("user", content)?;
            Ok(ParsedMessage {
                message: Message::User { content: text },
                tool_call_failures: Vec::new(),
            })
        }
        "system" => {
            let text = non_empty_message_content("system", content)?;
            Ok(ParsedMessage {
                message: Message::System { content: text },
                tool_call_failures: Vec::new(),
            })
        }
        "assistant" => {
            let mut tool_calls = Vec::new();
            let mut tool_call_failures = Vec::new();
            if let Some(arr) = obj.get("tool_calls").and_then(Value::as_array) {
                for v in arr {
                    match serde_json::from_value::<ToolCall>(v.clone()) {
                        Ok(tc) => {
                            if valid_tool_call_arguments(&tc) {
                                tool_calls.push(tc);
                            } else {
                                tool_call_failures.push(ToolCallParseFailure {
                                    id: Some(tc.id.clone()),
                                    error: "function.arguments is not a JSON object".to_owned(),
                                    raw: Value::String(tc.function.arguments.clone()),
                                });
                            }
                        }
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
            let content = content.filter(|s| !s.trim().is_empty());
            if content.is_none() && tool_calls.is_empty() {
                return Err(
                    "assistant message must have non-empty `content` or `tool_calls`".to_owned(),
                );
            }
            let reasoning = provider_message_reasoning(obj, provider, base_url, model)?;
            Ok(ParsedMessage {
                message: Message::Assistant {
                    content,
                    tool_calls,
                    reasoning,
                },
                tool_call_failures,
            })
        }
        "tool" => {
            let text = required_message_content("tool", content)?;
            let tool_call_id = obj
                .get("tool_call_id")
                .or_else(|| obj.get("tool_name"))
                .and_then(Value::as_str)
                .ok_or_else(|| "tool message missing `tool_call_id`".to_owned())?
                .to_owned();
            Ok(ParsedMessage {
                message: Message::Tool {
                    content: text,
                    tool_call_id,
                },
                tool_call_failures: Vec::new(),
            })
        }
        other => Err(format!("unknown message role `{other}`")),
    }
}

fn non_empty_message_content(role: &str, content: Option<String>) -> Result<String, String> {
    content
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("{role} message `content` must be non-empty"))
}

fn required_message_content(role: &str, content: Option<String>) -> Result<String, String> {
    content.ok_or_else(|| format!("{role} message missing `content`"))
}

fn completion_request_messages(body: &Map<String, Value>, messages: Vec<Message>) -> Vec<Message> {
    let Some(system) = body
        .get("system")
        .and_then(Value::as_str)
        .filter(|system| !system.trim().is_empty())
    else {
        return messages;
    };

    let mut request_messages = Vec::with_capacity(messages.len() + 1);
    request_messages.push(Message::system(system));
    request_messages.extend(messages);
    request_messages
}

fn completion_request_has_model_input(history: &[Message]) -> bool {
    history.iter().any(|message| match message {
        Message::User { content } => !content.trim().is_empty(),
        Message::Tool { .. } => true,
        Message::System { .. } | Message::Assistant { .. } => false,
    })
}

fn request_history_has_model_input(history: &[Message]) -> bool {
    history.iter().any(|message| match message {
        Message::User { content } => !content.trim().is_empty(),
        Message::Tool { .. } => true,
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            content
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
                || !tool_calls.is_empty()
        }
        Message::System { .. } => false,
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

fn retry_progress_body(
    prefix: &str,
    id: &str,
    chat_id: &ChatId,
    progress: RetryProgress,
) -> Map<String, Value> {
    let retry_index = progress.retry_index();
    let max_retries = progress.max_retries();
    let delay_ms = progress.next_delay.as_millis() as u64;
    let code = progress
        .status
        .map(|status| format!("HTTP {status}"))
        .unwrap_or_else(|| "request error".to_owned());
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{prefix}stream.retry")),
    );
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("chat_id".into(), Value::String(chat_id.to_string()));
    if let Some(status) = progress.status {
        m.insert("status".into(), Value::Number(status.into()));
    }
    m.insert("attempt".into(), Value::Number(retry_index.into()));
    m.insert("max_retries".into(), Value::Number(max_retries.into()));
    m.insert("delay_ms".into(), Value::Number(delay_ms.into()));
    m.insert(
        "message".into(),
        Value::String(format!(
            "{code}; retrying {retry_index}/{max_retries} in {}",
            format_retry_delay(progress.next_delay)
        )),
    );
    m
}

fn format_retry_delay(delay: Duration) -> String {
    if delay.as_secs() == 0 {
        format!("{}ms", delay.as_millis())
    } else if delay.subsec_millis() == 0 {
        format!("{}s", delay.as_secs())
    } else {
        format!("{:.1}s", delay.as_secs_f64())
    }
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
        Value::Array(
            models
                .iter()
                .map(|mi| Value::String(mi.id.clone()))
                .collect(),
        ),
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
    let caps: Map<String, Value> = models
        .iter()
        .filter(|mi| !mi.reasoning_efforts.is_empty())
        .map(|mi| {
            let levels = mi
                .reasoning_efforts
                .iter()
                .cloned()
                .map(Value::String)
                .collect::<Vec<_>>();
            let mut reasoning = Map::new();
            reasoning.insert("levels".into(), Value::Array(levels));
            if let Some(default) = &mi.default_reasoning_effort {
                reasoning.insert("default".into(), Value::String(default.clone()));
            }
            let mut entry = Map::new();
            entry.insert("reasoning".into(), Value::Object(reasoning));
            (mi.id.clone(), Value::Object(entry))
        })
        .collect();
    if !caps.is_empty() {
        m.insert("model_capabilities".into(), Value::Object(caps));
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

fn reasoning_set_ack_body(
    config: &Config,
    effort: &str,
    chat_id: Option<&ChatId>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}reasoning.set_ack", config.event_prefix())),
    );
    m.insert("effort".into(), Value::String(effort.to_owned()));
    if let Some(cid) = chat_id {
        m.insert("chat_id".into(), Value::String(cid.to_string()));
    }
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
/// to per-chunk `stream.reasoning_delta` events. Provider-native continuation
/// is carried separately in `provider_context` and never concatenated into
/// this display field.
fn reasoning_provider_context(
    config: &Config,
    model: &str,
    reasoning: Option<&ReasoningContinuation>,
) -> Option<Value> {
    let reasoning = reasoning?;
    Some(serde_json::json!({
        "provider": config.provider_name,
        "base_url": config.base_url,
        "model": model,
        "format": REASONING_CONTEXT_FORMAT,
        "artifact": reasoning.artifact(),
    }))
}

#[allow(clippy::too_many_arguments)]
fn chat_complete_result_body(
    config: &Config,
    chat_id: &ChatId,
    text: &str,
    tool_calls: &[ToolCall],
    finish_reason: Option<&str>,
    token_usage: Option<(u64, u64)>,
    model: &str,
    reasoning: &str,
    reasoning_continuation: Option<&ReasoningContinuation>,
) -> Map<String, Value> {
    let mut output = Map::new();
    output.insert("text".into(), Value::String(text.to_owned()));
    if !reasoning.is_empty() {
        output.insert("reasoning".into(), Value::String(reasoning.to_owned()));
    }
    if let Some(provider_context) =
        reasoning_provider_context(config, model, reasoning_continuation)
    {
        output.insert("provider_context".into(), provider_context);
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
    if let Some((prompt_tokens, completion_tokens)) = token_usage {
        let mut usage = Map::new();
        usage.insert("prompt_tokens".into(), Value::Number(prompt_tokens.into()));
        usage.insert(
            "completion_tokens".into(),
            Value::Number(completion_tokens.into()),
        );
        usage.insert("model".into(), Value::String(model.to_owned()));
        output.insert("usage".into(), Value::Object(usage));
    }

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
/// `generic-provider`'s canonical `ProviderRequest`/`ProviderInput`.
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
/// when targeting the canonical hub). MAG's compile-time routing
/// handles typed resolution. The orchestrator harmonizes between
/// the two specs/implementations.
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
            "into.completion_request_to_raw_request",
        ),
        // openai's wire → canonical (allowed by current spec; bare in,
        // cross-namespace out).
        register_into_entry(
            "RawResponse",
            "generic-provider.ProviderOut",
            "into.raw_response_to_completion_event",
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
/// `examples/nefor-agent/ncp.lua` `handle_event`).
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
        .map_err(|_| LlmError::Transport(TransportError::WriterClosed))
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), LlmError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| LlmError::Transport(TransportError::WriterClosed))
}
