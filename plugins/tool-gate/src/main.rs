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
//! ## What happens to a call
//!
//! The chain is always: a caller invokes a tool, the gate decides, and the
//! tool executes or the caller receives an error saying why it did not.
//! Nothing executes before the decision.
//!
//! 1. `tool-gate.tool.invoke { id, name, args, approval_policy?, allowlist? }`
//!    arrives. `approval_policy` is the calling agent's policy by name
//!    (`ReadOnly` or `Default`); a caller that states none gets `Default`,
//!    the configured behaviour.
//! 2. The gate sends `<source>.tool.prepare` with the same fields. The source
//!    checks the arguments, runs the tool's `prepare` step and the tool's
//!    decision function, and answers `tool.prepared { id, decision?, reason? }`
//!    where `decision` is `safe`, `unsure` or `denied`. A source may instead
//!    end the call with `tool.result { error }`.
//! 3. The gate applies the decision table in [`policy`] to the policy, the
//!    permission mode and that assessment: it sends `<source>.tool.invoke`
//!    under the same id, or asks the user with
//!    `chat.tool.permission_request`, or answers the caller with a
//!    `tool.result` error and tells the source to drop the call
//!    (`<source>.tool.cancel`).
//!
//! The gate forwards the caller's policy to the source on both requests. It
//! knows no tool's meaning: a tool that answers no decision is assessed from
//! the per-tool setting below.
//!
//! A call marked `workflow_node` was made by a node of a running workflow,
//! not by a model. The workflow was approved as a unit before it started, so
//! the gate only transports such a call: it goes straight to
//! `<source>.tool.invoke`, and neither the policy nor the mode is consulted.
//!
//! Per-tool setting from CLI flags, used when a tool does not assess a call:
//!
//! - `--auto <name>`   : such a call is safe.
//! - `--prompt <name>` : such a call is unsure.
//! - `--deny <name>`   : every call is denied, whatever the tool says.
//! - `--default <auto|prompt|deny>` : fallback for unlisted tools (default: prompt).
//!
//! Runtime mode: `tool-gate.set_mode { mode: "safe" | "auto" | "yolo" }`
//! selects the permission mode. Legacy `normal` maps to `safe`. The mode never
//! widens a `ReadOnly` caller. The gate broadcasts `tool-gate.mode_changed
//! { mode }` on transitions and also on startup so observers (chat statusline)
//! can render the current mode.
//!
//! Wire id mapping: the provider's outer id is preserved through the
//! permission-request flow (chat sees the same id the provider issued).
//! When forwarding to the underlying plugin we mint a fresh inner id so
//! the underlying plugin's broadcast `tool.result` doesn't collide with
//! the gate's eventual outbound `tool.result` to the provider.

//! Output failure contract: wire admission uses SDK `checked_send`, not delivery
//! acknowledgement. A rejected forward releases its inner correlation and emits
//! one small outer `tool.result.error`. Rejected source progress cancels the
//! actual inner invocation and settles the outer once; late results are inert.
//! Final-output rejection substitutes the same terminal error, never success.
//! Earlier/external effects remain unknown. Permission/catalog/control output
//! is required uncorrelated protocol: rejection or a closed writer ends the
//! plugin rather than inventing tool correlation. An undeliverable fallback also
//! ends transport. Malformed bounded JSON is noise; framing/I/O faults are not.

mod policy;

use std::collections::HashMap;

use nefor_plugin_sdk::{
    await_ready_ok, checked_send, spawn_stdin_reader, spawn_stdout_writer, TransportError,
};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::policy::{
    verdict, ApprovalPolicy, Assessment, Decision, GateMode, Policy, Verdict, DENIED_BY_SETTING,
};

const CHANNEL_CAP: usize = 256;

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
    let (out_tx, writer_handle) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let _reader_handle = spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = tokio::select! {
        result = await_ready_ok(&mut in_rx) => result?,
        _ = out_tx.closed() => return Err(TransportError::WriterClosed),
    };
    tracing::info!(engine_version = %engine_version, "ready");

    send_event(&out_tx, hello_body()).await?;

    let mut state = GateState::new(policy);
    // Announce the initial mode so any UI that comes up after us still sees it.
    send_event(&out_tx, mode_changed_body(state.mode)).await?;
    let mut result = run_dispatch_loop(&out_tx, &mut in_rx, &mut state).await;
    for (inner_id, pending) in &state.pending {
        if let Err(error) =
            send_event(&out_tx, forward_cancel_body(&pending.source, inner_id)).await
        {
            if result.is_ok() {
                result = Err(error);
            }
            break;
        }
    }
    state.pending.clear();
    state.preparing.clear();
    state.awaiting_approval.clear();
    result?;
    send_event(&out_tx, goodbye_body()).await?;
    drop(out_tx);
    if let Err(error) = writer_handle.await {
        std::panic::resume_unwind(error.into_panic());
    }
    Ok(())
}

/// Execution authority carried by an advertised capability.
///
/// Omitted wire metadata normalizes to `Routed`, preserving the existing tool
/// contract. Provider-native capabilities remain catalog entries, but the
/// named provider executes them without crossing the gate's invocation path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolExecution {
    Routed,
    ProviderNative { provider: String },
}

impl ToolExecution {
    fn parse(value: Option<&Value>, source: &str) -> Result<Self, String> {
        let Some(value) = value else {
            return Ok(Self::Routed);
        };
        let object = value
            .as_object()
            .ok_or_else(|| "execution must be an object".to_owned())?;
        if !object
            .keys()
            .all(|key| matches!(key.as_str(), "kind" | "provider"))
        {
            return Err("execution has unknown field".into());
        }
        match object.get("kind").and_then(Value::as_str) {
            Some("routed") => {
                if object.contains_key("provider") {
                    return Err("routed execution must not declare provider".into());
                }
                Ok(Self::Routed)
            }
            Some("provider_native") => {
                let provider = object
                    .get("provider")
                    .and_then(Value::as_str)
                    .filter(|provider| !provider.trim().is_empty())
                    .ok_or_else(|| {
                        "provider_native execution requires a non-empty provider".to_owned()
                    })?;
                if provider != source {
                    return Err(format!(
                        "provider_native execution provider `{provider}` must match advertising source `{source}`"
                    ));
                }
                Ok(Self::ProviderNative {
                    provider: provider.to_owned(),
                })
            }
            _ => Err("execution.kind must be routed or provider_native".into()),
        }
    }

    fn wire_value(&self) -> Value {
        match self {
            Self::Routed => serde_json::json!({ "kind": "routed" }),
            Self::ProviderNative { provider } => {
                serde_json::json!({ "kind": "provider_native", "provider": provider })
            }
        }
    }
}

/// One advertised tool. Mirrors the wire shape — name + description +
/// JSON Schema parameters — without depending on a provider catalog crate.
#[derive(Debug, Clone)]
struct ToolSpec {
    name: String,
    description: String,
    parameters: Value,
    display: Value,
    execution: ToolExecution,
    /// Text the tool adds to its description for an agent with a given
    /// approval policy, keyed by policy name. Carried to the public registry
    /// unchanged; whoever assembles an agent's request appends it.
    policy_descriptions: Option<Value>,
}

/// Pending forwarded invocation: maps the gate-minted inner id (used to
/// address the underlying plugin) back to the provider's outer id (so when
/// `tool.result` arrives we can rewrite the id, stamp the canonical invoked
/// tool name, and broadcast it) plus the source plugin the call was forwarded
/// to (so a `tool.cancel` for the outer id can be forwarded on to the same
/// source under the inner id).
#[derive(Debug, Clone)]
struct PendingForward {
    outer_id: String,
    source: String,
    name: String,
}

/// One call the gate has not yet executed: it is being prepared by its
/// source, or its prepared form is waiting for the user. `inner_id` is the
/// id the source knows the call by, from its prepare request onwards.
#[derive(Debug, Clone)]
struct PendingApproval {
    outer_id: String,
    inner_id: String,
    source: String,
    name: String,
    args: Value,
    invoking_from: Option<String>,
    invocation: Option<Value>,
    allowlist: Option<Value>,
    /// The caller's approval policy; `None` for a workflow node's call, which
    /// is transported without a decision.
    approval_policy: Option<ApprovalPolicy>,
}

struct GateState {
    /// Per-source advertised tools. Key: source plugin name.
    advertised: HashMap<String, Vec<ToolSpec>>,
    /// Reverse lookup: tool name → source plugin name. Rebuilt from
    /// `advertised` whenever it changes.
    tool_owner: HashMap<String, String>,
    /// Execution authority by tool name. Kept beside owner lookup so direct
    /// gate invocation can fail closed before permission policy is consulted.
    tool_execution: HashMap<String, ToolExecution>,
    /// Active forwards keyed by gate-minted inner id.
    pending: HashMap<String, PendingForward>,
    /// Calls whose source is preparing them, keyed by gate-minted inner id.
    preparing: HashMap<String, PendingApproval>,
    /// Active permission requests keyed by provider's outer id.
    awaiting_approval: HashMap<String, PendingApproval>,
    /// Monotonic counter for inner-id minting.
    inner_id_counter: u64,
    policy: Policy,
    /// Runtime permission mode, the second input of the decision table.
    mode: GateMode,
}

impl GateState {
    fn new(policy: Policy) -> Self {
        Self {
            advertised: HashMap::new(),
            tool_owner: HashMap::new(),
            tool_execution: HashMap::new(),
            pending: HashMap::new(),
            preparing: HashMap::new(),
            awaiting_approval: HashMap::new(),
            inner_id_counter: 0,
            policy,
            mode: GateMode::Safe,
        }
    }

    fn rebuild_owner_map(&mut self) {
        self.tool_owner.clear();
        self.tool_execution.clear();
        for (source, tools) in &self.advertised {
            for tool in tools {
                self.tool_owner.insert(tool.name.clone(), source.clone());
                self.tool_execution
                    .insert(tool.name.clone(), tool.execution.clone());
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
            _ = out_tx.closed() => return Err(TransportError::WriterClosed),
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
                    Some(Err(TransportError::Parse(e))) => {
                        tracing::warn!(error = %e, "stdin parse error; dropping line");
                    }
                    Some(Err(e)) => return Err(e),
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
    } else if kind == "tool.prepared" {
        handle_tool_prepared(out_tx, body, state).await?;
    } else if kind == "tool.stream" {
        handle_tool_stream(out_tx, body, state).await?;
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
    fn nonempty(value: Option<&Value>) -> bool {
        value
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    }
    fn selector(value: Option<&Value>, location: &str) -> Result<(), String> {
        let object = value
            .and_then(Value::as_object)
            .ok_or_else(|| format!("{location} must be an object"))?;
        if !object
            .keys()
            .all(|key| matches!(key.as_str(), "source" | "path" | "default" | "fallback"))
        {
            return Err(format!("{location} has unknown field"));
        }
        if !matches!(
            object.get("source").and_then(Value::as_str),
            Some("args" | "result")
        ) {
            return Err(format!("{location}.source must be args or result"));
        }
        let valid_path = object.get("path").is_some_and(|path| {
            path.as_str().is_some_and(|value| !value.is_empty())
                || path.as_array().is_some_and(|parts| {
                    !parts.is_empty()
                        && parts
                            .iter()
                            .all(|part| part.as_str().is_some_and(|value| !value.is_empty()))
                })
        });
        if !valid_path {
            return Err(format!(
                "{location}.path must be a non-empty string or string array"
            ));
        }
        if object.get("fallback").is_some() {
            selector(object.get("fallback"), &format!("{location}.fallback"))?;
        }
        Ok(())
    }
    fn field(value: &Value, location: &str) -> Result<(), String> {
        let object = value
            .as_object()
            .ok_or_else(|| format!("{location} must be an object"))?;
        if !object.keys().all(|key| {
            matches!(
                key.as_str(),
                "label" | "select" | "kind" | "omit" | "sensitive" | "max_lines" | "max_bytes"
            )
        }) {
            return Err(format!("{location} has unknown field"));
        }
        if !nonempty(object.get("label")) {
            return Err(format!("{location}.label must be non-empty"));
        }
        selector(object.get("select"), &format!("{location}.select"))?;
        let kind = object.get("kind").and_then(Value::as_str).unwrap_or("");
        if !matches!(
            kind,
            "text" | "scalar" | "status" | "path" | "bytes" | "list" | "structured"
        ) {
            return Err(format!("{location}.kind is unknown"));
        }
        if object
            .get("omit")
            .is_some_and(|value| !matches!(value.as_str(), Some("missing" | "empty")))
        {
            return Err(format!("{location}.omit must be missing or empty"));
        }
        if object
            .get("sensitive")
            .is_some_and(|value| !matches!(value.as_str(), Some("redact" | "omit")))
        {
            return Err(format!("{location}.sensitive must be redact or omit"));
        }
        let positive = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_u64)
                .is_some_and(|value| value > 0)
        };
        if kind == "text" && (!positive("max_lines") || !positive("max_bytes")) {
            return Err(format!(
                "{location} text requires positive max_lines and max_bytes"
            ));
        }
        if kind != "text" && (object.contains_key("max_lines") || object.contains_key("max_bytes"))
        {
            return Err(format!("{location} bounds are only valid for text"));
        }
        Ok(())
    }
    fn validate_primary(value: &Value, location: &str) -> Result<(), String> {
        let Some(object) = value.as_object() else {
            return Err(format!("{location} must be an object"));
        };
        let Some(parts) = object.get("parts") else {
            return field(value, location);
        };
        if object.keys().any(|key| key != "parts") {
            return Err(format!("{location} has unknown field"));
        }
        let parts = parts
            .as_array()
            .filter(|parts| !parts.is_empty())
            .ok_or_else(|| format!("{location}.parts must be a non-empty array"))?;
        for (index, part) in parts.iter().enumerate() {
            let part_location = format!("{location}.parts[{index}]");
            let Some(part_object) = part.as_object() else {
                return Err(format!("{part_location} must be an object"));
            };
            if let Some(text) = part_object.get("text") {
                if part_object.keys().any(|key| key != "text") {
                    return Err(format!("{part_location} has unknown field"));
                }
                if !text.is_string() {
                    return Err(format!("{part_location}.text must be a string"));
                }
            } else {
                field(part, &part_location)?;
            }
        }
        Ok(())
    }
    fn fields(value: Option<&Value>, location: &str) -> Result<(), String> {
        let values = value
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{location} must be an array"))?;
        for (index, value) in values.iter().enumerate() {
            field(value, &format!("{location}[{index}]"))?;
        }
        Ok(())
    }
    fn policy(value: &Value, location: &str, allow_variant: bool) -> Result<(), String> {
        let object = value
            .as_object()
            .ok_or_else(|| format!("{location} must be an object"))?;
        if !object.keys().all(|key| {
            matches!(
                key.as_str(),
                "compact" | "expanded" | "result" | "lifecycle"
            ) || (allow_variant && key == "variant")
        }) {
            return Err(format!("{location} has unknown field"));
        }
        for (name, expanded) in [("compact", false), ("expanded", true)] {
            let view = object
                .get(name)
                .and_then(Value::as_object)
                .ok_or_else(|| format!("{location}.{name} must be an object"))?;
            if !view.keys().all(|key| {
                matches!(key.as_str(), "label" | "primary") || (expanded && key == "fields")
            }) {
                return Err(format!("{location}.{name} has unknown field"));
            }
            if !nonempty(view.get("label")) {
                return Err(format!("{location}.{name}.label must be non-empty"));
            }
            if let Some(primary) = view.get("primary") {
                validate_primary(primary, &format!("{location}.{name}.primary"))?;
            }
            if expanded {
                fields(view.get("fields"), &format!("{location}.{name}.fields"))?;
            }
        }
        if object
            .get("lifecycle")
            .is_some_and(|value| value.as_str() != Some("delayed"))
        {
            return Err(format!("{location}.lifecycle must be delayed"));
        }
        let result = object
            .get("result")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("{location}.result must be an object"))?;
        if !result
            .keys()
            .all(|key| matches!(key.as_str(), "kind" | "text" | "fields"))
        {
            return Err(format!("{location}.result has unknown field"));
        }
        match result.get("kind").and_then(Value::as_str) {
            Some("content") if !result.contains_key("text") => (),
            Some("receipt") if nonempty(result.get("text")) => (),
            _ => {
                return Err(format!(
                    "{location}.result must be content or a receipt with text"
                ))
            }
        }
        fields(result.get("fields"), &format!("{location}.result.fields"))
    }
    policy(display, "display", true)?;
    if let Some(variant) = display.get("variant") {
        let object = variant
            .as_object()
            .ok_or_else(|| "display.variant must be an object".to_owned())?;
        if !object
            .keys()
            .all(|key| matches!(key.as_str(), "select" | "cases"))
        {
            return Err("display.variant has unknown field".into());
        }
        selector(object.get("select"), "display.variant.select")?;
        let cases = object
            .get("cases")
            .and_then(Value::as_object)
            .ok_or_else(|| "display.variant.cases must be an object".to_owned())?;
        for (name, case) in cases {
            policy(case, &format!("display.variant.cases.{name}"), false)?;
        }
    }
    Ok(())
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
            let execution = ToolExecution::parse(t.get("execution"), &source)
                .map_err(|e| "invalid execution for ".to_owned() + &name + ": " + &e)?;
            let policy_descriptions = parse_policy_descriptions(t.get("policy_descriptions"))
                .map_err(|e| "invalid policy_descriptions for ".to_owned() + &name + ": " + &e)?;
            Ok(ToolSpec {
                name,
                description,
                parameters,
                display,
                execution,
                policy_descriptions,
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
    let allowed_names = match tool_name_list(body, "allowlist") {
        Ok(names) => names,
        Err(message) => {
            send_event(out_tx, tool_result_error_body(&outer_id, &message)).await?;
            return Ok(());
        }
    };
    let allowlist = body.get("allowlist").and_then(Value::as_array);
    let request_local_tools = match tool_name_list(body, "request_local_tools") {
        Ok(names) => names.unwrap_or_default(),
        Err(message) => {
            send_event(out_tx, tool_result_error_body(&outer_id, &message)).await?;
            return Ok(());
        }
    };
    let invoking_from = body.get("from").and_then(Value::as_str).map(str::to_owned);
    let invocation = body.get("invocation").cloned();

    // A name outside the invocation's allowlist and a name no source
    // advertises get the same answer: the caller learns only which tools it
    // can call, not which other tools exist.
    let allowed = allowed_names
        .as_ref()
        .is_none_or(|names| names.contains(&name.as_str()));
    let source = match state.tool_owner.get(&name).filter(|_| allowed).cloned() {
        Some(s) => s,
        None => {
            let available = available_tools(state, allowed_names.as_deref(), &request_local_tools);
            send_event(
                out_tx,
                tool_result_error_body(&outer_id, &unknown_tool_message(&name, &available)),
            )
            .await?;
            return Ok(());
        }
    };
    if let Some(ToolExecution::ProviderNative { provider }) = state.tool_execution.get(&name) {
        send_event(
            out_tx,
            tool_result_error_body(
                &outer_id,
                &format!(
                    "tool `{name}` is provider-native to `{provider}` and cannot be invoked through tool-gate"
                ),
            ),
        )
        .await?;
        return Ok(());
    }

    if body.get("workflow_node").and_then(Value::as_bool) == Some(true) {
        // A node of a running workflow, not a model's tool call: the workflow
        // was the unit of approval, so this call is only transported.
        let inner_id = state.next_inner_id();
        return execute(
            out_tx,
            state,
            PendingApproval {
                outer_id,
                inner_id,
                source,
                name,
                args,
                invoking_from,
                invocation,
                allowlist: allowlist.map(|values| Value::Array(values.clone())),
                approval_policy: None,
            },
        )
        .await;
    }
    let approval_policy = match body.get("approval_policy") {
        // A caller that states no policy gets the configured behaviour.
        None | Some(Value::Null) => ApprovalPolicy::Default,
        Some(value) => match value.as_str().and_then(ApprovalPolicy::parse) {
            Some(policy) => policy,
            None => {
                send_event(
                    out_tx,
                    tool_result_error_body(
                        &outer_id,
                        "tool.invoke field `approval_policy` must be `ReadOnly` or `Default`",
                    ),
                )
                .await?;
                return Ok(());
            }
        },
    };
    // `--deny` rejects at once when the table has no way to run the call.
    if state.policy.decide(&name) == Decision::Deny
        && verdict(approval_policy, state.mode, Assessment::Denied) == Verdict::Deny
    {
        send_event(
            out_tx,
            tool_result_error_body(
                &outer_id,
                &denial_message(&name, approval_policy, Some(DENIED_BY_SETTING)),
            ),
        )
        .await?;
        return Ok(());
    }
    let inner_id = state.next_inner_id();
    prepare(
        out_tx,
        state,
        PendingApproval {
            outer_id,
            inner_id,
            source,
            name,
            args,
            invoking_from,
            invocation,
            allowlist: allowlist.map(|values| Value::Array(values.clone())),
            approval_policy: Some(approval_policy),
        },
    )
    .await
}

/// Ask the call's source to prepare it and say what it means under the
/// caller's policy. The inner id is reserved from here on, so the source's
/// answer, an early `tool.result`, and a cancellation all correlate.
async fn prepare(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    state: &mut GateState,
    call: PendingApproval,
) -> Result<(), TransportError> {
    state.pending.insert(
        call.inner_id.clone(),
        PendingForward {
            outer_id: call.outer_id.clone(),
            source: call.source.clone(),
            name: call.name.clone(),
        },
    );
    let result = send_event(out_tx, source_request_body("prepare", &call)).await;
    if let Err(error) = result {
        state.pending.remove(&call.inner_id);
        return admission_failure(out_tx, &call.outer_id, error).await;
    }
    state.preparing.insert(call.inner_id.clone(), call);
    Ok(())
}

/// The source's answer to a prepare request: apply the decision table.
async fn handle_tool_prepared(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    state: &mut GateState,
) -> Result<(), TransportError> {
    let Some(inner_id) = body.get("id").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(call) = state.preparing.remove(inner_id) else {
        return Ok(());
    };
    // Only calls that carry a policy are prepared.
    let approval_policy = call.approval_policy.unwrap_or(ApprovalPolicy::Default);
    let stated_reason = body
        .get("reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty());
    let (assessment, reason) = if state.policy.decide(&call.name) == Decision::Deny {
        (Assessment::Denied, Some(DENIED_BY_SETTING))
    } else {
        match body.get("decision") {
            None | Some(Value::Null) => state.policy.unassessed(&call.name, approval_policy),
            Some(value) => match value.as_str().and_then(Assessment::parse) {
                Some(assessment) => (assessment, stated_reason),
                // An answer the gate cannot read is not permission.
                None => (
                    Assessment::Denied,
                    Some("its source answered with an unknown decision"),
                ),
            },
        }
    };
    match verdict(approval_policy, state.mode, assessment) {
        Verdict::Run => execute(out_tx, state, call).await,
        Verdict::Ask => {
            let request = permission_request_body(&call, approval_policy, reason);
            state.awaiting_approval.insert(call.outer_id.clone(), call);
            send_event(out_tx, request).await
        }
        Verdict::Deny => {
            let message = denial_message(&call.name, approval_policy, reason);
            deny(out_tx, state, &call, &message).await
        }
    }
}

/// Run the call: `<source>.tool.invoke` under the id its source prepared it
/// with (or a fresh one for a transported workflow-node call).
async fn execute(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    state: &mut GateState,
    call: PendingApproval,
) -> Result<(), TransportError> {
    state.pending.insert(
        call.inner_id.clone(),
        PendingForward {
            outer_id: call.outer_id.clone(),
            source: call.source.clone(),
            name: call.name.clone(),
        },
    );
    let result = send_event(out_tx, source_request_body("invoke", &call)).await;
    if let Err(error) = result {
        state.pending.remove(&call.inner_id);
        return admission_failure(out_tx, &call.outer_id, error).await;
    }
    Ok(())
}

/// Refuse a prepared call: the source drops what it prepared and the caller
/// learns why the tool was not run.
async fn deny(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    state: &mut GateState,
    call: &PendingApproval,
    message: &str,
) -> Result<(), TransportError> {
    state.pending.remove(&call.inner_id);
    send_event(out_tx, tool_result_error_body(&call.outer_id, message)).await?;
    send_event(out_tx, forward_cancel_body(&call.source, &call.inner_id)).await
}

/// The error a caller receives for a call the gate did not run.
fn denial_message(name: &str, policy: ApprovalPolicy, reason: Option<&str>) -> String {
    match policy {
        ApprovalPolicy::ReadOnly => format!(
            "tool `{name}` was not run: your approval policy is ReadOnly and {}",
            reason.unwrap_or("this call could not be shown to be read-only")
        ),
        ApprovalPolicy::Default => format!(
            "tool `{name}` was not run: {}",
            reason.unwrap_or("this call is not allowed")
        ),
    }
}

/// Cancel an invocation by the caller's outer id.
///
/// Cancellation is a terminal gate operation: permission state is removed
/// before a stale approval can observe it, forwarded correlation is removed
/// before a late source result can observe it, and one cancellation result
/// settles the outer caller. Removing every matching forward also makes an
/// accidental duplicate outer id fail closed without retaining correlations.
async fn handle_tool_cancel(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    state: &mut GateState,
) -> Result<(), TransportError> {
    let outer_id = match body.get("id").and_then(Value::as_str) {
        Some(s) => s.to_owned(),
        None => return Ok(()),
    };

    let canceled_approval = state.awaiting_approval.remove(&outer_id).is_some();
    let mut forwards = state
        .pending
        .iter()
        .filter(|(_, pending)| pending.outer_id == outer_id)
        .map(|(inner_id, pending)| (inner_id.clone(), pending.source.clone()))
        .collect::<Vec<_>>();
    forwards.sort_unstable();
    for (inner_id, _) in &forwards {
        state.pending.remove(inner_id);
        state.preparing.remove(inner_id);
    }

    if !canceled_approval && forwards.is_empty() {
        tracing::info!(outer_id = %outer_id, "tool.cancel for an unknown/settled invocation; no-op");
        return Ok(());
    }

    for (inner_id, source) in forwards {
        tracing::info!(outer_id = %outer_id, inner_id = %inner_id, source = %source, "tool.cancel forwarded");
        send_event(out_tx, forward_cancel_body(&source, &inner_id)).await?;
    }
    send_event(out_tx, tool_canceled_body(&outer_id)).await?;
    Ok(())
}

async fn handle_tool_stream(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    state: &mut GateState,
) -> Result<(), TransportError> {
    let inner_id = match body.get("id").and_then(Value::as_str) {
        Some(id) => id,
        None => return Ok(()),
    };
    let Some(pending) = state.pending.get(inner_id) else {
        return Ok(());
    };
    let mut out = body.clone();
    out.insert("id".into(), Value::String(pending.outer_id.clone()));
    let outer_id = pending.outer_id.clone();
    let source = pending.source.clone();
    if let Err(error) = send_event(out_tx, out).await {
        state.pending.remove(inner_id);
        if matches!(
            error,
            TransportError::FrameTooLarge { .. } | TransportError::InvalidFraming
        ) {
            send_event(out_tx, forward_cancel_body(&source, inner_id)).await?;
        }
        return admission_failure(out_tx, &outer_id, error).await;
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
    // A source may end a call while preparing it or while the user is asked.
    state.preparing.remove(inner_id);
    state
        .awaiting_approval
        .retain(|_, call| call.inner_id != inner_id);
    let mut out = body.clone();
    out.insert("kind".into(), Value::String("tool.result".into()));
    out.insert("id".into(), Value::String(pending.outer_id.clone()));
    out.insert("name".into(), Value::String(pending.name));
    send_event(out_tx, out).await
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

    let Some(mut approval) = state.awaiting_approval.remove(&outer_id) else {
        // No matching pending request — likely a stale response or one
        // belonging to a different gate. Drop silently.
        return Ok(());
    };

    if decision == "approve" {
        approval.args = body.get("args").cloned().unwrap_or(approval.args);
        execute(out_tx, state, approval).await?;
    } else {
        // Optional `reason` carries context from a non-user approver. When
        // present we surface it verbatim in the tool.result.error so the
        // agent learns why and what to do next.
        let reason = body.get("reason").and_then(Value::as_str);
        let error_msg = match reason {
            Some(r) if !r.is_empty() => format!("tool `{}` denied: {r}", approval.name),
            _ => format!("tool `{}` denied by user", approval.name),
        };
        deny(out_tx, state, &approval, &error_msg).await?;
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
    let mut specs = state
        .advertised
        .iter()
        .flat_map(|(owner, tools)| tools.iter().map(move |tool| (owner, tool)))
        .collect::<Vec<_>>();
    specs.sort_unstable_by(|(_, left), (_, right)| left.name.cmp(&right.name));
    let mut tools: Vec<Value> = Vec::new();
    for (owner, ts) in specs {
        let mut m = Map::new();
        m.insert("name".into(), Value::String(ts.name.clone()));
        m.insert("owner".into(), Value::String(owner.clone()));
        m.insert("description".into(), Value::String(ts.description.clone()));
        m.insert("parameters".into(), ts.parameters.clone());
        m.insert("display".into(), ts.display.clone());
        m.insert("execution".into(), ts.execution.wire_value());
        if let Some(descriptions) = &ts.policy_descriptions {
            m.insert("policy_descriptions".into(), descriptions.clone());
        }
        tools.push(Value::Object(m));
    }
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("tool.register".into()));
    m.insert("tools".into(), Value::Array(tools));
    m
}

/// An advertised tool's per-policy description text: an object from an
/// approval policy's name to non-empty text. An empty object is the same as
/// none.
fn parse_policy_descriptions(value: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .ok_or_else(|| "must be an object".to_owned())?;
    for (policy, text) in object {
        if ApprovalPolicy::parse(policy).is_none() {
            return Err(format!("unknown approval policy `{policy}`"));
        }
        if text.as_str().is_none_or(str::is_empty) {
            return Err(format!("text for `{policy}` must be a non-empty string"));
        }
    }
    Ok((!object.is_empty()).then(|| value.clone()))
}

/// Read an optional invocation field holding tool names. `Err` carries the
/// caller-facing message for a malformed field.
fn tool_name_list<'a>(
    body: &'a Map<String, Value>,
    field: &str,
) -> Result<Option<Vec<&'a str>>, String> {
    match body.get(field) {
        None => Ok(None),
        Some(Value::Array(values)) => values
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or_else(|| malformed_tool_name_list(field)),
        Some(_) => Err(malformed_tool_name_list(field)),
    }
}

fn malformed_tool_name_list(field: &str) -> String {
    format!("tool.invoke field `{field}` must be an array containing only tool-name strings")
}

/// The tools the caller of one invocation can call, in the order its model
/// was offered them. An allowlist is offered in its own order, less the names
/// no source advertises; without one the whole registry is offered in
/// `tool.register` order. Request-local tools are the caller's own and follow.
fn available_tools(
    state: &GateState,
    allowlist: Option<&[&str]>,
    request_local_tools: &[&str],
) -> Vec<String> {
    let mut available: Vec<String> = Vec::new();
    match allowlist {
        Some(allowlist) => {
            for name in allowlist {
                if state.tool_owner.contains_key(*name) && !available.iter().any(|n| n == name) {
                    available.push((*name).to_owned());
                }
            }
        }
        None => {
            available.extend(state.tool_owner.keys().cloned());
            available.sort_unstable();
        }
    }
    for name in request_local_tools {
        if !available.iter().any(|n| n == name) {
            available.push((*name).to_owned());
        }
    }
    available
}

/// The one answer to a call naming a tool its caller cannot use. `libs.tool`
/// words the same message for a tool source (`unknown_tool_message` there);
/// keep the two identical.
fn unknown_tool_message(name: &str, available: &[String]) -> String {
    if available.is_empty() {
        format!("unknown tool `{name}`; no tools are available")
    } else {
        format!(
            "unknown tool `{name}`; available tools: {}",
            available.join(", ")
        )
    }
}

/// `<source>.tool.prepare` or `<source>.tool.invoke` for one call. Both carry
/// the same fields, the caller's approval policy included.
fn source_request_body(phase: &str, call: &PendingApproval) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{}.tool.{phase}", call.source)),
    );
    m.insert("id".into(), Value::String(call.inner_id.clone()));
    m.insert("caller_id".into(), Value::String(call.outer_id.clone()));
    if let Some(from) = &call.invoking_from {
        m.insert("from".into(), Value::String(from.clone()));
    }
    if let Some(invocation) = &call.invocation {
        m.insert("invocation".into(), invocation.clone());
    }
    if let Some(allowlist) = &call.allowlist {
        m.insert("allowlist".into(), allowlist.clone());
    }
    match call.approval_policy {
        Some(policy) => {
            m.insert(
                "approval_policy".into(),
                Value::String(policy.as_str().into()),
            );
        }
        None => {
            m.insert("workflow_node".into(), Value::Bool(true));
        }
    }
    m.insert("name".into(), Value::String(call.name.clone()));
    m.insert("args".into(), call.args.clone());
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

fn tool_canceled_body(outer_id: &str) -> Map<String, Value> {
    tool_result_error_body(outer_id, "tool invocation canceled")
}

fn permission_request_body(
    call: &PendingApproval,
    policy: ApprovalPolicy,
    reason: Option<&str>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String("chat.tool.permission_request".into()),
    );
    m.insert("id".into(), Value::String(call.outer_id.clone()));
    m.insert("tool".into(), Value::String(call.name.clone()));
    m.insert("args".into(), call.args.clone());
    m.insert(
        "approval_policy".into(),
        Value::String(policy.as_str().into()),
    );
    if let Some(reason) = reason {
        m.insert("reason".into(), Value::String(reason.to_owned()));
    }
    if let Some(allowlist) = &call.allowlist {
        m.insert("allowlist".into(), allowlist.clone());
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
    let terminal_id = if body.get("kind").and_then(Value::as_str) == Some("tool.result") {
        body.get("id").and_then(Value::as_str).map(str::to_owned)
    } else {
        None
    };
    match checked_send(out_tx, PluginOutgoing::event(body)).await {
        Err(TransportError::FrameTooLarge { .. } | TransportError::InvalidFraming)
            if terminal_id.is_some() =>
        {
            checked_send(
                out_tx,
                PluginOutgoing::event(tool_result_error_body(
                    terminal_id.as_deref().unwrap_or_default(),
                    "output admission failed; earlier or external effects may have occurred",
                )),
            )
            .await
        }
        result => result,
    }
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), TransportError> {
    checked_send(
        out_tx,
        PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }),
    )
    .await
}

/// Admission is not delivery. Never replay the source operation after a rejected output.
async fn admission_failure(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    outer_id: &str,
    error: TransportError,
) -> Result<(), TransportError> {
    match error {
        TransportError::FrameTooLarge { .. } | TransportError::InvalidFraming => {
            checked_send(
                out_tx,
                PluginOutgoing::event(tool_result_error_body(
                    outer_id,
                    "output admission failed; earlier or external effects may have occurred",
                )),
            )
            .await
        }
        error => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use mlua::{Function, Lua, Table, Value as LuaValue};
    use serde_json::json;

    fn huge() -> Value {
        Value::String("x".repeat(16 * 1024 * 1024))
    }

    fn pending_invocation(args: Value) -> PendingApproval {
        PendingApproval {
            outer_id: "outer".into(),
            inner_id: "gate-1".into(),
            approval_policy: Some(ApprovalPolicy::Default),
            source: "source".into(),
            name: "test".into(),
            args,
            invoking_from: None,
            invocation: None,
            allowlist: None,
        }
    }

    /// Invoke a tool whose source assesses nothing: answer the gate's prepare
    /// request without a decision, so the configured per-tool setting decides.
    async fn invoke_unassessed(
        tx: &mpsc::Sender<PluginOutgoing>,
        rx: &mut mpsc::Receiver<PluginOutgoing>,
        invoke: &Map<String, Value>,
        state: &mut GateState,
    ) {
        handle_tool_invoke(tx, invoke, state).await.unwrap();
        // Everything that is not this call's prepare request goes back to the
        // test in its original order.
        let mut prepare = None;
        let mut others = Vec::new();
        while let Ok(message) = rx.try_recv() {
            let body = wire_body(message.clone());
            let is_prepare = body["kind"]
                .as_str()
                .is_some_and(|kind| kind.ends_with(".tool.prepare"));
            if is_prepare && body["caller_id"] == invoke["id"] {
                prepare = Some(body);
            } else {
                others.push(message);
            }
        }
        for message in others {
            tx.send(message).await.unwrap();
        }
        if let Some(prepare) = prepare {
            answer_prepared(tx, state, &prepare, json!({})).await;
        }
    }

    /// Answer one prepare request as its source would.
    async fn answer_prepared(
        tx: &mpsc::Sender<PluginOutgoing>,
        state: &mut GateState,
        prepare: &Value,
        mut answer: Value,
    ) {
        answer["kind"] = json!("tool.prepared");
        answer["id"] = prepare["id"].clone();
        handle_tool_prepared(tx, answer.as_object().unwrap(), state)
            .await
            .unwrap();
    }

    fn wire_body(message: PluginOutgoing) -> Value {
        serde_json::from_str::<Value>(&message.to_line()).unwrap()["body"].clone()
    }

    #[tokio::test]
    async fn oversized_forward_rolls_back_then_later_invocation_works() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut state = make_state();
        execute(&tx, &mut state, pending_invocation(huge()))
            .await
            .unwrap();
        assert!(state.pending.is_empty());
        let error = wire_body(rx.recv().await.unwrap());
        assert_eq!(error["id"], "outer");
        assert!(error["error"].as_str().unwrap().contains("admission"));
        assert!(rx.try_recv().is_err());
        execute(&tx, &mut state, pending_invocation(json!({})))
            .await
            .unwrap();
        assert_eq!(state.pending.len(), 1);
        assert_eq!(
            wire_body(rx.recv().await.unwrap())["kind"],
            "source.tool.invoke"
        );
    }

    #[tokio::test]
    async fn rejected_stream_cancels_source_and_makes_late_result_inert() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut state = make_state();
        execute(&tx, &mut state, pending_invocation(json!({})))
            .await
            .unwrap();
        let inner = wire_body(rx.recv().await.unwrap())["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let body = json!({"kind": "tool.stream", "id": inner, "text": huge()})
            .as_object()
            .unwrap()
            .clone();
        handle_tool_stream(&tx, &body, &mut state).await.unwrap();
        assert!(state.pending.is_empty());
        let cancel = wire_body(rx.recv().await.unwrap());
        assert_eq!(cancel["kind"], "source.tool.cancel");
        assert_eq!(cancel["id"], inner);
        let error = wire_body(rx.recv().await.unwrap());
        assert_eq!(error["kind"], "tool.result");
        assert_eq!(error["id"], "outer");
        handle_tool_result(&tx, &body, &mut state).await.unwrap();
        handle_permission_response(
            &tx,
            &json!({"id": "outer", "decision": "approve"})
                .as_object()
                .unwrap()
                .clone(),
            &mut state,
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn oversized_final_result_settles_once_without_success() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut state = make_state();
        execute(&tx, &mut state, pending_invocation(json!({})))
            .await
            .unwrap();
        let inner = wire_body(rx.recv().await.unwrap())["id"].clone();
        let result = json!({"kind": "tool.result", "id": inner, "output": huge()})
            .as_object()
            .unwrap()
            .clone();
        handle_tool_result(&tx, &result, &mut state).await.unwrap();
        let error = wire_body(rx.recv().await.unwrap());
        assert_eq!(error["id"], "outer");
        assert!(error.get("output").is_none());
        assert!(error["error"].as_str().unwrap().contains("effects"));
        handle_tool_result(&tx, &result, &mut state).await.unwrap();
        assert!(rx.try_recv().is_err());
        assert!(state.pending.is_empty());
    }

    #[tokio::test]
    async fn closed_writer_ends_idle_dispatch_and_failed_forward_releases_pending() {
        let (tx, rx) = mpsc::channel(8);
        drop(rx);
        let mut state = make_state();
        assert!(matches!(
            execute(&tx, &mut state, pending_invocation(json!({}))).await,
            Err(TransportError::WriterClosed)
        ));
        assert!(state.pending.is_empty());
        let (_input, mut input) = mpsc::channel(8);
        assert!(matches!(
            run_dispatch_loop(&tx, &mut input, &mut state).await,
            Err(TransportError::WriterClosed)
        ));
    }

    #[tokio::test]
    async fn required_uncorrelated_output_is_not_disguised_as_tool_result() {
        let (tx, mut rx) = mpsc::channel(8);
        let output = json!({"kind": "tool.register", "tools": huge()})
            .as_object()
            .unwrap()
            .clone();
        assert!(matches!(
            send_event(&tx, output).await,
            Err(TransportError::FrameTooLarge { .. })
        ));
        assert!(rx.try_recv().is_err());
    }

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
                        json!({
                            "compact": { "label": "Test tool" },
                            "expanded": { "label": "Test tool", "fields": [] },
                            "result": { "kind": "content", "fields": [] }
                        }),
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

    fn production_lua() -> Lua {
        let lua = Lua::new();
        let nefor = lua.create_table().expect("nefor table");
        nefor::lua::bindings::install_json(&lua, &nefor).expect("real json binding");

        let log = lua.create_table().expect("log table");
        let no_op: Function = lua
            .create_function(|_, _: mlua::Variadic<LuaValue>| Ok(()))
            .expect("no-op logger");
        for level in ["info", "warn", "error", "debug"] {
            log.set(level, no_op.clone()).expect("logger level");
        }
        nefor.set("log", log).expect("install logger");

        let engine = lua.create_table().expect("engine table");
        let calls = lua.create_table().expect("calls table");
        lua.globals()
            .set("_engine_calls", calls)
            .expect("calls global");
        engine
            .set(
                "send",
                lua.create_function(|lua, args: mlua::Variadic<LuaValue>| {
                    let payload = match args.first() {
                        Some(LuaValue::String(value)) => value.to_str()?.to_owned(),
                        _ => return Ok(()),
                    };
                    let calls: Table = lua.globals().get("_engine_calls")?;
                    calls.set(calls.len()? + 1, payload)?;
                    Ok(())
                })
                .expect("send function"),
            )
            .expect("install send");
        engine
            .set(
                "now",
                lua.create_function(|_, _: ()| Ok("2026-06-01T00:00:00.000Z"))
                    .expect("now function"),
            )
            .expect("install now");
        engine
            .set(
                "plugins",
                lua.create_function(|lua, _: ()| lua.create_table())
                    .expect("plugins function"),
            )
            .expect("install plugins");
        engine
            .set(
                "exit",
                lua.create_function(|_, _: mlua::Variadic<LuaValue>| Ok(()))
                    .expect("exit function"),
            )
            .expect("install exit");
        nefor.set("engine", engine).expect("install engine");

        let bus = lua.create_table().expect("bus table");
        bus.set(
            "on_event",
            lua.create_function(|_, _: mlua::Variadic<LuaValue>| Ok(()))
                .expect("on_event function"),
        )
        .expect("install on_event");
        nefor.set("bus", bus).expect("install bus");

        let fs = lua.create_table().expect("fs table");
        fs.set(
            "data_root",
            lua.create_function(|_, _: ()| Ok("/var/empty/nefor-tool-advertise-test"))
                .expect("data_root function"),
        )
        .expect("install data_root");
        nefor.set("fs", fs).expect("install fs");
        lua.globals().set("nefor", nefor).expect("nefor global");

        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("repository root")
            .to_path_buf();
        let starter = root.join("examples/nefor-agent");
        let shared = root.join("lua");
        let tool_gate = root.join("plugins/tool-gate/lua");
        lua.load(format!(
            r#"package.path = table.concat({{
              "{}/?.lua", "{}/?/init.lua",
              "{}/?.lua", "{}/?/init.lua",
              "{}/?.lua", "{}/?/init.lua", package.path
            }}, ";")"#,
            starter.display(),
            starter.display(),
            shared.display(),
            shared.display(),
            tool_gate.display(),
            tool_gate.display(),
        ))
        .exec()
        .expect("package path");
        lua
    }

    fn take_advertisement(lua: &Lua) -> Map<String, Value> {
        let payload: String = lua
            .load(
                r#"
                local calls = _engine_calls
                local payload = calls[#calls]
                _engine_calls = {}
                return payload
                "#,
            )
            .eval()
            .expect("captured advertisement");
        serde_json::from_str::<Value>(&payload)
            .expect("advertisement JSON")
            .get("body")
            .and_then(Value::as_object)
            .expect("advertisement body")
            .clone()
    }

    #[tokio::test]
    async fn production_lua_advertisements_cross_json_into_strict_catalog() {
        let lua = production_lua();
        lua.load(
            r#"
            local lead = require("libs.lead-workflow")
            lead.receive_msg({
              origin = "tool-gate",
              payload = nefor.json.encode({ body = { kind = "tool-gate.hello" } }),
            })
            "#,
        )
        .exec()
        .expect("lead advertisement");
        let lead = take_advertisement(&lua);

        lua.load(
            r#"
            local spec = require("libs.read-only-tools").build({
              include = { "discover_instruction_files", "skill" },
            })
            spec.receive_msg({
              origin = "tool-gate",
              payload = nefor.json.encode({ body = { kind = "tool-gate.hello" } }),
            })
            "#,
        )
        .exec()
        .expect("read-only advertisement");
        let read_only = take_advertisement(&lua);

        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        for advertisement in [lead, read_only] {
            handle_tools_advertise(&tx, &advertisement, &mut state)
                .await
                .expect("strict advertisement handling");
            let event: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
            assert_eq!(event["body"]["kind"], "tool.register", "{event}");
        }

        for name in [
            "mag-status",
            "mag-await",
            "mag-terminate",
            "write-review",
            "mag-write-file",
            "mag-preview",
            "mag-apply",
            "discover_instruction_files",
            "skill",
        ] {
            assert!(state.tool_owner.contains_key(name), "missing {name}");
        }
    }

    #[test]
    fn display_fields_marks_empty_table_as_json_array() {
        let lua = production_lua();
        let encoded: String = lua
            .load(
                r#"
                local display = require("libs.chat.tool_display")
                return nefor.json.encode({ fields = display.fields() })
                "#,
            )
            .eval()
            .expect("encode marked fields");
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).unwrap()["fields"],
            json!([])
        );
    }

    #[test]
    fn validates_display_contract() {
        let valid = json!({
            "compact": { "label": "Read" },
            "expanded": { "label": "Read", "fields": [] },
            "result": { "kind": "receipt", "text": "loaded", "fields": [] }
        });
        assert!(validate_display_contract(&valid).is_ok());
        let mut invalid = valid;
        invalid["expanded"]["fields"] = json!([{
            "label": "content", "select": { "source": "result", "path": "$" }, "kind": "text"
        }]);
        assert_eq!(
            validate_display_contract(&invalid).unwrap_err(),
            "display.expanded.fields[0] text requires positive max_lines and max_bytes"
        );
    }

    #[test]
    fn display_contract_wire_shape_is_strict() {
        let accepted = json!({
            "compact": { "label": "Read", "primary": {
                "label": "path", "select": { "source": "args", "path": "path" }, "kind": "path"
            } },
            "expanded": { "label": "Read", "fields": [{
                "label": "content", "select": { "source": "result", "path": "$" },
                "kind": "text", "max_lines": 20, "max_bytes": 1600
            }] },
            "result": { "kind": "content", "fields": [] }
        });
        assert!(validate_display_contract(&accepted).is_ok());
        let mut rejected = accepted;
        rejected["expanded"]["fields"][0]["unknown"] = json!(true);
        assert!(validate_display_contract(&rejected).is_err());
    }
    #[test]
    fn display_contract_accepts_composed_primary_parts() {
        let accepted = json!({
            "compact": { "label": "Click", "primary": { "parts": [
                { "label": "target", "select": { "source": "args", "path": "url" }, "kind": "path" },
                { "text": " link " },
                { "label": "link", "select": { "source": "args", "path": "link" }, "kind": "scalar" }
            ] } },
            "expanded": { "label": "Click", "fields": [] },
            "result": { "kind": "content", "fields": [] }
        });
        assert!(validate_display_contract(&accepted).is_ok());

        let mut rejected = accepted;
        rejected["compact"]["primary"]["parts"][1]["unknown"] = json!(true);
        assert_eq!(
            validate_display_contract(&rejected).unwrap_err(),
            "display.compact.primary.parts[1] has unknown field"
        );
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
        assert_eq!(arr[0]["owner"], "basic-tools");
    }

    #[tokio::test]
    async fn execution_metadata_is_closed_and_normalized_in_public_registry() {
        let cases = [
            (
                "basic-tools",
                json!({"name": "read_file"}),
                ToolExecution::Routed,
                json!({"kind": "routed"}),
            ),
            (
                "chatgpt",
                json!({
                    "name": "web_search",
                    "execution": {"kind": "provider_native", "provider": "chatgpt"}
                }),
                ToolExecution::ProviderNative {
                    provider: "chatgpt".into(),
                },
                json!({"kind": "provider_native", "provider": "chatgpt"}),
            ),
        ];

        for (source, tool, expected_execution, expected_wire) in cases {
            let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
            let mut state = make_state();
            let body = advertise_body(source, Value::Array(vec![tool]));
            handle_tools_advertise(&tx, &body, &mut state)
                .await
                .unwrap();

            assert_eq!(
                state.tool_execution.values().next(),
                Some(&expected_execution)
            );
            let event: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
            assert_eq!(event["body"]["tools"][0]["execution"], expected_wire);
        }
    }

    #[tokio::test]
    async fn malformed_execution_metadata_is_rejected_atomically() {
        let cases = [
            (json!("provider_native"), "execution must be an object"),
            (
                json!({"kind": "future"}),
                "execution.kind must be routed or provider_native",
            ),
            (
                json!({"kind": "routed", "provider": "chatgpt"}),
                "routed execution must not declare provider",
            ),
            (
                json!({"kind": "provider_native"}),
                "provider_native execution requires a non-empty provider",
            ),
            (
                json!({"kind": "provider_native", "provider": "other"}),
                "provider_native execution provider `other` must match advertising source `chatgpt`",
            ),
            (
                json!({"kind": "provider_native", "provider": "chatgpt", "extra": true}),
                "execution has unknown field",
            ),
        ];

        for (execution, expected) in cases {
            let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
            let mut state = make_state();
            let valid = advertise_body("basic-tools", json!([{"name": "read_file"}]));
            handle_tools_advertise(&tx, &valid, &mut state)
                .await
                .unwrap();
            let _register = rx.recv().await.unwrap();

            let invalid = advertise_body(
                "chatgpt",
                json!([{"name": "web_search", "execution": execution}]),
            );
            handle_tools_advertise(&tx, &invalid, &mut state)
                .await
                .unwrap();

            let event: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
            assert_eq!(event["body"]["kind"], "tool-gate.advertise_error");
            assert!(
                event["body"]["message"]
                    .as_str()
                    .is_some_and(|message| message.ends_with(expected)),
                "{event}"
            );
            assert_eq!(
                state.tool_owner.get("read_file").map(String::as_str),
                Some("basic-tools")
            );
            assert!(!state.tool_owner.contains_key("web_search"));
            assert!(!state.tool_execution.contains_key("web_search"));
        }
    }

    #[tokio::test]
    async fn provider_native_tools_share_collisions_but_never_enter_gate_execution() {
        for mode in [GateMode::Safe, GateMode::Yolo] {
            let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
            let mut state = make_state();
            state.mode = mode;
            let native = advertise_body(
                "chatgpt",
                json!([{
                    "name": "web_search",
                    "execution": {"kind": "provider_native", "provider": "chatgpt"}
                }]),
            );
            handle_tools_advertise(&tx, &native, &mut state)
                .await
                .unwrap();
            let register: Value =
                serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
            assert_eq!(register["body"]["tools"][0]["owner"], "chatgpt");

            let duplicate = advertise_body("basic-tools", json!([{"name": "web_search"}]));
            handle_tools_advertise(&tx, &duplicate, &mut state)
                .await
                .unwrap();
            let collision: Value =
                serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
            assert_eq!(collision["body"]["kind"], "tool-gate.advertise_error");
            assert_eq!(
                state.tool_owner.get("web_search").map(String::as_str),
                Some("chatgpt")
            );

            let invoke = json!({
                "kind": "tool-gate.tool.invoke",
                "id": format!("native-{mode:?}"),
                "name": "web_search",
                "args": {"query": "nefor"},
                "allowlist": ["web_search"]
            })
            .as_object()
            .unwrap()
            .clone();
            invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;
            let result: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
            assert_eq!(result["body"]["kind"], "tool.result");
            assert_eq!(
                result["body"]["error"],
                "tool `web_search` is provider-native to `chatgpt` and cannot be invoked through tool-gate"
            );
            assert!(state.awaiting_approval.is_empty());
            assert!(state.pending.is_empty());
            assert!(
                rx.try_recv().is_err(),
                "provider-native tool must not forward"
            );
        }
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
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;

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
    async fn invoke_excluded_from_allowlist_is_rejected_before_policy() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let body = advertise_body(
            "basic-tools",
            json!([
                {"name": "read_file", "description": "", "parameters": {}},
                {"name": "write_file", "description": "", "parameters": {}}
            ]),
        );
        handle_tools_advertise(&tx, &body, &mut state)
            .await
            .unwrap();
        let _register = rx.recv().await.unwrap();

        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "cap-excluded",
            "name": "write_file",
            "args": {"path": "forbidden"},
            "allowlist": ["read_file"]
        })
        .as_object()
        .unwrap()
        .clone();
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;

        let result: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(result["body"]["kind"], "tool.result");
        assert_eq!(result["body"]["id"], "cap-excluded");
        assert_eq!(
            result["body"]["error"],
            "unknown tool `write_file`; available tools: read_file"
        );
        assert!(state.awaiting_approval.is_empty());
        assert!(state.pending.is_empty());
        assert!(
            rx.try_recv().is_err(),
            "excluded tool must not be forwarded"
        );
    }

    #[tokio::test]
    async fn invocation_allowlist_is_a_strict_capability_boundary_in_all_modes() {
        let cases = [
            (
                "empty",
                json!([]),
                GateMode::Safe,
                "unknown tool `read_file`; no tools are available",
            ),
            (
                "malformed-member",
                json!(["read_file", 7]),
                GateMode::Safe,
                "must be an array containing only tool-name strings",
            ),
            (
                "yolo-excluded",
                json!(["write_file"]),
                GateMode::Yolo,
                "unknown tool `read_file`; no tools are available",
            ),
            (
                "yolo-malformed",
                json!(["read_file", null]),
                GateMode::Yolo,
                "must be an array containing only tool-name strings",
            ),
        ];

        for (id, allowlist, mode, expected_error) in cases {
            let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
            let mut state = make_state();
            state.mode = mode;
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
                "id": id,
                "name": "read_file",
                "args": {"path": "README.md"},
                "allowlist": allowlist,
                "read_only": true
            })
            .as_object()
            .unwrap()
            .clone();
            invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;

            let result: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
            assert_eq!(result["body"]["kind"], "tool.result", "case {id}");
            assert!(
                result["body"]["error"]
                    .as_str()
                    .is_some_and(|error| error.contains(expected_error)),
                "case {id}: {result}"
            );
            assert!(state.awaiting_approval.is_empty(), "case {id}");
            assert!(state.pending.is_empty(), "case {id}");
            assert!(rx.try_recv().is_err(), "case {id} must not forward");
        }
    }

    #[tokio::test]
    async fn missing_allowlist_preserves_unrestricted_compatibility_and_ignores_read_only() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
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
            "id": "legacy-unrestricted",
            "name": "read_file",
            "args": {"path": "README.md"},
            "read_only": true
        })
        .as_object()
        .unwrap()
        .clone();
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;

        let permission: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(permission["body"]["kind"], "chat.tool.permission_request");
        assert!(permission["body"].get("allowlist").is_none());
        assert!(permission["body"].get("read_only").is_none());
        assert!(state.awaiting_approval.contains_key("legacy-unrestricted"));
    }

    #[tokio::test]
    async fn prompt_request_preserves_invocation_allowlist() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
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
            "id": "cap-read",
            "name": "read_file",
            "args": {"path": "README.md"},
            "allowlist": ["read_file", "read_image"]
        })
        .as_object()
        .unwrap()
        .clone();
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;

        let permission: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(permission["body"]["kind"], "chat.tool.permission_request");
        assert_eq!(
            permission["body"]["allowlist"],
            json!(["read_file", "read_image"])
        );
        assert!(permission["body"].get("read_only").is_none());
    }

    #[tokio::test]
    async fn delayed_approval_preserves_invocation_provenance() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let body = advertise_body(
            "lead-workflow",
            json!([{"name": "mag", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &body, &mut state)
            .await
            .unwrap();
        let _register = rx.recv().await.unwrap();

        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "r-old/cap-1",
            "from": "lead.run-tool",
            "invocation": {
                "session_id": "session-before-switch",
                "run_id": "lead-turn-before-switch",
                "run_scope": "r-old",
                "actor_id": "lead.run-tool",
                "capability_id": "r-old/cap-1",
                "principal": "lead"
            },
            "allowlist": ["mag"],
            "name": "mag",
            "args": {"action": "apply", "file": "build.mag"}
        })
        .as_object()
        .unwrap()
        .clone();
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;
        let permission: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(permission["body"]["kind"], "chat.tool.permission_request");

        // The gate has no session singleton to consult at approval time: the
        // exact validated local invocation context captured above must survive.
        let response = json!({
            "kind": "tool.permission_response",
            "id": "r-old/cap-1",
            "decision": "approve"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_permission_response(&tx, &response, &mut state)
            .await
            .unwrap();
        let forwarded: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(
            forwarded["body"]["invocation"]["session_id"],
            "session-before-switch"
        );
        assert_eq!(forwarded["body"]["invocation"]["principal"], "lead");
        assert_eq!(forwarded["body"]["allowlist"], json!(["mag"]));
        assert_eq!(forwarded["body"]["caller_id"], "r-old/cap-1");
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
            "id": "r7/cap-2",
            "from": "lead.llm",
            "name": "write_file",
            "args": {}
        })
        .as_object()
        .unwrap()
        .clone();
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;

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
        assert_eq!(body["caller_id"], "r7/cap-2");
        assert_eq!(body["from"], "lead.llm");
        assert!(body.get("invocation").is_none());
        assert_eq!(
            state.pending.get(inner_id).map(|p| p.outer_id.as_str()),
            Some("r7/cap-2")
        );
    }

    async fn unknown_tool_error(state: &mut GateState, invoke: Value) -> String {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let invoke = invoke.as_object().unwrap().clone();
        handle_tool_invoke(&tx, &invoke, state).await.unwrap();
        let result: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(result["body"]["kind"], "tool.result");
        assert_eq!(result["body"]["id"], invoke["id"]);
        assert!(
            rx.try_recv().is_err(),
            "an unusable tool is never forwarded"
        );
        assert!(state.awaiting_approval.is_empty());
        assert!(state.pending.is_empty());
        result["body"]["error"].as_str().unwrap().to_owned()
    }

    async fn state_with_two_sources() -> GateState {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        for (source, tools) in [
            (
                "basic-tools",
                json!([
                    {"name": "write_file", "description": "", "parameters": {}},
                    {"name": "read_file", "description": "", "parameters": {}}
                ]),
            ),
            (
                "lead-workflow",
                json!([{"name": "mag-apply", "description": "", "parameters": {}}]),
            ),
        ] {
            handle_tools_advertise(&tx, &advertise_body(source, tools), &mut state)
                .await
                .unwrap();
            let _register = rx.recv().await.unwrap();
        }
        state
    }

    #[tokio::test]
    async fn unknown_tool_without_allowlist_lists_the_registry_in_register_order() {
        let mut state = state_with_two_sources().await;
        let registered: Vec<String> = tool_register_body(&state)["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(registered, ["mag-apply", "read_file", "write_file"]);

        let error = unknown_tool_error(
            &mut state,
            json!({"kind": "tool-gate.tool.invoke", "id": "prov-3", "name": "bash", "args": {}}),
        )
        .await;
        assert_eq!(
            error,
            "unknown tool `bash`; available tools: mag-apply, read_file, write_file"
        );
    }

    #[tokio::test]
    async fn unknown_tool_with_nothing_registered_says_no_tools_are_available() {
        let mut state = make_state();
        let error = unknown_tool_error(
            &mut state,
            json!({"kind": "tool-gate.tool.invoke", "id": "prov-3", "name": "bash", "args": {}}),
        )
        .await;
        assert_eq!(error, "unknown tool `bash`; no tools are available");
    }

    #[tokio::test]
    async fn unusable_tool_lists_the_allowlist_as_offered_to_the_caller() {
        let mut state = state_with_two_sources().await;
        // `bash` is allowlisted but advertised by no source, so the model was
        // never offered it; `mag-apply` exists but is outside the allowlist.
        let allowlist = json!(["write_file", "bash", "read_file", "write_file"]);
        let expected = "available tools: write_file, read_file";
        for (id, name) in [
            ("stale", "bash"),
            ("nowhere", "shell"),
            ("excluded", "mag-apply"),
        ] {
            let error = unknown_tool_error(
                &mut state,
                json!({
                    "kind": "tool-gate.tool.invoke", "id": id, "name": name, "args": {},
                    "allowlist": allowlist
                }),
            )
            .await;
            assert_eq!(error, format!("unknown tool `{name}`; {expected}"));
        }
    }

    #[tokio::test]
    async fn unusable_tool_lists_request_local_tools_after_the_allowlist() {
        let mut state = state_with_two_sources().await;
        let error = unknown_tool_error(
            &mut state,
            json!({
                "kind": "tool-gate.tool.invoke", "id": "typed", "name": "mag-apply", "args": {},
                "allowlist": ["read_file"],
                "request_local_tools": ["write_output", "submit_output"]
            }),
        )
        .await;
        assert_eq!(
            error,
            "unknown tool `mag-apply`; available tools: read_file, write_output, submit_output"
        );

        let malformed = unknown_tool_error(
            &mut state,
            json!({
                "kind": "tool-gate.tool.invoke", "id": "malformed", "name": "read_file",
                "args": {}, "allowlist": ["read_file"], "request_local_tools": "write_output"
            }),
        )
        .await;
        assert_eq!(
            malformed,
            "tool.invoke field `request_local_tools` must be an array containing only tool-name strings"
        );
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
            "id": "r9/cap-7",
            "from": "worker.run-tool",
            "name": "read_file",
            "args": {"path": "/x"}
        })
        .as_object()
        .unwrap()
        .clone();
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;
        let _request = rx.recv().await.unwrap();

        let response = json!({
            "kind": "tool.permission_response",
            "id": "r9/cap-7",
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
        assert_eq!(body["caller_id"], "r9/cap-7");
        assert_eq!(body["from"], "worker.run-tool");
        assert!(!state.awaiting_approval.contains_key("r9/cap-7"));
    }

    #[tokio::test]
    async fn approval_can_override_forwarded_args() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let body = advertise_body(
            "basic-tools",
            json!([{"name": "patch_file", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &body, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await;

        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-8",
            "name": "patch_file",
            "args": {"path": "/x", "old_string": "a", "new_string": "b"}
        })
        .as_object()
        .unwrap()
        .clone();
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;
        let _request = rx.recv().await.unwrap();

        let response = json!({
            "kind": "tool.permission_response",
            "id": "prov-8",
            "decision": "approve",
            "args": {
                "path": "/x",
                "old_string": "a",
                "new_string": "B"
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
        assert_eq!(args["new_string"], json!("B"));
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
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;
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
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;
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
                name: "read_file".into(),
            },
        );
        let body = json!({
            "kind": "tool.result",
            "id": "gate-1",
            "error": "await failed",
            "error_code": "await_run_failed",
            "status": "failed",
            "run_id": "mag-run-rg-1-2-3",
            "terminal": {"metadata": "preserved"}
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_result(&tx, &body, &mut state).await.unwrap();
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(body.get("id").and_then(Value::as_str), Some("prov-42"));
        assert_eq!(
            body.get("error_code").and_then(Value::as_str),
            Some("await_run_failed")
        );
        assert_eq!(body.get("status").and_then(Value::as_str), Some("failed"));
        assert_eq!(body["run_id"], "mag-run-rg-1-2-3");
        assert_eq!(body["terminal"]["metadata"], "preserved");
        assert!(!state.pending.contains_key("gate-1"));
    }

    #[tokio::test]
    async fn auto_forward_round_trip_preserves_caller_and_result_correlation() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let advertise = advertise_body(
            "lead-workflow",
            json!([{"name": "write_file", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await;
        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "r7/cap-3",
            "from": "lead.llm",
            "invocation": {
                "session_id": "session-a",
                "run_id": "lead-turn-a",
                "run_scope": "r7",
                "actor_id": "lead.llm",
                "capability_id": "r7/cap-3",
                "principal": "lead"
            },
            "name": "write_file",
            "args": {}
        })
        .as_object()
        .unwrap()
        .clone();
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;
        let forwarded: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(forwarded["body"]["id"], "gate-1");
        assert_eq!(forwarded["body"]["caller_id"], "r7/cap-3");
        assert_eq!(forwarded["body"]["from"], "lead.llm");
        assert_eq!(forwarded["body"]["invocation"]["session_id"], "session-a");
        assert_eq!(forwarded["body"]["invocation"]["principal"], "lead");

        let source_result = json!({
            "kind": "tool.result",
            "id": "gate-1",
            "output": {"status": "executing", "run_id": "mag-run-1"},
            "completion_delivery": "async"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_tool_result(&tx, &source_result, &mut state)
            .await
            .unwrap();
        let returned: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(returned["body"]["id"], "r7/cap-3");
        assert_eq!(returned["body"]["name"], "write_file");
        assert_eq!(returned["body"]["output"]["run_id"], "mag-run-1");
        assert_eq!(returned["body"]["completion_delivery"], "async");
    }

    #[tokio::test]
    async fn concurrent_results_preserve_outer_correlation_in_reverse_order() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(16);
        let mut state = make_state();
        let advertise = advertise_body(
            "basic-tools",
            json!([{"name": "write_file", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await;

        for outer in ["r7/cap-1", "r7/cap-2"] {
            let invoke = json!({
                "kind": "tool-gate.tool.invoke",
                "id": outer,
                "name": "write_file",
                "args": {}
            })
            .as_object()
            .unwrap()
            .clone();
            invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;
        }
        let first: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        let second: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        let first_inner = first["body"]["id"].as_str().unwrap();
        let second_inner = second["body"]["id"].as_str().unwrap();

        for (inner, output) in [(second_inner, "second"), (first_inner, "first")] {
            let result = json!({"kind": "tool.result", "id": inner, "output": output})
                .as_object()
                .unwrap()
                .clone();
            handle_tool_result(&tx, &result, &mut state).await.unwrap();
        }
        let second_out: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        let first_out: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(second_out["body"]["id"], "r7/cap-2");
        assert_eq!(second_out["body"]["output"], "second");
        assert_eq!(first_out["body"]["id"], "r7/cap-1");
        assert_eq!(first_out["body"]["output"], "first");
    }

    #[tokio::test]
    async fn forwarded_cancel_settles_once_and_drops_late_result() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let advertise = advertise_body(
            "basic-tools",
            json!([{"name": "shell.script", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await;

        state.mode = GateMode::Yolo;
        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "r16/cap-2",
            "name": "shell.script",
            "args": { "command": "sleep 10" }
        })
        .as_object()
        .unwrap()
        .clone();
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;
        let forwarded: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        let inner_id = forwarded["body"]["id"].as_str().unwrap().to_owned();

        let cancel = json!({"kind": "tool-gate.tool.cancel", "id": "r16/cap-2"})
            .as_object()
            .unwrap()
            .clone();
        handle_tool_cancel(&tx, &cancel, &mut state).await.unwrap();

        let source_cancel: Value =
            serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(source_cancel["body"]["kind"], "basic-tools.tool.cancel");
        assert_eq!(source_cancel["body"]["id"], inner_id);
        let settlement: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(settlement["body"]["kind"], "tool.result");
        assert_eq!(settlement["body"]["id"], "r16/cap-2");
        assert_eq!(settlement["body"]["error"], "tool invocation canceled");
        assert!(state.pending.is_empty());
        assert!(state.awaiting_approval.is_empty());

        let late = json!({"kind": "tool.result", "id": inner_id, "output": "late"})
            .as_object()
            .unwrap()
            .clone();
        handle_tool_result(&tx, &late, &mut state).await.unwrap();
        handle_tool_cancel(&tx, &cancel, &mut state).await.unwrap();
        drop(tx);
        assert!(
            rx.recv().await.is_none(),
            "late result and duplicate cancel must be silent"
        );
    }

    #[tokio::test]
    async fn cancel_before_permission_drops_stale_approval_and_settles_once() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let advertise = advertise_body(
            "basic-tools",
            json!([{"name": "read_file", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await;

        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "r20/cap-1",
            "name": "read_file",
            "args": {"path": "/x"}
        })
        .as_object()
        .unwrap()
        .clone();
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;
        let permission: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(permission["body"]["kind"], "chat.tool.permission_request");

        let cancel = json!({"kind": "tool-gate.tool.cancel", "id": "r20/cap-1"})
            .as_object()
            .unwrap()
            .clone();
        handle_tool_cancel(&tx, &cancel, &mut state).await.unwrap();
        // The source prepared the call, so it is told to drop it.
        let dropped: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(dropped["body"]["kind"], "basic-tools.tool.cancel");
        let settlement: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(settlement["body"]["id"], "r20/cap-1");
        assert_eq!(settlement["body"]["error"], "tool invocation canceled");
        assert!(state.awaiting_approval.is_empty());
        assert!(state.pending.is_empty());

        let stale_approval = json!({
            "kind": "tool.permission_response",
            "id": "r20/cap-1",
            "decision": "approve"
        })
        .as_object()
        .unwrap()
        .clone();
        handle_permission_response(&tx, &stale_approval, &mut state)
            .await
            .unwrap();
        handle_tool_cancel(&tx, &cancel, &mut state).await.unwrap();
        drop(tx);
        assert!(
            rx.recv().await.is_none(),
            "stale approval and duplicate cancel must be silent"
        );
        assert!(state.awaiting_approval.is_empty());
        assert!(state.pending.is_empty());
    }

    #[tokio::test]
    async fn result_before_cancel_settles_once() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        state.pending.insert(
            "gate-1".into(),
            PendingForward {
                outer_id: "r21/cap-1".into(),
                source: "basic-tools".into(),
                name: "shell.script".into(),
            },
        );
        let result = json!({"kind": "tool.result", "id": "gate-1", "output": "done"})
            .as_object()
            .unwrap()
            .clone();
        handle_tool_result(&tx, &result, &mut state).await.unwrap();
        let settlement: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(settlement["body"]["id"], "r21/cap-1");
        assert_eq!(settlement["body"]["output"], "done");

        let cancel = json!({"kind": "tool-gate.tool.cancel", "id": "r21/cap-1"})
            .as_object()
            .unwrap()
            .clone();
        handle_tool_cancel(&tx, &cancel, &mut state).await.unwrap();
        handle_tool_result(&tx, &result, &mut state).await.unwrap();
        drop(tx);
        assert!(
            rx.recv().await.is_none(),
            "cancel and duplicate result after settlement must be silent"
        );
        assert!(state.pending.is_empty());
    }

    #[tokio::test]
    async fn tool_cancel_for_unknown_outer_id_is_a_noop() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let cancel = json!({ "kind": "tool-gate.tool.cancel", "id": "nope" })
            .as_object()
            .unwrap()
            .clone();
        handle_tool_cancel(&tx, &cancel, &mut state).await.unwrap();
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
    async fn yolo_runs_a_default_callers_unsure_and_denied_calls() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        // Default `prompt` policy + a per-tool `deny` to prove yolo wins.
        let mut policy = Policy::new(Decision::Prompt);
        policy.set("shell.script", Decision::Deny);
        let mut state = GateState::new(policy);
        state.mode = GateMode::Yolo;
        let advertise = advertise_body(
            "basic-tools",
            json!([
                {"name": "read_file", "description": "", "parameters": {}},
                {"name": "shell.script", "description": "", "parameters": {}},
            ]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await; // tool.register

        for (id, name) in [("y-1", "read_file"), ("y-2", "shell.script")] {
            let invoke = json!({
                "kind": "tool-gate.tool.invoke",
                "id": id,
                "name": name,
                "args": {}
            })
            .as_object()
            .unwrap()
            .clone();
            invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;
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
    async fn set_mode_auto_emits_mode_changed_and_runs_an_unsure_call() {
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
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("basic-tools.tool.invoke"),
            "auto mode runs a call that is only unsure instead of asking"
        );
        assert!(state.awaiting_approval.is_empty());
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
            json!([{"name": "shell.script", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _register = rx.recv().await.unwrap();

        // First invoke: bash(pwd).
        let first = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-pwd",
            "name": "shell.script",
            "args": { "command": "pwd" },
        })
        .as_object()
        .unwrap()
        .clone();
        invoke_unassessed(&tx, &mut rx, &first, &mut state).await;
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
            "name": "shell.script",
            "args": { "command": "ls -la" },
        })
        .as_object()
        .unwrap()
        .clone();
        invoke_unassessed(&tx, &mut rx, &second, &mut state).await;
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
        policy.set("shell.script", Decision::Deny);
        let mut state = GateState::new(policy);
        let advertise = advertise_body(
            "basic-tools",
            json!([{"name": "shell.script", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _ = rx.recv().await;

        let invoke = json!({
            "kind": "tool-gate.tool.invoke",
            "id": "prov-x",
            "name": "shell.script",
            "args": {}
        })
        .as_object()
        .unwrap()
        .clone();
        invoke_unassessed(&tx, &mut rx, &invoke, &mut state).await;
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg.to_line()).unwrap();
        let body = v.get("body").unwrap();
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("tool.result")
        );
        let err = body.get("error").and_then(Value::as_str).unwrap();
        assert_eq!(
            err,
            "tool `shell.script` was not run: the gate is configured to deny this tool"
        );
        assert!(rx.try_recv().is_err(), "a denied tool is not even prepared");
        assert!(state.pending.is_empty());
        assert!(state.awaiting_approval.is_empty());
    }
    // ---- approval policy, the decision table, and the prepare step ---------

    const TOOLS: &str = "tools";

    /// A gate with one source advertising `tool`, in `mode`.
    async fn gate_with_tool(
        mode: GateMode,
        setting: Decision,
    ) -> (
        GateState,
        mpsc::Sender<PluginOutgoing>,
        mpsc::Receiver<PluginOutgoing>,
    ) {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(16);
        let mut policy = Policy::new(Decision::Prompt);
        policy.set("tool", setting);
        let mut state = GateState::new(policy);
        state.mode = mode;
        let advertise = advertise_body(
            TOOLS,
            json!([{"name": "tool", "description": "", "parameters": {}}]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let _register = rx.recv().await.unwrap();
        (state, tx, rx)
    }

    fn call(id: &str, policy: Option<&str>) -> Map<String, Value> {
        let mut invoke = json!({
            "kind": "tool-gate.tool.invoke", "id": id, "from": "worker.run-tool",
            "name": "tool", "args": {"x": 1}, "allowlist": ["tool"],
            "invocation": {"run_id": "run-1"}
        });
        if let Some(policy) = policy {
            invoke["approval_policy"] = json!(policy);
        }
        invoke.as_object().unwrap().clone()
    }

    fn next(rx: &mut mpsc::Receiver<PluginOutgoing>) -> Value {
        wire_body(rx.try_recv().expect("the gate emitted a message"))
    }

    /// Invoke `tool` and answer its prepare request with `answer`; returns
    /// the prepare request.
    async fn invoke_assessed(
        state: &mut GateState,
        tx: &mpsc::Sender<PluginOutgoing>,
        rx: &mut mpsc::Receiver<PluginOutgoing>,
        policy: Option<&str>,
        answer: Value,
    ) -> Value {
        handle_tool_invoke(tx, &call("outer-1", policy), state)
            .await
            .unwrap();
        let prepare = next(rx);
        assert_eq!(prepare["kind"], "tools.tool.prepare");
        assert!(
            rx.try_recv().is_err(),
            "nothing else happens before the source answers"
        );
        answer_prepared(tx, state, &prepare, answer).await;
        prepare
    }

    #[tokio::test]
    async fn the_callers_policy_is_forwarded_on_prepare_and_on_invoke() {
        for (stated, forwarded) in [
            (Some("ReadOnly"), "ReadOnly"),
            (Some("Default"), "Default"),
            // A caller that states no policy gets the configured behaviour.
            (None, "Default"),
        ] {
            let (mut state, tx, mut rx) = gate_with_tool(GateMode::Safe, Decision::Prompt).await;
            let prepare = invoke_assessed(
                &mut state,
                &tx,
                &mut rx,
                stated,
                json!({"decision": "safe"}),
            )
            .await;
            assert_eq!(prepare["approval_policy"], forwarded);
            assert_eq!(prepare["name"], "tool");
            assert_eq!(prepare["args"], json!({"x": 1}));
            assert_eq!(prepare["caller_id"], "outer-1");
            assert_eq!(prepare["from"], "worker.run-tool");
            assert_eq!(prepare["allowlist"], json!(["tool"]));
            assert_eq!(prepare["invocation"]["run_id"], "run-1");
            let invoke = next(&mut rx);
            assert_eq!(invoke["kind"], "tools.tool.invoke");
            assert_eq!(invoke["approval_policy"], forwarded);
            assert_eq!(
                invoke["id"], prepare["id"],
                "execute runs the call that was prepared"
            );
            assert_eq!(invoke["args"], json!({"x": 1}));
            assert!(invoke.get("workflow_node").is_none());
        }
    }

    #[tokio::test]
    async fn an_unknown_policy_is_rejected_before_the_source_hears_of_the_call() {
        let (mut state, tx, mut rx) = gate_with_tool(GateMode::Yolo, Decision::Auto).await;
        for policy in [
            json!("Rules"),
            json!("readonly"),
            json!({"rules": {}}),
            json!(true),
        ] {
            let mut invoke = call("outer-1", None);
            invoke.insert("approval_policy".into(), policy);
            handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();
            let result = next(&mut rx);
            assert_eq!(result["kind"], "tool.result");
            assert_eq!(
                result["error"],
                "tool.invoke field `approval_policy` must be `ReadOnly` or `Default`"
            );
            assert!(rx.try_recv().is_err());
            assert!(state.pending.is_empty() && state.preparing.is_empty());
        }
    }

    /// Every cell of the table, through the gate: what the source and the
    /// caller observe for each policy, mode and assessment.
    #[tokio::test]
    async fn the_decision_table_holds_for_every_policy_mode_and_assessment() {
        #[derive(Debug, Clone, Copy, PartialEq)]
        enum Seen {
            Runs,
            Asks,
            Denied,
        }
        use Seen::{Asks, Denied, Runs};
        let table = [
            ("ReadOnly", GateMode::Safe, "safe", Runs),
            ("ReadOnly", GateMode::Auto, "safe", Runs),
            ("ReadOnly", GateMode::Yolo, "safe", Runs),
            ("ReadOnly", GateMode::Safe, "unsure", Denied),
            ("ReadOnly", GateMode::Auto, "unsure", Denied),
            ("ReadOnly", GateMode::Yolo, "unsure", Denied),
            ("ReadOnly", GateMode::Safe, "denied", Denied),
            ("ReadOnly", GateMode::Auto, "denied", Denied),
            ("ReadOnly", GateMode::Yolo, "denied", Denied),
            ("Default", GateMode::Safe, "safe", Runs),
            ("Default", GateMode::Auto, "safe", Runs),
            ("Default", GateMode::Yolo, "safe", Runs),
            ("Default", GateMode::Safe, "unsure", Asks),
            ("Default", GateMode::Auto, "unsure", Runs),
            ("Default", GateMode::Yolo, "unsure", Runs),
            ("Default", GateMode::Safe, "denied", Denied),
            ("Default", GateMode::Auto, "denied", Denied),
            ("Default", GateMode::Yolo, "denied", Runs),
        ];
        for (policy, mode, assessment, expected) in table {
            let case = format!("{policy} / {mode:?} / {assessment}");
            // The configured setting says `auto`: a tool's own assessment wins.
            let (mut state, tx, mut rx) = gate_with_tool(mode, Decision::Auto).await;
            let prepare = invoke_assessed(
                &mut state,
                &tx,
                &mut rx,
                Some(policy),
                json!({"decision": assessment, "reason": "it changes files"}),
            )
            .await;
            let first = next(&mut rx);
            let seen = match first["kind"].as_str().unwrap() {
                "tools.tool.invoke" => Runs,
                "chat.tool.permission_request" => Asks,
                "tool.result" => Denied,
                other => panic!("{case}: unexpected {other}"),
            };
            assert_eq!(seen, expected, "{case}");
            match seen {
                Runs => {
                    assert_eq!(first["id"], prepare["id"], "{case}");
                    assert!(rx.try_recv().is_err(), "{case}");
                }
                Asks => {
                    assert_eq!(first["id"], "outer-1", "{case}");
                    assert_eq!(first["tool"], "tool", "{case}");
                    assert_eq!(first["approval_policy"], policy, "{case}");
                    assert_eq!(first["reason"], "it changes files", "{case}");
                    assert!(state.awaiting_approval.contains_key("outer-1"), "{case}");
                    assert!(
                        rx.try_recv().is_err(),
                        "{case}: nothing runs while the user is asked"
                    );
                }
                Denied => {
                    assert_eq!(first["id"], "outer-1", "{case}");
                    assert!(first.get("output").is_none(), "{case}");
                    // The source drops the prepared call; it is never invoked.
                    let dropped = next(&mut rx);
                    assert_eq!(dropped["kind"], "tools.tool.cancel", "{case}");
                    assert_eq!(dropped["id"], prepare["id"], "{case}");
                    assert!(
                        rx.try_recv().is_err(),
                        "{case}: a denied call never reaches execute"
                    );
                    assert!(state.pending.is_empty(), "{case}");
                    assert!(state.awaiting_approval.is_empty(), "{case}");
                }
            }
        }
    }

    #[tokio::test]
    async fn the_caller_is_told_why_a_call_was_not_run() {
        let cases = [
            (
                "ReadOnly",
                json!({"decision": "denied", "reason": "it writes a file"}),
                "tool `tool` was not run: your approval policy is ReadOnly and it writes a file",
            ),
            (
                "ReadOnly",
                json!({"decision": "unsure", "reason": "not shown to be read-only"}),
                "tool `tool` was not run: your approval policy is ReadOnly and not shown to be read-only",
            ),
            (
                "ReadOnly",
                json!({"decision": "unsure"}),
                "tool `tool` was not run: your approval policy is ReadOnly and this call could not be shown to be read-only",
            ),
            (
                "Default",
                json!({"decision": "denied", "reason": "write_file requires an approved plan"}),
                "tool `tool` was not run: write_file requires an approved plan",
            ),
            (
                "Default",
                json!({"decision": "denied"}),
                "tool `tool` was not run: this call is not allowed",
            ),
            (
                "Default",
                json!({"decision": "approve"}),
                "tool `tool` was not run: its source answered with an unknown decision",
            ),
        ];
        for (policy, answer, expected) in cases {
            let (mut state, tx, mut rx) = gate_with_tool(GateMode::Safe, Decision::Auto).await;
            invoke_assessed(&mut state, &tx, &mut rx, Some(policy), answer).await;
            assert_eq!(next(&mut rx)["error"], expected);
        }
    }

    #[tokio::test]
    async fn a_tool_that_assesses_nothing_follows_the_configured_setting_under_default_only() {
        let cases = [
            ("Default", Decision::Auto, "tools.tool.invoke"),
            ("Default", Decision::Prompt, "chat.tool.permission_request"),
            ("ReadOnly", Decision::Auto, "tool.result"),
            ("ReadOnly", Decision::Prompt, "tool.result"),
        ];
        for (policy, setting, expected) in cases {
            let (mut state, tx, mut rx) = gate_with_tool(GateMode::Safe, setting).await;
            invoke_assessed(&mut state, &tx, &mut rx, Some(policy), json!({})).await;
            let first = next(&mut rx);
            assert_eq!(first["kind"], expected, "{policy} / {setting:?}");
            if policy == "ReadOnly" {
                assert_eq!(
                    first["error"],
                    "tool `tool` was not run: your approval policy is ReadOnly and the tool does not say what it does under this policy"
                );
            }
        }
        // Yolo does not widen a ReadOnly caller of a tool that says nothing.
        let (mut state, tx, mut rx) = gate_with_tool(GateMode::Yolo, Decision::Auto).await;
        invoke_assessed(&mut state, &tx, &mut rx, Some("ReadOnly"), json!({})).await;
        assert_eq!(next(&mut rx)["kind"], "tool.result");
    }

    #[tokio::test]
    async fn a_denied_setting_overrides_the_tools_own_assessment() {
        // Safe and auto: denied at once, the source never hears of the call.
        for (policy, mode) in [
            ("Default", GateMode::Safe),
            ("Default", GateMode::Auto),
            ("ReadOnly", GateMode::Yolo),
        ] {
            let (mut state, tx, mut rx) = gate_with_tool(mode, Decision::Deny).await;
            handle_tool_invoke(&tx, &call("outer-1", Some(policy)), &mut state)
                .await
                .unwrap();
            let result = next(&mut rx);
            assert_eq!(result["kind"], "tool.result", "{policy} / {mode:?}");
            assert!(result["error"]
                .as_str()
                .unwrap()
                .ends_with("the gate is configured to deny this tool"));
            assert!(rx.try_recv().is_err());
        }
        // Default in yolo runs denied calls, so the call is prepared and run.
        let (mut state, tx, mut rx) = gate_with_tool(GateMode::Yolo, Decision::Deny).await;
        invoke_assessed(
            &mut state,
            &tx,
            &mut rx,
            Some("Default"),
            json!({"decision": "safe"}),
        )
        .await;
        assert_eq!(next(&mut rx)["kind"], "tools.tool.invoke");
    }

    #[tokio::test]
    async fn an_approved_call_executes_the_prepared_call_and_a_refused_one_is_dropped() {
        let (mut state, tx, mut rx) = gate_with_tool(GateMode::Safe, Decision::Auto).await;
        let prepare = invoke_assessed(
            &mut state,
            &tx,
            &mut rx,
            Some("Default"),
            json!({"decision": "unsure"}),
        )
        .await;
        assert_eq!(next(&mut rx)["kind"], "chat.tool.permission_request");
        let approve =
            json!({"kind": "tool.permission_response", "id": "outer-1", "decision": "approve"});
        handle_permission_response(&tx, approve.as_object().unwrap(), &mut state)
            .await
            .unwrap();
        let invoke = next(&mut rx);
        assert_eq!(invoke["kind"], "tools.tool.invoke");
        assert_eq!(invoke["id"], prepare["id"]);
        assert_eq!(invoke["approval_policy"], "Default");

        let (mut state, tx, mut rx) = gate_with_tool(GateMode::Safe, Decision::Auto).await;
        let prepare = invoke_assessed(
            &mut state,
            &tx,
            &mut rx,
            Some("Default"),
            json!({"decision": "unsure"}),
        )
        .await;
        let _request = next(&mut rx);
        let refuse =
            json!({"kind": "tool.permission_response", "id": "outer-1", "decision": "deny"});
        handle_permission_response(&tx, refuse.as_object().unwrap(), &mut state)
            .await
            .unwrap();
        assert_eq!(next(&mut rx)["error"], "tool `tool` denied by user");
        let dropped = next(&mut rx);
        assert_eq!(dropped["kind"], "tools.tool.cancel");
        assert_eq!(dropped["id"], prepare["id"]);
        assert!(
            rx.try_recv().is_err(),
            "a refused call never reaches execute"
        );
        assert!(state.pending.is_empty());
    }

    #[tokio::test]
    async fn a_source_may_end_a_call_while_preparing_it() {
        let (mut state, tx, mut rx) = gate_with_tool(GateMode::Yolo, Decision::Auto).await;
        handle_tool_invoke(&tx, &call("outer-1", Some("Default")), &mut state)
            .await
            .unwrap();
        let prepare = next(&mut rx);
        let failed = json!({"kind": "tool.result", "id": prepare["id"], "error": "tool: missing required argument `y`"});
        handle_tool_result(&tx, failed.as_object().unwrap(), &mut state)
            .await
            .unwrap();
        let result = next(&mut rx);
        assert_eq!(result["id"], "outer-1");
        assert_eq!(result["error"], "tool: missing required argument `y`");
        assert!(state.preparing.is_empty() && state.pending.is_empty());
        // A late answer to the prepare request is inert.
        answer_prepared(&tx, &mut state, &prepare, json!({"decision": "safe"})).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancelling_a_call_that_is_being_prepared_drops_it_at_its_source() {
        let (mut state, tx, mut rx) = gate_with_tool(GateMode::Yolo, Decision::Auto).await;
        handle_tool_invoke(&tx, &call("outer-1", Some("Default")), &mut state)
            .await
            .unwrap();
        let prepare = next(&mut rx);
        let cancel = json!({"kind": "tool-gate.tool.cancel", "id": "outer-1"});
        handle_tool_cancel(&tx, cancel.as_object().unwrap(), &mut state)
            .await
            .unwrap();
        let dropped = next(&mut rx);
        assert_eq!(dropped["kind"], "tools.tool.cancel");
        assert_eq!(dropped["id"], prepare["id"]);
        assert_eq!(next(&mut rx)["error"], "tool invocation canceled");
        answer_prepared(&tx, &mut state, &prepare, json!({"decision": "safe"})).await;
        assert!(rx.try_recv().is_err(), "a cancelled call is never executed");
    }

    #[tokio::test]
    async fn a_workflow_nodes_call_is_transported_without_a_decision() {
        // Safe mode and a setting that would ask: neither is consulted.
        for mode in [GateMode::Safe, GateMode::Auto, GateMode::Yolo] {
            let (mut state, tx, mut rx) = gate_with_tool(mode, Decision::Prompt).await;
            let mut invoke = call("node-1", None);
            invoke.insert("workflow_node".into(), json!(true));
            handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();
            let forwarded = next(&mut rx);
            assert_eq!(forwarded["kind"], "tools.tool.invoke", "{mode:?}");
            assert_eq!(forwarded["workflow_node"], true);
            assert!(forwarded.get("approval_policy").is_none());
            assert_eq!(forwarded["caller_id"], "node-1");
            assert!(rx.try_recv().is_err(), "no prepare request and no question");
            assert!(state.preparing.is_empty() && state.awaiting_approval.is_empty());
        }
        // The marker is exactly `true`; anything else is an ordinary call.
        let (mut state, tx, mut rx) = gate_with_tool(GateMode::Safe, Decision::Prompt).await;
        let mut invoke = call("node-2", None);
        invoke.insert("workflow_node".into(), json!("true"));
        handle_tool_invoke(&tx, &invoke, &mut state).await.unwrap();
        assert_eq!(next(&mut rx)["kind"], "tools.tool.prepare");
    }

    #[tokio::test]
    async fn per_policy_descriptions_reach_the_public_registry_unchanged() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let mut state = make_state();
        let advertise = advertise_body(
            TOOLS,
            json!([
                {"name": "write", "description": "Writes.", "parameters": {},
                 "policy_descriptions": {"ReadOnly": "Rejected for you."}},
                {"name": "read", "description": "Reads.", "parameters": {}},
                {"name": "empty", "description": "Says nothing.", "parameters": {},
                 "policy_descriptions": {}}
            ]),
        );
        handle_tools_advertise(&tx, &advertise, &mut state)
            .await
            .unwrap();
        let register = next(&mut rx);
        let tools = register["tools"].as_array().unwrap();
        let by_name = |name: &str| tools.iter().find(|tool| tool["name"] == name).unwrap();
        assert_eq!(
            by_name("write")["policy_descriptions"],
            json!({"ReadOnly": "Rejected for you."})
        );
        assert_eq!(
            by_name("write")["description"],
            "Writes.",
            "the gate appends nothing"
        );
        assert!(by_name("read").get("policy_descriptions").is_none());
        assert!(by_name("empty").get("policy_descriptions").is_none());

        for (bad, expected) in [
            (json!({"Rules": "x"}), "unknown approval policy `Rules`"),
            (
                json!({"ReadOnly": ""}),
                "text for `ReadOnly` must be a non-empty string",
            ),
            (json!("text"), "must be an object"),
        ] {
            let advertise = advertise_body(
                "other",
                json!([{"name": "odd", "description": "", "parameters": {},
                        "policy_descriptions": bad}]),
            );
            handle_tools_advertise(&tx, &advertise, &mut state)
                .await
                .unwrap();
            let error = next(&mut rx);
            assert_eq!(error["kind"], "tool-gate.advertise_error");
            assert!(
                error["message"].as_str().unwrap().contains(expected),
                "{error}"
            );
        }
    }
}
