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
//! Runtime override (yolo mode): `tool-gate.set_mode { mode: "yolo" | "normal" }`
//! flips a global override. While `yolo`, every tool resolves to `Auto`
//! regardless of per-tool policy — useful for unattended testing. The gate
//! broadcasts `tool-gate.mode_changed { mode }` on transitions and also on
//! startup so observers (chat statusline) can render the current mode.
//!
//! Wire id mapping: the provider's outer id is preserved through the
//! permission-request flow (chat sees the same id the provider issued).
//! When forwarding to the underlying plugin we mint a fresh inner id so
//! the underlying plugin's broadcast `tool.result` doesn't collide with
//! the gate's eventual outbound `tool.result` to the provider.

mod error;
mod ncp;
mod policy;

use std::collections::HashMap;

use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::error::ToolGateError;
use crate::policy::{Decision, Policy};

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

async fn run(policy: Policy) -> Result<(), ToolGateError> {
    let (out_tx, _writer_handle) = ncp::spawn_stdout_writer();
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, ToolGateError>>(ncp::CHANNEL_CAP);
    let _reader_handle = ncp::spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = ncp::await_ready_ok(&mut in_rx).await?;
    tracing::info!(engine_version = %engine_version, "ready");

    send_event(&out_tx, hello_body()).await?;

    let mut state = GateState::new(policy);
    // Announce the initial mode so any UI that comes up after us still sees
    // it (chat starts fresh assuming `normal`; this keeps them in sync if a
    // future starter ever boots the gate in `yolo` from the CLI).
    send_event(&out_tx, mode_changed_body(state.yolo)).await?;
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
}

/// Pending forwarded invocation: maps the gate-minted inner id (used to
/// address the underlying plugin) back to the provider's outer id (so when
/// `tool.result` arrives we can rewrite the id and broadcast it).
#[derive(Debug, Clone)]
struct PendingForward {
    outer_id: String,
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
    /// Global override: while true, every tool resolves to `Auto` regardless
    /// of per-tool policy. Toggled by `tool-gate.set_mode`.
    yolo: bool,
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
            yolo: false,
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
    in_rx: &mut mpsc::Receiver<Result<Envelope, ToolGateError>>,
    state: &mut GateState,
) -> Result<(), ToolGateError> {
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
) -> Result<(), ToolGateError> {
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(k) => k,
        None => return Ok(()),
    };
    let advertise_kind = format!("{PLUGIN_NAME}.tools.advertise");
    let invoke_kind = format!("{PLUGIN_NAME}.tool.invoke");
    let set_mode_kind = format!("{PLUGIN_NAME}.set_mode");

    if kind == advertise_kind {
        handle_tools_advertise(out_tx, body, state).await?;
    } else if kind == invoke_kind {
        handle_tool_invoke(out_tx, body, state).await?;
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
) -> Result<(), ToolGateError> {
    let mode = match body.get("mode").and_then(Value::as_str) {
        Some(s) => s,
        None => {
            tracing::warn!("tool-gate.set_mode missing string field `mode`; dropping");
            return Ok(());
        }
    };
    let target = match mode {
        "yolo" => true,
        "normal" => false,
        other => {
            tracing::warn!(mode = %other, "tool-gate.set_mode: unknown mode; expected yolo|normal");
            return Ok(());
        }
    };
    if state.yolo == target {
        // No-op: still re-broadcast so a late observer (newly-spawned chat)
        // sees the current mode.
        send_event(out_tx, mode_changed_body(state.yolo)).await?;
        return Ok(());
    }
    state.yolo = target;
    tracing::info!(yolo = state.yolo, "mode changed");
    send_event(out_tx, mode_changed_body(state.yolo)).await?;
    Ok(())
}

async fn handle_tools_advertise(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    state: &mut GateState,
) -> Result<(), ToolGateError> {
    let source = match body.get("source").and_then(Value::as_str) {
        Some(s) => s.to_owned(),
        None => {
            tracing::warn!("tools.advertise missing required string field `source`; dropping");
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
    let tools: Vec<ToolSpec> = tools_arr
        .iter()
        .filter_map(|t| {
            let name = t.get("name").and_then(Value::as_str)?.to_owned();
            let description = t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let parameters = t
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            Some(ToolSpec {
                name,
                description,
                parameters,
            })
        })
        .collect();

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
) -> Result<(), ToolGateError> {
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
    let args = body.get("args").cloned().unwrap_or(Value::Object(Map::new()));

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

    let decision = if state.yolo {
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
                permission_request_body(&outer_id, &name, &args),
            )
            .await?;
        }
        Decision::Deny => {
            send_event(
                out_tx,
                tool_result_error_body(
                    &outer_id,
                    &format!("tool `{name}` denied by gate policy"),
                ),
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
) -> Result<(), ToolGateError> {
    let inner_id = state.next_inner_id();
    state.pending.insert(
        inner_id.clone(),
        PendingForward {
            outer_id: outer_id.to_owned(),
        },
    );
    send_event(
        out_tx,
        forward_invoke_body(source, &inner_id, name, args),
    )
    .await?;
    Ok(())
}

async fn handle_tool_result(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    state: &mut GateState,
) -> Result<(), ToolGateError> {
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
) -> Result<(), ToolGateError> {
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
        forward_to_source(
            out_tx,
            state,
            &approval.outer_id,
            &approval.source,
            &approval.name,
            approval.args,
        )
        .await?;
    } else {
        send_event(
            out_tx,
            tool_result_error_body(
                &approval.outer_id,
                &format!("tool `{}` denied by user", approval.name),
            ),
        )
        .await?;
    }
    Ok(())
}

// ---- body constructors -----------------------------------------------------

fn hello_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{PLUGIN_NAME}.hello")),
    );
    m.insert("version".into(), Value::String(PLUGIN_VERSION.into()));
    m
}

fn tool_register_body(state: &GateState) -> Map<String, Value> {
    let mut tools: Vec<Value> = Vec::new();
    for ts in state.advertised.values().flatten() {
        let mut m = Map::new();
        m.insert("name".into(), Value::String(ts.name.clone()));
        m.insert("description".into(), Value::String(ts.description.clone()));
        m.insert("parameters".into(), ts.parameters.clone());
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

fn permission_request_body(id: &str, name: &str, args: &Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String("chat.tool.permission_request".into()),
    );
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("tool".into(), Value::String(name.to_owned()));
    m.insert("args".into(), args.clone());
    m
}

fn mode_changed_body(yolo: bool) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{PLUGIN_NAME}.mode_changed")),
    );
    m.insert(
        "mode".into(),
        Value::String(if yolo { "yolo" } else { "normal" }.into()),
    );
    m
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
) -> Result<(), ToolGateError> {
    out_tx
        .send(PluginOutgoing::event(body))
        .await
        .map_err(|_| ToolGateError::WriterClosed)
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), ToolGateError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| ToolGateError::WriterClosed)
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

    fn advertise_body(source: &str, tools: Value) -> Map<String, Value> {
        json!({
            "kind": "tool-gate.tools.advertise",
            "source": source,
            "tools": tools,
        })
        .as_object()
        .expect("obj")
        .clone()
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
        handle_tools_advertise(&tx, &body, &mut state).await.unwrap();
        assert_eq!(state.tool_owner.get("read_file"), Some(&"basic-tools".into()));

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
    async fn invoke_with_prompt_emits_permission_request() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        // Pre-populate so tool_owner is set up.
        let body = advertise_body(
            "basic-tools",
            json!([{"name": "read_file", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &body, &mut state).await.unwrap();
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
        handle_tools_advertise(&tx, &body, &mut state).await.unwrap();
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
        assert_eq!(state.pending.get(inner_id).map(|p| p.outer_id.as_str()), Some("prov-2"));
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
        assert_eq!(body.get("kind").and_then(Value::as_str), Some("tool.result"));
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
        handle_tools_advertise(&tx, &body, &mut state).await.unwrap();
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
        handle_permission_response(&tx, &response, &mut state).await.unwrap();

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
    async fn denial_emits_tool_result_error_with_outer_id() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let body = advertise_body(
            "basic-tools",
            json!([{"name": "read_file", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &body, &mut state).await.unwrap();
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
        handle_permission_response(&tx, &response, &mut state).await.unwrap();

        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(body.get("kind").and_then(Value::as_str), Some("tool.result"));
        assert_eq!(body.get("id").and_then(Value::as_str), Some("prov-9"));
        let err = body.get("error").and_then(Value::as_str).unwrap();
        assert!(err.contains("denied by user"));
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
        state.yolo = true;
        let advertise = advertise_body(
            "basic-tools",
            json!([
                {"name": "read_file", "description": "", "parameters": {}},
                {"name": "bash", "description": "", "parameters": {}},
            ]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state).await.unwrap();
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
        assert!(state.yolo);
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
        state.yolo = true;
        let body = json!({
            "kind": "tool-gate.set_mode",
            "mode": "normal"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_set_mode(&tx, &body, &mut state).await.unwrap();
        assert!(!state.yolo);
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(body.get("mode").and_then(Value::as_str), Some("normal"));
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
        assert!(!state.yolo);
        drop(tx);
        assert!(rx.recv().await.is_none());
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
        handle_tools_advertise(&tx, &advertise, &mut state).await.unwrap();
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
        assert_eq!(body.get("kind").and_then(Value::as_str), Some("tool.result"));
        let err = body.get("error").and_then(Value::as_str).unwrap();
        assert!(err.contains("denied by gate policy"));
        assert!(state.awaiting_approval.is_empty());
    }
}
