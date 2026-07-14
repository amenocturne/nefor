//! tool-gate — NCP v0.1 plugin: per-tool permission gate.
//!
//! Architecture: tool-gate is a transparent proxy between providers and
//! tool-providing plugins.
//!
//! ```text
//! provider ──tool-gate.tool.invoke──▶ tool-gate ──<source>.tool.invoke──▶ basic-tools
//!                                         │                                    │
//!                                         ▼ (when policy=prompt)               │
//!                                       chat ◀── chat.tool.permission_request──┘
//!                                         │                                    │
//!                                         └── tool.permission_response ──▶ tool-gate
//!                                                                              │
//!                       provider ◀── tool.result ────────────────────── tool-gate ◀── tool.result
//! ```
//!
//! Tool-providing plugins (basic-tools, …) advertise themselves *privately*
//! to the gate via `tool-gate.tools.advertise { tools, source }`. The gate
//! aggregates and re-emits a single public `tool.register { tools }` so
//! providers see only one canonical registry, with the gate as the owner —
//! routing them to `tool-gate.tool.invoke` instead of the underlying plugin.
//!
//! Per-tool policy from CLI flags:
//!
//! - `--auto <name>`   : forward without prompting.
//! - `--prompt <name>` : emit permission request, wait for user.
//! - `--deny <name>`   : reject immediately.
//! - `--default <auto|prompt|deny>` : fallback for unlisted tools (default: prompt).
//!
//! Runtime mode: `tool-gate.set_mode { mode: "safe" | "auto" | "yolo" }`
//! selects the permission profile. Legacy `normal` maps to `safe`. While
//! `yolo`, every tool resolves to `Auto` regardless of per-tool policy —
//! useful for unattended testing. `safe` and `auto` continue to use the
//! configured per-tool policy. The gate broadcasts `tool-gate.mode_changed
//! { mode }` on transitions and also on startup so observers (chat statusline)
//! can render the current mode.
//!
//! Wire id mapping: the provider's outer id is preserved through the
//! permission-request flow (chat sees the same id the provider issued).
//! When forwarding to the underlying plugin we mint a fresh inner id so
//! the underlying plugin's broadcast `tool.result` doesn't collide with
//! the gate's eventual outbound `tool.result` to the provider.

mod policy;

use std::collections::HashMap;

use nefor_plugin_sdk::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, TransportError};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::policy::{Decision, Policy};

const CHANNEL_CAP: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateMode {
    Safe,
    Auto,
    Yolo,
}

impl GateMode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "safe" | "normal" => Some(Self::Safe),
            "auto" => Some(Self::Auto),
            "yolo" => Some(Self::Yolo),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Auto => "auto",
            Self::Yolo => "yolo",
        }
    }
}

const PROTOCOL_VERSION: &str = "0.1";
pub(crate) const PLUGIN_NAME: &str = "tool-gate";
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let policy = parse_args();

    if let Err(e) = run(policy).await {
        tracing::error!(error = %e, "tool-gate exited with error");
        eprintln!("tool-gate: {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
}

fn parse_args() -> Policy {
    use clap::{Arg, ArgAction, Command};
    let m = Command::new("tool-gate")
        .arg(
            Arg::new("auto")
                .long("auto")
                .action(ArgAction::Append)
                .help("Mark <tool> as auto-allow (no prompt)."),
        )
        .arg(
            Arg::new("prompt")
                .long("prompt")
                .action(ArgAction::Append)
                .help("Mark <tool> as prompt-on-call."),
        )
        .arg(
            Arg::new("deny")
                .long("deny")
                .action(ArgAction::Append)
                .help("Mark <tool> as denied (immediate rejection)."),
        )
        .arg(
            Arg::new("default")
                .long("default")
                .default_value("prompt")
                .help("Fallback decision for unlisted tools (auto|prompt|deny)."),
        )
        .get_matches();

    let default_decision = m
        .get_one::<String>("default")
        .expect("default has clap default")
        .parse::<Decision>()
        .expect("validated by clap value parse");
    let mut policy = Policy::new(default_decision);
    for v in m.get_many::<String>("auto").into_iter().flatten() {
        policy.set(v.clone(), Decision::Auto);
    }
    for v in m.get_many::<String>("prompt").into_iter().flatten() {
        policy.set(v.clone(), Decision::Prompt);
    }
    for v in m.get_many::<String>("deny").into_iter().flatten() {
        policy.set(v.clone(), Decision::Deny);
    }
    policy
}

async fn run(policy: Policy) -> Result<(), TransportError> {
    let (out_tx, _writer_handle) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let _reader_handle = spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = await_ready_ok(&mut in_rx).await?;
    tracing::info!(engine_version = %engine_version, "ready");

    send_event(&out_tx, hello_body()).await?;

    let mut state = GateState::new(policy);
    // Announce the initial mode so any UI that comes up after us still sees it.
    send_event(&out_tx, mode_changed_body(state.mode)).await?;
    run_dispatch_loop(&out_tx, &mut in_rx, &mut state).await?;

    let _ = out_tx.send(PluginOutgoing::event(goodbye_body())).await;
    Ok(())
}

/// One advertised tool. Mirrors the wire shape — name + description +
/// JSON Schema parameters — without depending on a provider catalog crate.
#[derive(Debug, Clone)]
struct ToolSpec {
    name: String,
    description: String,
    parameters: Value,
    display: Value,
}

/// Pending forwarded invocation: maps the gate-minted inner id (used to
/// address the underlying plugin) back to the provider's outer id (so when
/// `tool.result` arrives we can rewrite the id and broadcast it) plus the
/// source plugin the call was forwarded to (so a `tool.cancel` for the outer
/// id can be forwarded on to the same source under the inner id).
#[derive(Debug, Clone)]
struct PendingForward {
    outer_id: String,
    source: String,
}

/// Pending permission request: maps the provider's outer id to the
/// invocation context, so when the user approves we can synthesize the
/// inner forward.
#[derive(Debug, Clone)]
struct PendingApproval {
    outer_id: String,
    source: String,
    name: String,
    args: Value,
}

struct GateState {
    /// Per-source advertised tools. Key: source plugin name.
    advertised: HashMap<String, Vec<ToolSpec>>,
    /// Reverse lookup: tool name → source plugin name. Rebuilt from
    /// `advertised` whenever it changes.
    tool_owner: HashMap<String, String>,
    /// Active forwards keyed by gate-minted inner id.
    pending: HashMap<String, PendingForward>,
    /// Active permission requests keyed by provider's outer id.
    awaiting_approval: HashMap<String, PendingApproval>,
    /// Monotonic counter for inner-id minting.
    inner_id_counter: u64,
    policy: Policy,
    /// Runtime permission mode. `safe` and `auto` use per-tool policy;
    /// `yolo` resolves every tool to `Auto`.
    mode: GateMode,
}

impl GateState {
    fn new(policy: Policy) -> Self {
        Self {
            advertised: HashMap::new(),
            tool_owner: HashMap::new(),
            pending: HashMap::new(),
            awaiting_approval: HashMap::new(),
            inner_id_counter: 0,
            policy,
            mode: GateMode::Safe,
        }
    }

    fn rebuild_owner_map(&mut self) {
        self.tool_owner.clear();
        for (source, tools) in &self.advertised {
            for t in tools {
                self.tool_owner.insert(t.name.clone(), source.clone());
            }
        }
    }

    fn next_inner_id(&mut self) -> String {
        self.inner_id_counter += 1;
        format!("gate-{}", self.inner_id_counter)
    }
}

async fn run_dispatch_loop(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    in_rx: &mut mpsc::Receiver<Result<Envelope, TransportError>>,
    state: &mut GateState,
) -> Result<(), TransportError> {
    loop {
        tokio::select! {
            maybe = in_rx.recv() => {
                match maybe {
                    Some(Ok(env)) => match &env.body {
                        Body::System(SystemBody::Shutdown { .. }) => {
                            tracing::info!("shutdown received");
                            return Ok(());
                        }
                        Body::System(_) => {
                            tracing::warn!(?env, "unexpected system envelope after handshake");
                        }
                        Body::Event(map) => {
                            dispatch_event(out_tx, map, state).await?;
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
                return Ok(());
            }
        }
    }
}

async fn dispatch_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    state: &mut GateState,
) -> Result<(), TransportError> {
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(k) => k,
        None => return Ok(()),
    };
    let advertise_kind = format!("{PLUGIN_NAME}.tools.advertise");
    let invoke_kind = format!("{PLUGIN_NAME}.tool.invoke");
    let cancel_kind = format!("{PLUGIN_NAME}.tool.cancel");
    let set_mode_kind = format!("{PLUGIN_NAME}.set_mode");

    if kind == advertise_kind {
        handle_tools_advertise(out_tx, body, state).await?;
    } else if kind == invoke_kind {
        handle_tool_invoke(out_tx, body, state).await?;
    } else if kind == cancel_kind {
        handle_tool_cancel(out_tx, body, state).await?;
    } else if kind == "tool.result" {
        handle_tool_result(out_tx, body, state).await?;
    } else if kind == "tool.permission_response" {
        handle_permission_response(out_tx, body, state).await?;
    } else if kind == set_mode_kind {
        handle_set_mode(out_tx, body, state).await?;
    }
    Ok(())
}

async fn handle_set_mode(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    state: &mut GateState,
) -> Result<(), TransportError> {
    let mode = match body.get("mode").and_then(Value::as_str) {
        Some(s) => s,
        None => {
            tracing::warn!("tool-gate.set_mode missing string field `mode`; dropping");
            return Ok(());
        }
    };
    let Some(target) = GateMode::parse(mode) else {
        tracing::warn!(mode = %mode, "tool-gate.set_mode: unknown mode; expected safe|auto|yolo");
        return Ok(());
    };
    if state.mode == target {
        // No-op: still re-broadcast so a late observer (newly-spawned chat)
        // sees the current mode.
        send_event(out_tx, mode_changed_body(state.mode)).await?;
        return Ok(());
    }
    state.mode = target;
    tracing::info!(mode = state.mode.as_str(), "mode changed");
    send_event(out_tx, mode_changed_body(state.mode)).await?;
    Ok(())
}

fn validate_display_contract(display: &Value) -> Result<(), String> {
    let object = display
        .as_object()
        .ok_or_else(|| "display must be an object".to_owned())?;
    for key in object.keys() {
        if !matches!(key.as_str(), "label" | "primary" | "arguments" | "result") {
            return Err("display has unknown field".to_owned());
        }
    }
    let valid_selector = |value: &Value| {
        value.as_str().is_some_and(|s| !s.is_empty())
            || value.as_object().is_some_and(|o| {
                o.len() == 1
                    && o.get("arg")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty())
            })
    };
    if !object.get("label").is_some_and(valid_selector) {
        return Err("display.label must be text or {arg}".into());
    }
    if object.get("primary").is_some_and(|value| {
        !value.as_object().is_some_and(|object| {
            object.len() == 1
                && object
                    .get("arg")
                    .and_then(Value::as_str)
                    .is_some_and(|arg| !arg.is_empty())
        })
    }) {
        return Err("display.primary must be {arg}".into());
    }
    if let Some(arguments) = object.get("arguments") {
        let fields = arguments
            .as_array()
            .ok_or_else(|| "display.arguments must be an array".to_owned())?;
        for field in fields {
            let field = field
                .as_object()
                .ok_or_else(|| "display argument must be an object".to_owned())?;
            if field.len() != 2
                || field
                    .get("label")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                || field
                    .get("arg")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                return Err(
                    "display argument must contain only non-empty label and arg strings".into(),
                );
            }
        }
    }
    let result = object
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| "display.result must be an object".to_owned())?;
    for key in result.keys() {
        if !matches!(key.as_str(), "kind" | "text") {
            return Err("display.result has unknown field".into());
        }
    }
    match result.get("kind").and_then(Value::as_str) {
        Some("content") if !result.contains_key("text") => Ok(()),
        Some("receipt")
            if result
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty()) =>
        {
            Ok(())
        }
        _ => Err("display.result must be content or a receipt with text".into()),
    }
}

fn validate_unique_tool_names(
    source: &str,
    tools: &[ToolSpec],
    state: &GateState,
) -> Result<(), String> {
    let mut names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    for pair in names.windows(2) {
        if pair[0] == pair[1] {
            return Err(format!(
                "duplicate tool name `{}` advertised more than once by source `{source}`",
                pair[0]
            ));
        }
    }

    let mut collisions = Vec::new();
    for (owner, advertised) in &state.advertised {
        if owner == source {
            continue;
        }
        for tool in advertised {
            if names.binary_search(&tool.name.as_str()).is_ok() {
                collisions.push((tool.name.as_str(), owner.as_str()));
            }
        }
    }
    collisions.sort_unstable();
    if let Some((name, owner)) = collisions.first() {
        return Err(format!(
            "duplicate tool name `{name}` advertised by sources `{owner}` and `{source}`"
        ));
    }
    Ok(())
}
async fn handle_tools_advertise(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    state: &mut GateState,
) -> Result<(), TransportError> {
    let source = match body.get("source").and_then(Value::as_str) {
        Some(source) if !source.trim().is_empty() => source.to_owned(),
        _ => {
            let error = "source must be a non-empty string";
            tracing::error!(%error, "rejecting malformed tools advertisement");
            send_event(out_tx, advertise_error_body("", error)).await?;
            return Ok(());
        }
    };
    let tools_arr = match body.get("tools").and_then(Value::as_array) {
        Some(a) => a,
        None => {
            tracing::warn!(source = %source, "tools.advertise missing array `tools`; dropping");
            return Ok(());
        }
    };
    let tools: Result<Vec<ToolSpec>, String> = tools_arr
        .iter()
        .map(|t| -> Result<ToolSpec, String> {
            let name = t
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| "tool name must be a non-empty string".to_owned())?
                .to_owned();
            let description = t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let parameters = t
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            let display = t
                .get("display")
                .ok_or_else(|| "tool missing display contract: ".to_owned() + &name)?
                .clone();
            validate_display_contract(&display)
                .map_err(|e| "invalid display for ".to_owned() + &name + ": " + &e)?;
            Ok(ToolSpec {
                name,
                description,
                parameters,
                display,
            })
        })
        .collect::<Result<_, _>>();
    let tools = match tools {
        Ok(tools) => tools,
        Err(error) => {
            tracing::error!(source = %source, %error, "rejecting malformed tools advertisement");
            send_event(out_tx, advertise_error_body(&source, &error)).await?;
            return Ok(());
        }
    };

    if let Err(error) = validate_unique_tool_names(&source, &tools, state) {
        tracing::error!(source = %source, %error, "rejecting duplicate tools advertisement");
        send_event(out_tx, advertise_error_body(&source, &error)).await?;
        return Ok(());
    }

    tracing::info!(source = %source, count = tools.len(), "tools.advertise");
    if tools.is_empty() {
        state.advertised.remove(&source);
    } else {
        state.advertised.insert(source.clone(), tools);
    }
    state.rebuild_owner_map();

    // Re-emit the public registry. Providers key catalogs by `from`, so
    // every advertise rebuilds and broadcasts — they replace `tool-gate`'s
    // entry wholesale.
    send_event(out_tx, tool_register_body(state)).await?;
    Ok(())
}

async fn handle_tool_invoke(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    state: &mut GateState,
) -> Result<(), TransportError> {
    let outer_id = match body.get("id").and_then(Value::as_str) {
        Some(s) => s.to_owned(),
        None => {
            tracing::warn!("tool-gate.tool.invoke missing required string field `id`; dropping");
            return Ok(());
        }
    };
    let name = match body.get("name").and_then(Value::as_str) {
        Some(s) => s.to_owned(),
        None => {
            send_event(
                out_tx,
                tool_result_error_body(
                    &outer_id,
                    "tool-gate.tool.invoke missing required string field `name`",
                ),
            )
            .await?;
            return Ok(());
        }
    };
    let args = body
        .get("args")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let read_only = body
        .get("read_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let source = match state.tool_owner.get(&name).cloned() {
        Some(s) => s,
        None => {
            send_event(
                out_tx,
                tool_result_error_body(&outer_id, &format!("unknown tool `{name}`")),
            )
            .await?;
            return Ok(());
        }
    };

    let decision = if state.mode == GateMode::Yolo {
        Decision::Auto
    } else {
        state.policy.decide(&name)
    };
    match decision {
        Decision::Auto => {
            forward_to_source(out_tx, state, &outer_id, &source, &name, args).await?;
        }
        Decision::Prompt => {
            state.awaiting_approval.insert(
                outer_id.clone(),
                PendingApproval {
                    outer_id: outer_id.clone(),
                    source,
                    name: name.clone(),
                    args: args.clone(),
                },
            );
            send_event(
                out_tx,
                permission_request_body(&outer_id, &name, &args, read_only),
            )
            .await?;
        }
        Decision::Deny => {
            send_event(
                out_tx,
                tool_result_error_body(&outer_id, &format!("tool `{name}` denied by gate policy")),
            )
            .await?;
        }
    }
    Ok(())
}

async fn forward_to_source(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    state: &mut GateState,
    outer_id: &str,
    source: &str,
    name: &str,
    args: Value,
) -> Result<(), TransportError> {
    let inner_id = state.next_inner_id();
    state.pending.insert(
        inner_id.clone(),
        PendingForward {
            outer_id: outer_id.to_owned(),
            source: source.to_owned(),
        },
    );
    send_event(out_tx, forward_invoke_body(source, &inner_id, name, args)).await?;
    Ok(())
}

/// Forward a `tool.cancel` for a caller's outer id onto the underlying source.
/// The kernel's interrupt emits `tool-gate.tool.cancel { id: <outer> }` for
/// each open tool correlation; we look up the live forward by its outer id,
/// then re-emit `<source>.tool.cancel { id: <inner> }` so the owning plugin
/// (basic-tools kills the child; lead-workflow cancels a mag-eval sub-run)
/// aborts exactly that firing. The `pending` entry is KEPT: the real (now
/// cancelled) `tool.result` still flows back through the normal path and drops
/// at the caller's already-settled correlation.
async fn handle_tool_cancel(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    state: &GateState,
) -> Result<(), TransportError> {
    let outer_id = match body.get("id").and_then(Value::as_str) {
        Some(s) => s,
        None => return Ok(()),
    };
    // Reverse-scan the (small) live-forward table for the matching outer id.
    let forward = state
        .pending
        .iter()
        .find(|(_, p)| p.outer_id == outer_id)
        .map(|(inner, p)| (inner.clone(), p.source.clone()));
    match forward {
        Some((inner_id, source)) => {
            tracing::info!(outer_id = %outer_id, inner_id = %inner_id, source = %source, "tool.cancel forwarded");
            send_event(out_tx, forward_cancel_body(&source, &inner_id)).await?;
        }
        None => {
            tracing::info!(outer_id = %outer_id, "tool.cancel for an unknown/settled forward; no-op");
        }
    }
    Ok(())
}

async fn handle_tool_result(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    state: &mut GateState,
) -> Result<(), TransportError> {
    let inner_id = match body.get("id").and_then(Value::as_str) {
        Some(s) => s,
        None => return Ok(()),
    };
    // Match by inner id. tool.result is broadcast to all plugins; if it's
    // not in our `pending` map it belongs to a different caller (or a
    // result we already forwarded) — drop silently.
    let Some(pending) = state.pending.remove(inner_id) else {
        return Ok(());
    };
    let mut out = Map::new();
    out.insert("kind".into(), Value::String("tool.result".into()));
    out.insert("id".into(), Value::String(pending.outer_id));
    if let Some(output) = body.get("output") {
        out.insert("output".into(), output.clone());
    }
    if let Some(err) = body.get("error") {
        out.insert("error".into(), err.clone());
    }
    send_event(out_tx, out).await?;
    Ok(())
}

async fn handle_permission_response(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    state: &mut GateState,
) -> Result<(), TransportError> {
    let outer_id = match body.get("id").and_then(Value::as_str) {
        Some(s) => s.to_owned(),
        None => return Ok(()),
    };
    let decision = body
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("deny");

    let Some(approval) = state.awaiting_approval.remove(&outer_id) else {
        // No matching pending request — likely a stale response or one
        // belonging to a different gate. Drop silently.
        return Ok(());
    };

    if decision == "approve" {
        let args = body.get("args").cloned().unwrap_or(approval.args);
        forward_to_source(
            out_tx,
            state,
            &approval.outer_id,
            &approval.source,
            &approval.name,
            args,
        )
        .await?;
    } else {
        // Optional `reason` carries auto-deny context from a non-user
        // approver (e.g. tool-validator pre-rejecting a dispatch-graph
        // call that lacks an approved plan). When present we surface it
        // verbatim in the tool.result.error so the agent learns why
        // and what to do next, instead of getting the generic
        // "denied by user" string and having to guess.
        let reason = body.get("reason").and_then(Value::as_str);
        let error_msg = match reason {
            Some(r) if !r.is_empty() => format!("tool `{}` denied: {r}", approval.name),
            _ => format!("tool `{}` denied by user", approval.name),
        };
        send_event(
            out_tx,
            tool_result_error_body(&approval.outer_id, &error_msg),
        )
        .await?;
    }
    Ok(())
}

// ---- body constructors -----------------------------------------------------

fn hello_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(format!("{PLUGIN_NAME}.hello")));
    m.insert("version".into(), Value::String(PLUGIN_VERSION.into()));
    m
}

fn tool_register_body(state: &GateState) -> Map<String, Value> {
    let mut specs = state.advertised.values().flatten().collect::<Vec<_>>();
    specs.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    let mut tools: Vec<Value> = Vec::new();
    for ts in specs {
        let mut m = Map::new();
        m.insert("name".into(), Value::String(ts.name.clone()));
        m.insert("description".into(), Value::String(ts.description.clone()));
        m.insert("parameters".into(), ts.parameters.clone());
        m.insert("display".into(), ts.display.clone());
        tools.push(Value::Object(m));
    }
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("tool.register".into()));
    m.insert("tools".into(), Value::Array(tools));
    m
}

fn forward_invoke_body(
    source: &str,
    inner_id: &str,
    name: &str,
    args: Value,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{source}.tool.invoke")),
    );
    m.insert("id".into(), Value::String(inner_id.to_owned()));
    m.insert("name".into(), Value::String(name.to_owned()));
    m.insert("args".into(), args);
    m
}

fn forward_cancel_body(source: &str, inner_id: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{source}.tool.cancel")),
    );
    m.insert("id".into(), Value::String(inner_id.to_owned()));
    m
}

fn permission_request_body(
    id: &str,
    name: &str,
    args: &Value,
    read_only: bool,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String("chat.tool.permission_request".into()),
    );
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("tool".into(), Value::String(name.to_owned()));
    m.insert("args".into(), args.clone());
    if read_only {
        m.insert("read_only".into(), Value::Bool(true));
    }
    m
}

fn mode_changed_body(mode: GateMode) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{PLUGIN_NAME}.mode_changed")),
    );
    m.insert("mode".into(), Value::String(mode.as_str().into()));
    m
}

fn advertise_error_body(source: &str, message: &str) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert(
        "kind".into(),
        Value::String(format!("{PLUGIN_NAME}.advertise_error")),
    );
    body.insert("source".into(), Value::String(source.to_owned()));
    body.insert("message".into(), Value::String(message.to_owned()));
    body
}
fn tool_result_error_body(id: &str, message: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("tool.result".into()));
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("error".into(), Value::String(message.to_owned()));
    m
}

fn goodbye_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{PLUGIN_NAME}.goodbye")),
    );
    m.insert("reason".into(), Value::String("stream closed".into()));
    m
}

async fn send_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: Map<String, Value>,
) -> Result<(), TransportError> {
    out_tx
        .send(PluginOutgoing::event(body))
        .await
        .map_err(|_| TransportError::WriterClosed)
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), TransportError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| TransportError::WriterClosed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_state() -> GateState {
        let mut policy = Policy::new(Decision::Prompt);
        policy.set("read_file", Decision::Prompt);
        policy.set("write_file", Decision::Auto);
        GateState::new(policy)
    }

    fn advertise_body(source: &str, mut tools: Value) -> Map<String, Value> {
        if let Some(items) = tools.as_array_mut() {
            for item in items {
                if item.get("display").is_none() {
                    item.as_object_mut().expect("tool object").insert(
                        "display".into(),
                        json!({"label": "Test tool", "result": {"kind": "content"}}),
                    );
                }
            }
        }
        json!({
            "kind": "tool-gate.tools.advertise",
            "source": source,
            "tools": tools,
        })
        .as_object()
        .expect("obj")
        .clone()
    }

    #[test]
    fn validates_display_contract() {
        assert!(validate_display_contract(&json!({"label": "Read"})).is_err());
        assert!(validate_display_contract(
            &json!({"label": "Read", "result": {"kind": "receipt", "text": "loaded"}})
        )
        .is_ok());
    }

    #[test]
    fn shared_display_contract_fixtures_match_rust_validator() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/tool_display_contracts.json"
        ))
        .expect("display fixtures JSON");
        for fixture in fixtures.as_array().expect("fixture array") {
            let expected = fixture["valid"].as_bool().expect("valid bool");
            let actual = validate_display_contract(&fixture["contract"]).is_ok();
            assert_eq!(actual, expected, "fixture {}", fixture["name"]);
        }
    }

    #[test]
    fn display_contract_wire_shape_is_strict() {
        let accepted = [
            json!({"label": "Read", "primary": {"arg": "path"}, "result": {"kind": "content"}}),
            json!({"label": {"arg": "action"}, "arguments": [{"label": "in", "arg": "path"}], "result": {"kind": "receipt", "text": "done"}}),
        ];
        for contract in accepted {
            assert!(validate_display_contract(&contract).is_ok(), "{contract}");
        }

        let rejected = [
            json!({"label": "Read", "primary": "literal", "result": {"kind": "content"}}),
            json!({"label": {"arg": ""}, "result": {"kind": "content"}}),
            json!({"label": {"arg": "path", "unknown": true}, "result": {"kind": "content"}}),
            json!({"label": "Read", "primary": {"arg": 3}, "result": {"kind": "content"}}),
            json!({"label": "Read", "arguments": [{"label": "in"}], "result": {"kind": "content"}}),
            json!({"label": "Read", "result": {"kind": "content", "text": "no"}}),
            json!({"label": "Read", "result": {"kind": "receipt"}}),
            json!({"label": "Read", "result": {"kind": "other"}}),
        ];
        for contract in rejected {
            assert!(validate_display_contract(&contract).is_err(), "{contract}");
        }
    }
    #[tokio::test]
    async fn rejects_empty_and_whitespace_only_sources_with_advertise_error() {
        for source in ["", " 	"] {
            let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
            let mut state = make_state();
            let body = advertise_body(source, json!([{"name": "read_file"}]));

            handle_tools_advertise(&tx, &body, &mut state)
                .await
                .unwrap();

            let event: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
            assert_eq!(event["body"]["kind"], "tool-gate.advertise_error");
            assert_eq!(event["body"]["source"], "");
            assert_eq!(
                event["body"]["message"],
                "source must be a non-empty string"
            );
            assert!(state.advertised.is_empty());
            assert!(state.tool_owner.is_empty());
        }
    }

    #[tokio::test]
    async fn rejects_empty_and_whitespace_only_tool_names_with_advertise_error() {
        for name in ["", " 	"] {
            let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
            let mut state = make_state();
            let body = advertise_body("basic-tools", json!([{"name": name}]));

            handle_tools_advertise(&tx, &body, &mut state)
                .await
                .unwrap();

            let event: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
            assert_eq!(event["body"]["kind"], "tool-gate.advertise_error");
            assert_eq!(event["body"]["source"], "basic-tools");
            assert_eq!(
                event["body"]["message"],
                "tool name must be a non-empty string"
            );
            assert!(state.advertised.is_empty());
            assert!(state.tool_owner.is_empty());
        }
    }

    #[tokio::test]
    async fn invalid_readvertisement_preserves_prior_valid_state_atomically() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let valid = advertise_body("basic-tools", json!([{"name": "read_file"}]));
        handle_tools_advertise(&tx, &valid, &mut state)
            .await
            .unwrap();
        let _register = rx.recv().await.unwrap();

        let invalid = advertise_body(
            "basic-tools",
            json!([
                {"name": "write_file"},
                {"name": ""}
            ]),
        );
        handle_tools_advertise(&tx, &invalid, &mut state)
            .await
            .unwrap();

        let event: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(event["body"]["kind"], "tool-gate.advertise_error");
        assert_eq!(state.advertised["basic-tools"].len(), 1);
        assert_eq!(state.advertised["basic-tools"][0].name, "read_file");
        assert_eq!(
            state.tool_owner.get("read_file").map(String::as_str),
            Some("basic-tools")
        );
        assert!(!state.tool_owner.contains_key("write_file"));
    }

    #[tokio::test]
    async fn advertise_rebuilds_owner_map_and_emits_register() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let body = advertise_body(
            "basic-tools",
            json!([
                {"name": "read_file", "description": "Read a file.", "parameters": {}}
            ]),
        );
        handle_tools_advertise(&tx, &body, &mut state)
            .await
            .unwrap();
        assert_eq!(
            state.tool_owner.get("read_file"),
            Some(&"basic-tools".into())
        );

        let msg = rx.recv().await.expect("got register");
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("tool.register")
        );
        let arr = body.get("tools").and_then(Value::as_array).unwrap();
        assert_eq!(arr.len(), 1);
    }

    #[tokio::test]
    async fn duplicate_tool_names_are_rejected_without_mutating_registry() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let first = advertise_body(
            "basic-tools",
            json!([{"name": "search_text", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &first, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap();
        let duplicate = advertise_body(
            "read-only-tools",
            json!([{"name": "search_text", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &duplicate, &mut state)
            .await
            .unwrap();
        let event: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(event["body"]["kind"], "tool-gate.advertise_error");
        assert_eq!(event["body"]["message"], "duplicate tool name `search_text` advertised by sources `basic-tools` and `read-only-tools`");
        assert_eq!(
            state.tool_owner.get("search_text").map(String::as_str),
            Some("basic-tools")
        );
        assert!(!state.advertised.contains_key("read-only-tools"));
    }

    #[tokio::test]
    async fn source_readvertisement_replaces_inventory_and_replays_full_register() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let first = advertise_body(
            "basic-tools",
            json!([{"name": "write_file", "parameters": {}}, {"name": "read_file", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &first, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap();
        let replacement = advertise_body(
            "basic-tools",
            json!([{"name": "search_text", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &replacement, &mut state)
            .await
            .unwrap();
        let event: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        let names = event["body"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["search_text"]);
        assert!(!state.tool_owner.contains_key("read_file"));
        assert!(!state.tool_owner.contains_key("write_file"));
    }
    #[tokio::test]
    async fn invoke_with_prompt_emits_permission_request() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        // Pre-populate so tool_owner is set up.
        let body = advertise_body(
            "basic-tools",
            json!([{"name": "read_file", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &body, &mut state)
            .await
            .unwrap();
        let _register = rx.recv().await.unwrap();

        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-1",
            "name": "read_file",
            "args": {"path": "/etc/hosts"}
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();

        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("chat.tool.permission_request")
        );
        assert_eq!(body.get("id").and_then(Value::as_str), Some("prov-1"));
        assert_eq!(body.get("tool").and_then(Value::as_str), Some("read_file"));
        assert!(state.awaiting_approval.contains_key("prov-1"));
    }

    #[tokio::test]
    async fn invoke_with_auto_forwards_immediately() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let body = advertise_body(
            "basic-tools",
            json!([{"name": "write_file", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &body, &mut state)
            .await
            .unwrap();
        let _register = rx.recv().await.unwrap();

        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-2",
            "name": "write_file",
            "args": {}
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();

        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("basic-tools.tool.invoke")
        );
        // inner id is freshly minted, not the outer id
        let inner_id = body.get("id").and_then(Value::as_str).unwrap();
        assert!(inner_id.starts_with("gate-"));
        assert_eq!(
            state.pending.get(inner_id).map(|p| p.outer_id.as_str()),
            Some("prov-2")
        );
    }

    #[tokio::test]
    async fn invoke_unknown_tool_replies_with_error() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-3",
            "name": "no_such_tool",
            "args": {}
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("tool.result")
        );
        assert_eq!(body.get("id").and_then(Value::as_str), Some("prov-3"));
        let err = body.get("error").and_then(Value::as_str).unwrap();
        assert!(err.contains("unknown tool"));
    }

    #[tokio::test]
    async fn approval_forwards_pending_invocation() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let body = advertise_body(
            "basic-tools",
            json!([{"name": "read_file", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &body, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await;

        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-7",
            "name": "read_file",
            "args": {"path": "/x"}
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();
        let _request = rx.recv().await.unwrap();

        let response = json!({
            "kind": "tool.permission_response",
            "id": "prov-7",
            "decision": "approve"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_permission_response(&tx, &response, &mut state)
            .await
            .unwrap();

        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("basic-tools.tool.invoke")
        );
        assert!(!state.awaiting_approval.contains_key("prov-7"));
    }

    #[tokio::test]
    async fn approval_can_override_forwarded_args() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let body = advertise_body(
            "basic-tools",
            json!([{"name": "edit_file", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &body, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await;

        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-8",
            "name": "edit_file",
            "args": {"path": "/x", "old_string": "a", "new_string": "b"}
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();
        let _request = rx.recv().await.unwrap();

        let response = json!({
            "kind": "tool.permission_response",
            "id": "prov-8",
            "decision": "approve",
            "args": {
                "path": "/x",
                "old_string": "a",
                "new_string": "b",
                "policy": {"require_unique_match": false}
            }
        })
        .as_object()
        .unwrap()
        .clone();
        handle_permission_response(&tx, &response, &mut state)
            .await
            .unwrap();

        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let args = v
            .get("body")
            .and_then(|b| b.get("args"))
            .expect("forwarded args");
        assert_eq!(args["policy"]["require_unique_match"], json!(false));
    }

    #[tokio::test]
    async fn denial_emits_tool_result_error_with_outer_id() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let body = advertise_body(
            "basic-tools",
            json!([{"name": "read_file", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &body, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await;

        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-9",
            "name": "read_file",
            "args": {}
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();
        let _ = rx.recv().await;

        let response = json!({
            "kind": "tool.permission_response",
            "id": "prov-9",
            "decision": "deny"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_permission_response(&tx, &response, &mut state)
            .await
            .unwrap();

        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("tool.result")
        );
        assert_eq!(body.get("id").and_then(Value::as_str), Some("prov-9"));
        let err = body.get("error").and_then(Value::as_str).unwrap();
        assert!(err.contains("denied by user"));
    }

    #[tokio::test]
    async fn denial_with_reason_surfaces_reason_in_tool_result_error() {
        // tool-validator emits permission_response{decision="deny",
        // reason="..."} when it auto-rejects an invocation (e.g. a
        // dispatch-graph call without an approved plan). The reason
        // must reach the agent verbatim — that's the whole point of
        // the auto-deny: tell the agent what to do next instead of
        // letting it discover the rejection only by re-asking the
        // user via popup.
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let advertise = advertise_body(
            "basic-tools",
            json!([{"name": "dispatch-graph", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await;

        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-11",
            "name": "dispatch-graph",
            "args": {}
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();
        let _ = rx.recv().await;

        let response = json!({
            "kind": "tool.permission_response",
            "id": "prov-11",
            "decision": "deny",
            "reason": "writer roles need an approved plan, but no plan was submitted yet"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_permission_response(&tx, &response, &mut state)
            .await
            .unwrap();

        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        let err = body.get("error").and_then(Value::as_str).unwrap();
        assert!(
            err.contains("writer roles need an approved plan"),
            "tool.result.error must surface the validator's reason verbatim, got: {err}"
        );
        assert!(
            !err.contains("denied by user"),
            "deny-with-reason must not fall through to the generic 'denied by user' message, got: {err}"
        );
    }

    #[tokio::test]
    async fn tool_result_inner_id_is_rewritten_to_outer() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        // Set up an active forward.
        state.pending.insert(
            "gate-1".into(),
            PendingForward {
                outer_id: "prov-42".into(),
                source: "basic-tools".into(),
            },
        );
        let body = json!({
            "kind": "tool.result",
            "id": "gate-1",
            "output": "abc"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_result(&tx, &body, &mut state).await.unwrap();
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(body.get("id").and_then(Value::as_str), Some("prov-42"));
        assert_eq!(body.get("output").and_then(Value::as_str), Some("abc"));
        assert!(!state.pending.contains_key("gate-1"));
    }

    #[tokio::test]
    async fn tool_cancel_forwards_to_source_under_the_inner_id() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let advertise = advertise_body(
            "basic-tools",
            json!([{"name": "bash", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await; // tool.register

        // Establish a live forward: bash auto-forwards under yolo.
        state.mode = GateMode::Yolo;
        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "r16/cap-2",
            "name": "bash",
            "args": { "command": "sleep 10" }
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();
        let fwd = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&fwd.to_line()).unwrap();
        let inner_id = v
            .get("body")
            .and_then(|b| b.get("id"))
            .and_then(Value::as_str)
            .unwrap()
            .to_owned();
        assert!(inner_id.starts_with("gate-"));

        // Cancel by the OUTER (kernel correlation) id.
        let cancel = json!({
            "kind": "tool-gate.tool.cancel",
            "id": "r16/cap-2"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_cancel(&tx, &cancel, &state).await.unwrap();

        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("basic-tools.tool.cancel"),
            "cancel is forwarded to the owning source"
        );
        assert_eq!(
            body.get("id").and_then(Value::as_str),
            Some(inner_id.as_str()),
            "forwarded under the gate's inner id, matching the forwarded invoke"
        );
        // The forward is kept so the real (cancelled) result still correlates.
        assert!(state.pending.contains_key(&inner_id));
    }

    #[tokio::test]
    async fn tool_cancel_for_unknown_outer_id_is_a_noop() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let state = make_state();
        let cancel = json!({ "kind": "tool-gate.tool.cancel", "id": "nope" })
            .as_object()
            .unwrap()
            .clone();
        handle_tool_cancel(&tx, &cancel, &state).await.unwrap();
        drop(tx);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn tool_result_for_unknown_id_is_dropped() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let body = json!({
            "kind": "tool.result",
            "id": "not-ours",
            "output": "x"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_result(&tx, &body, &mut state).await.unwrap();
        drop(tx);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn yolo_forces_auto_regardless_of_policy() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        // Default `prompt` policy + a per-tool `deny` to prove yolo wins.
        let mut policy = Policy::new(Decision::Prompt);
        policy.set("bash", Decision::Deny);
        let mut state = GateState::new(policy);
        state.mode = GateMode::Yolo;
        let advertise = advertise_body(
            "basic-tools",
            json!([
                {"name": "read_file", "description": "", "parameters": {}},
                {"name": "bash", "description": "", "parameters": {}},
            ]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await; // tool.register

        for (id, name) in [("y-1", "read_file"), ("y-2", "bash")] {
            let invoke = json!({
                "kind": "tool-gate.tool.invoke",
                "id": id,
                "name": name,
                "args": {}
            })
            .as_object()
            .unwrap()
            .clone();
            handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();
            let msg = rx.recv().await.unwrap();
            let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
            let body = v.get("body").unwrap();
            // Forwarded as `<source>.tool.invoke`, not a permission_request,
            // not a tool.result error.
            assert_eq!(
                body.get("kind").and_then(Value::as_str),
                Some("basic-tools.tool.invoke"),
                "yolo should auto-forward {name}"
            );
        }
        assert!(state.awaiting_approval.is_empty());
    }

    #[tokio::test]
    async fn set_mode_yolo_emits_mode_changed_and_flips_state() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let body = json!({
            "kind": "tool-gate.set_mode",
            "mode": "yolo"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_set_mode(&tx, &body, &mut state).await.unwrap();
        assert_eq!(state.mode, GateMode::Yolo);
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("tool-gate.mode_changed")
        );
        assert_eq!(body.get("mode").and_then(Value::as_str), Some("yolo"));
    }

    #[tokio::test]
    async fn set_mode_normal_clears_yolo() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        state.mode = GateMode::Yolo;
        let body = json!({
            "kind": "tool-gate.set_mode",
            "mode": "normal"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_set_mode(&tx, &body, &mut state).await.unwrap();
        assert_eq!(state.mode, GateMode::Safe);
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(body.get("mode").and_then(Value::as_str), Some("safe"));
    }

    #[tokio::test]
    async fn set_mode_auto_emits_mode_changed_and_uses_policy() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let body = json!({
            "kind": "tool-gate.set_mode",
            "mode": "auto"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_set_mode(&tx, &body, &mut state).await.unwrap();
        assert_eq!(state.mode, GateMode::Auto);
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(body.get("mode").and_then(Value::as_str), Some("auto"));

        let advertise = advertise_body(
            "basic-tools",
            json!([{"name": "read_file", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap();
        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-read",
            "name": "read_file",
            "args": {}
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("chat.tool.permission_request"),
            "auto mode still uses per-tool policy"
        );
    }

    #[tokio::test]
    async fn set_mode_unknown_value_is_dropped() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let body = json!({
            "kind": "tool-gate.set_mode",
            "mode": "wat"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_set_mode(&tx, &body, &mut state).await.unwrap();
        assert_eq!(state.mode, GateMode::Safe);
        drop(tx);
        assert!(rx.recv().await.is_none());
    }

    /// Regression: two sequential `bash` invokes with different `args` must
    /// EACH emit `chat.tool.permission_request`. The gate's policy is
    /// per-tool-name only (no per-arg cache, no first-call-establishes-default
    /// fast-path) — both calls under `Decision::Prompt` must prompt the user.
    /// Pins the invariant against any future "approved this tool already"
    /// short-circuit.
    #[tokio::test]
    async fn two_sequential_bash_invokes_with_different_args_both_prompt() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        // Default `prompt` policy with bash unlisted — falls through to default.
        let mut state = GateState::new(Policy::new(Decision::Prompt));
        let advertise = advertise_body(
            "basic-tools",
            json!([{"name": "bash", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _register = rx.recv().await.unwrap();

        // First invoke: bash(pwd).
        let first = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-pwd",
            "name": "bash",
            "args": { "command": "pwd" },
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_invoke(&tx, &first, &mut state).await.unwrap();
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("chat.tool.permission_request"),
            "bash(pwd) must prompt under default Prompt policy"
        );
        assert_eq!(body.get("id").and_then(Value::as_str), Some("prov-pwd"));

        // User approves bash(pwd).
        let approve = json!({
            "kind": "tool.permission_response",
            "id": "prov-pwd",
            "decision": "approve",
        })
        .as_object()
        .unwrap()
        .clone();
        handle_permission_response(&tx, &approve, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap(); // basic-tools.tool.invoke forwarded

        // Second invoke: bash(ls -la). Same tool, different args.
        let second = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-ls",
            "name": "bash",
            "args": { "command": "ls -la" },
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_invoke(&tx, &second, &mut state).await.unwrap();
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("chat.tool.permission_request"),
            "bash(ls -la) must ALSO prompt — prior approve of bash(pwd) \
             must not establish a per-tool auto-allow cache"
        );
        assert_eq!(body.get("id").and_then(Value::as_str), Some("prov-ls"));
        assert!(state.awaiting_approval.contains_key("prov-ls"));
    }

    #[tokio::test]
    async fn deny_policy_replies_immediately_without_prompt() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut policy = Policy::new(Decision::Prompt);
        policy.set("bash", Decision::Deny);
        let mut state = GateState::new(policy);
        let advertise = advertise_body(
            "basic-tools",
            json!([{"name": "bash", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await;

        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-x",
            "name": "bash",
            "args": {}
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("tool.result")
        );
        let err = body.get("error").and_then(Value::as_str).unwrap();
        assert!(err.contains("denied by gate policy"));
        assert!(state.awaiting_approval.is_empty());
    }
}
