// mag — NCP v0.1 plugin hosting the MAG actor-kernel runtime.
//
// Completes the NCP ready handshake, hosts a Lua VM that loads the kernel from
// the config-resolved Lua path, and answers the mag protocol: `mag.ping`
// (liveness), `mag.load` (compile and return an immutable versioned artifact),
// `mag.execute` (run an inline immutable program artifact —
// register the constellation, deliver its initial messages (actors construct
// lazily at first firing), stream lifecycle events, and reply
// `mag.run_result` with the sink's final result + output path).
//
// Layering mirrors the sibling plugins (`reasoner-graph`, `tool-gate`):
// - `main.rs` — entry, handshake, dispatch loop, execute drive, bus encoding.
// - `kernel.rs` — the embedded Lua VM, host bindings, and kernel driving.
// - `error.rs` — `MagError` domain error hierarchy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use nefor_plugin_sdk::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, TransportError};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::bridge::CapabilityBridge;
use crate::error::MagError;
use crate::kernel::{ExecutionModelSnapshot, LuaHost, RunCompletion, TeardownReason};

/// A run driven asynchronously to completion: the execute reply is deferred
/// until that run signals `mag.run_complete` or `mag.run_failed` (via inbound
/// capability responses that unblock deferred activations). Runs are
/// concurrent — each `mag.execute` gets its own run-scoped kernel context and
/// its own entry here, keyed by run_id, settling independently. Synchronous
/// programs (the shipped factories) finish inside the `mag.execute` call and
/// never register one.
struct ActiveExecute {
    /// The `mag.execute` request id to correlate the terminal reply to.
    in_reply_to: Option<String>,
    /// Monotonic start of accepted run execution, retained across every apply.
    started_at: Instant,
}

/// The in-flight async runs, keyed by run_id.
type ActiveExecutes = HashMap<String, ActiveExecute>;

/// Outbound/inbound channel capacity for the stdio transport tasks.
const CHANNEL_CAP: usize = 128;

/// NCP version this plugin speaks.
const PROTOCOL_VERSION: &str = "0.1";

/// Plugin name (bus identity is assigned by the engine from spawn-config;
/// this is what we prefix our own event kinds with).
const PLUGIN_NAME: &str = "mag";

/// Plugin version, advertised in `mag.hello`.
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Liveness ping we answer, and the reply kind.
const PING_KIND: &str = "mag.ping";
const PONG_KIND: &str = "mag.pong";

/// Compile a MAG source file and return its immutable versioned envelope.
const LOAD_KIND: &str = "mag.load";
const BUILD_KIND: &str = "mag.build";
const LOADED_KIND: &str = "mag.loaded";
/// Reply kind for compilation or control-plane validation failures.
/// is data on the bus, not a plugin crash (ir.md: "the run continues").
const ERROR_KIND: &str = "mag.error";

/// Run an inline program's initial modification through the
/// kernel: register the constellation, deliver its initial messages (actors
/// construct lazily at first firing), stream lifecycle events, and reply with
/// the run's terminal status.
const EXECUTE_KIND: &str = "mag.execute";

/// Terminal reply for a run: status, the sink's final result INLINE, and the
/// persisted output path. Mid-run events stay paths-and-statuses-only
/// (docs/ir.md, architecture.md §Control plane); the terminal reply is the
/// one envelope whose consumer (the lead) relays the result itself, so it
/// carries the sink's result rather than forcing a read-back through the path.
const RUN_RESULT_KIND: &str = "mag.run_result";

/// Apply one graph modification directly to the kernel — the
/// control plane's direct kernel op (docs/ir.md, "Kernel
/// operations": the control plane reaches `actors`/`kills`/`messages`
/// directly). This is the mid-run control-plane surface the actor-kernel
/// cutover needs: a `{ kills = [...] }` modification is how the plane kills an
/// in-flight actor (its abort/cancel envelope reaches the bus, its correlations
/// drop, its late reply voids). Guarded — the modification must be an object —
/// and acknowledged with `mag.applied` (or `mag.error` on a rejected/ill-shaped
/// modification). Any lifecycle events the apply produced stream on the wire as
/// usual, and a completion it triggers settles the in-flight run.
const APPLY_KIND: &str = "mag.apply";
const APPLIED_KIND: &str = "mag.applied";

/// Kill a live run outright — the control plane's interrupt surface (the
/// TUI's Esc path). Ends the run's kernel context through the fold (kill
/// handlers run, so in-flight capabilities emit cancellation) and settles the
/// pending execute as `mag.run_result status:"killed"`. A kill for a run that is
/// not live is a logged no-op (monotone lifecycles: kill on dead, no-op).
const KILL_RUN_KIND: &str = "mag.kill_run";
const KILL_ALL_RUNS_KIND: &str = "mag.kill_all_runs";
const STEER_RUN_KIND: &str = "mag.steer_run";
const RUN_STEERED_KIND: &str = "mag.run_steered";
const RESUME_ACTOR_KIND: &str = "mag.resume_actor";
const ACTOR_RESUMED_KIND: &str = "mag.actor_resumed";

/// Interrupt a live run — the control plane's graceful-interrupt surface. Carries an
/// optional `terminate` flag selecting the semantics (see `handle_interrupt_run`):
///
/// * `terminate` absent/false — GRACEFUL (the lead's OWN turn): settle every
///   in-flight capability as "interrupted by user" and cancel the real work,
///   then let the run wind down normally — the lead llm re-fires with the
///   interrupted tool result and produces a real final answer, so the turn
///   completes (`status:"completed"`) and history records itself (no amnesia).
///   The run's context stays alive.
/// * `terminate == true` — TERMINATING (a dispatched sub-run): cancel the
///   in-flight work but deliver no reply, then END the run FAILED
///   (`status:"failed"`). The run's llm never re-fires; the failure relays to
///   the lead. A dispatched run is ephemeral, so an interrupt must stop it.
///
/// An interrupt for a run that is not live is a logged no-op.
const INTERRUPT_RUN_KIND: &str = "mag.interrupt_run";

/// The failure detail an interrupt settles in-flight capabilities with.
const INTERRUPT_FAILURE: &str = "interrupted by user";

/// Correlation id echoed by capability responses (tool.result). The kernel
/// mints these on `capability.invoke` (routing.lua); the reply carries `output`
/// (tool/provider convention) or `result`, plus an optional `error`. The tool
/// gate answers its `<gate>.tool.invoke` with exactly this shape, keyed by the
/// caller's id, so gated tool invocations correlate back through this path.
const TOOL_RESULT_KIND: &str = "tool.result";
const BASH_STREAM_KIND: &str = "tool.stream";

/// Fallback tool-gate bus name when the spawn config passes no `--tool-gate`.
/// The gate's name is composition-owned (examples/nefor-agent/init.lua names the gate when
/// spawning it and threads the same name here); this default only keeps a bare
/// `mag-plugin` spawn functional against the shipped starter composition.
const DEFAULT_GATE_TARGET: &str = "tool-gate";

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
        tracing::error!(error = %e, "mag exited with error");
        eprintln!("mag: {e}");
        std::process::exit(1);
    }
    // Force exit: `tokio::io::stdin()` parks a non-cancellable blocking
    // reader thread; letting the runtime drop naturally would hang the
    // process. Same fix as reasoner-graph / nefor-tui.
    std::process::exit(0);
}

async fn run() -> Result<(), MagError> {
    let (out_tx, _writer_handle) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let _reader_handle = spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = await_ready_ok(&mut in_rx).await?;
    tracing::info!(engine_version = %engine_version, "ready");

    // Host the Lua VM and load the kernel before advertising liveness, so
    // `mag.hello` truthfully reports the loaded kernel. `host` is held for
    // the whole session — the VM is the kernel's entire world.
    let lua_root = arg_value("--lua-root").map(PathBuf::from);
    let kernel_path = resolve_kernel_path(lua_root.as_deref())?;
    tracing::info!(path = %kernel_path.display(), "loading mag kernel");
    let host = LuaHost::load_kernel(&kernel_path, lua_root.as_deref())?;

    // Advertise the registry's factory names on hello so the control plane has
    // a validation snapshot from plugin startup — the source of truth for
    // reasoner/factory types on both execute paths (replaces a hand-synced
    // allowlist; docs/ir.md division-of-responsibility).
    let factories = host.registry_names().unwrap_or_default();
    // Registry declarations are startup invariants, not optional diagnostics.
    // Refuse to advertise readiness if their immutable snapshot cannot cross
    // the wire; otherwise actors could spawn before observers know their type.
    let contracts = host.registry_contracts()?;
    send_event(
        &out_tx,
        hello_body(host.kernel_name().as_deref(), &factories, contracts),
    )
    .await?;

    run_dispatch_loop(&out_tx, &mut in_rx, &host).await?;

    let _ = out_tx.send(PluginOutgoing::event(goodbye_body())).await;
    Ok(())
}

/// Resolve where the kernel entry Lua lives, highest precedence first:
///
/// 1. `--kernel <path>` (or `-k`) argv — an explicit override for dev
///    experiments and the plugin's integration tests.
/// 2. `<lua-root>/../plugins/mag/lua/mag-kernel/init.lua` — the plugin's own
///    shipped kernel, the DEFAULT. Located via the composition-threaded
///    `--lua-root` (`NEFOR_ROOT/lua`), whose parent is `NEFOR_ROOT`. That root
///    carries the whole `plugins/` tree in every install mode — dev checkout,
///    `NEFOR_LOCAL_DIR` override, or the pm sparse-clone (its cone includes
///    `plugins`). So configs no longer copy the kernel; the plugin owns it.
/// 3. `NEFOR_DEV_DIR/plugins/mag/lua/mag-kernel/init.lua` — in-checkout dev
///    fallback for a bare spawn that passes no `--lua-root`.
///
/// Mirrors [`set_kernel_path`]'s own lua-root-then-`NEFOR_DEV_DIR` ordering.
fn resolve_kernel_path(lua_root: Option<&Path>) -> Result<PathBuf, MagError> {
    if let Some(path) = arg_value("--kernel").or_else(|| arg_value("-k")) {
        return Ok(PathBuf::from(path));
    }

    if let Some(nefor_root) = lua_root.and_then(Path::parent) {
        let candidate = nefor_root.join("plugins/mag/lua/mag-kernel/init.lua");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    if let Some(dev) = std::env::var_os("NEFOR_DEV_DIR") {
        let candidate = PathBuf::from(dev).join("plugins/mag/lua/mag-kernel/init.lua");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(MagError::NoKernelPath)
}

/// Resolve the tool gate's bus name from `--tool-gate <name>` argv — how the
/// starter threads the composition-owned gate identity into this plugin
/// (mirroring how it names the gate itself: `tools.gate_spec("tool-gate", …)`).
/// Falls back to [`DEFAULT_GATE_TARGET`] when the flag is absent.
fn resolve_gate_target() -> String {
    arg_value("--tool-gate").unwrap_or_else(|| DEFAULT_GATE_TARGET.to_owned())
}

/// The value following a `<flag> <value>` argv pair, if present.
fn arg_value(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

async fn run_dispatch_loop(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    in_rx: &mut mpsc::Receiver<Result<Envelope, TransportError>>,
    host: &LuaHost,
) -> Result<(), MagError> {
    // The in-flight async runs, keyed by run_id (deferred-completion path).
    // Concurrent `mag.execute` requests each hold one entry; each settles
    // independently against its own run-scoped kernel context.
    let mut active: ActiveExecutes = HashMap::new();
    // Capability bridge: correlates provider-class `tool.invoke` requests with
    // provider completions and rewrites tool-class invocations onto the
    // composition-named gate's `<gate>.tool.invoke` contract (bridge.rs).
    let mut bridge = CapabilityBridge::new(resolve_gate_target());
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
                            handle_event(
                                out_tx,
                                env.from.as_str(),
                                map,
                                host,
                                &mut active,
                                &mut bridge,
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
                return Ok(());
            }
        }
    }
}

/// Forward everything the kernel emitted since the last drain (capability
/// requests + lifecycle events) onto the NCP wire, in order. Each drained body
/// passes through the capability bridge first: a provider-class `tool.invoke`
/// becomes a single-shot completion request, a tool-class `tool.invoke` goes to
/// the gate's `<gate>.tool.invoke` contract (bridge.rs); everything else
/// forwards unchanged.
async fn flush_emits(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    host: &LuaHost,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    for body in host.drain_emits()? {
        for envelope in bridge.translate_emit(body) {
            send_event(out_tx, envelope).await?;
        }
    }
    Ok(())
}

/// If the named run signalled a terminal state, send its terminal reply,
/// drop its in-flight slot, and end its run context: completion carries the
/// sink's final result plus its output PATH; an unhandled actor failure (the
/// kernel's `mag.run_failed` escalation) fails the run with the failure
/// detail surfaced. Ending the context reaps the run's remaining live actors
/// through the fold — kill handlers run, so a still-open provider request on
/// a parallel branch is cancelled — and the reap's envelopes are flushed.
/// Other runs are untouched: kill semantics are per run.
async fn settle_run(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    host: &LuaHost,
    active: &mut ActiveExecutes,
    bridge: &mut CapabilityBridge,
    run_id: &str,
) -> Result<(), MagError> {
    if !active.contains_key(run_id) {
        return Ok(());
    }
    flush_emits(out_tx, host, bridge).await?;
    // The teardown reason rides the reap's `mag.actor_killed` events so
    // consumers can tell a completed run's bookkeeping sweep from a real
    // termination.
    let Some(active_run) = active.get(run_id) else {
        return Ok(());
    };
    let duration_ms = elapsed_ms(active_run.started_at);
    let (mut reply, reason) = if let Some(rc) = host.take_run_complete(run_id)? {
        (
            run_result_ok(None, run_id, &rc, duration_ms),
            TeardownReason::RunComplete,
        )
    } else if let Some(error) = host.take_run_failed(run_id)? {
        (
            run_result_failed(None, run_id, &error, duration_ms),
            TeardownReason::RunFailed,
        )
    } else {
        return Ok(());
    };
    let Some(active_run) = active.remove(run_id) else {
        return Ok(());
    };
    if let Some(id) = active_run.in_reply_to {
        reply.insert("in_reply_to".into(), Value::String(id));
    }
    send_event(out_tx, reply).await?;
    host.end_run(run_id, reason)?;
    flush_emits(out_tx, host, bridge).await
}

/// Fail the still-pending execute replies of runs the kernel reaped at a
/// session boundary (begin_run's `reaped` list) and flush the reap's
/// envelopes (actor_killed events, abort/cancel emits).
async fn settle_reaped(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    host: &LuaHost,
    active: &mut ActiveExecutes,
    bridge: &mut CapabilityBridge,
    reaped: &[String],
) -> Result<(), MagError> {
    if reaped.is_empty() {
        return Ok(());
    }
    flush_emits(out_tx, host, bridge).await?;
    for run_id in reaped {
        if let Some(a) = active.remove(run_id) {
            send_event(
                out_tx,
                run_result_failed(
                    a.in_reply_to.as_deref(),
                    run_id,
                    "run reaped at session boundary (a later session began a new run)",
                    elapsed_ms(a.started_at),
                ),
            )
            .await?;
        }
    }
    Ok(())
}

/// Handle one inbound event body. We answer `mag.ping` (liveness), `mag.load`
/// (compile a program or delta and reply with its immutable envelope). Everything else on the
/// broadcast bus is not ours and drops silently.
fn is_provider_diagnostic_event(event: &str) -> bool {
    matches!(
        event,
        "attempt_discarded"
            | "retry"
            | "retry_decision"
            | "usage"
            | "failed"
            | "error"
            | "interrupted"
    )
}

const PROVIDER_TOOL_ERROR_LIMIT: usize = 160;

fn provider_tool_lifecycle_observation(body: &Map<String, Value>) -> Option<Value> {
    let event = body.get("event").and_then(Value::as_str)?;
    if !matches!(
        event,
        "tool_execution_started" | "tool_execution_completed" | "tool_execution_failed"
    ) {
        return None;
    }
    let tool_call_id = body
        .get("tool_call_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())?;
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())?;
    let arguments = body.get("arguments").and_then(Value::as_object)?;
    let mut observation = Map::new();
    observation.insert("kind".into(), Value::String(event.to_owned()));
    observation.insert(
        "tool_call_id".into(),
        Value::String(tool_call_id.to_owned()),
    );
    observation.insert("name".into(), Value::String(name.to_owned()));
    observation.insert("arguments".into(), Value::Object(arguments.clone()));
    match event {
        "tool_execution_completed" => {
            observation.insert("result".into(), body.get("result")?.clone());
        }
        "tool_execution_failed" => {
            let error = body
                .get("error")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())?;
            let mut bounded = error.chars().take(PROVIDER_TOOL_ERROR_LIMIT).collect::<String>();
            if error.chars().count() > PROVIDER_TOOL_ERROR_LIMIT {
                bounded.push('…');
            }
            observation.insert("error".into(), Value::String(bounded));
        }
        _ => {}
    }
    Some(Value::Object(observation))
}

async fn handle_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    source: &str,
    body: &Map<String, Value>,
    host: &LuaHost,
    active: &mut ActiveExecutes,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(k) => k,
        None => return Ok(()),
    };
    // Canonical provider events correlate themselves by request_id. Terminal
    // events settle the kernel capability; streaming and other telemetry remain
    // observational and keep the request open. Unknown and late ids are ignored.
    if let Some(request_id) = bridge
        .provider_request_id(source, kind, body)
        .map(str::to_owned)
    {
        if let Some(event @ ("text_delta" | "reasoning_delta")) =
            body.get("event").and_then(Value::as_str)
        {
            if event != "text_delta" || !bridge.is_structured_request(&request_id) {
                if let Some(text) = body.get("text").and_then(Value::as_str) {
                    let event_kind = if event == "reasoning_delta" {
                        "reasoning"
                    } else {
                        "assistant"
                    };
                    let value = serde_json::json!({ "kind": event_kind, "text": text });
                    let _ = host.bus_observation(&request_id, "append", "transcript", &value)?;
                    flush_emits(out_tx, host, bridge).await?;
                }
            }
        }
        if let Some(observation) = provider_tool_lifecycle_observation(body) {
            let _ = host.bus_observation(
                &request_id,
                "append",
                "conversation",
                &observation,
            )?;
            flush_emits(out_tx, host, bridge).await?;
        }
        if let Some(event) = body
            .get("event")
            .and_then(Value::as_str)
            .filter(|event| is_provider_diagnostic_event(event))
        {
            let mut observation = body.clone();
            observation.insert("kind".into(), Value::String(event.into()));
            observation.remove("request_id");
            observation.remove("event");
            let _ = host.bus_observation(
                &request_id,
                "append",
                "conversation",
                &Value::Object(observation),
            )?;
            flush_emits(out_tx, host, bridge).await?;
        }
        if let Some(reply) = bridge.take_reply(kind, body) {
            return handle_provider_reply(out_tx, reply, host, active, bridge).await;
        }
        return Ok(());
    }
    let in_reply_to = body.get("id").and_then(Value::as_str);
    match kind {
        PING_KIND => send_event(out_tx, pong_body(in_reply_to)).await,
        LOAD_KIND => handle_load(out_tx, body, in_reply_to, host).await,
        BUILD_KIND => handle_build(out_tx, body, in_reply_to, host).await,
        EXECUTE_KIND => {
            handle_execute(
                out_tx,
                source,
                body,
                in_reply_to,
                (host, active, bridge),
            )
            .await
        }
        APPLY_KIND => {
            handle_apply(
                out_tx,
                body,
                in_reply_to,
                host,
                active,
                bridge,
            )
            .await
        }
        KILL_RUN_KIND => handle_kill_run(out_tx, body, host, active, bridge).await,
        KILL_ALL_RUNS_KIND => handle_kill_all_runs(out_tx, host, active, bridge).await,
        STEER_RUN_KIND => handle_steer_run(out_tx, body, host, active).await,
        RESUME_ACTOR_KIND => {
            handle_resume_actor(out_tx, body, host, active, bridge).await
        }
        INTERRUPT_RUN_KIND => {
            handle_interrupt_run(out_tx, body, host, active, bridge).await
        }
        // A capability response correlated to a kernel-minted request id.
        // Unknown ids are dropped inside the kernel (no open correlation), so
        // forwarding every tool.result while any run is live is safe.
        BASH_STREAM_KIND if !active.is_empty() => {
            handle_tool_stream(out_tx, body, host, bridge).await
        }
        TOOL_RESULT_KIND if !active.is_empty() => {
            handle_tool_result(out_tx, body, host, active, bridge).await
        }
        _ => Ok(()),
    }
}

/// Load `source_dir/entry` in-process and reply with its initial modification.
async fn handle_load(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    in_reply_to: Option<&str>,
    host: &LuaHost,
) -> Result<(), MagError> {
    let source_dir = match body.get("source_dir").and_then(Value::as_str) {
        Some(s) => s,
        None => return send_event(out_tx, error_body(in_reply_to, "mag.load missing source_dir")).await,
    };
    let entry = match body.get("entry").and_then(Value::as_str) {
        Some(s) => s,
        None => return send_event(out_tx, error_body(in_reply_to, "mag.load missing entry")).await,
    };
    let module_roots = match load_module_roots(body, Path::new(source_dir)) {
        Ok(roots) => roots,
        Err(error) => return send_event(out_tx, error_body(in_reply_to, &error)).await,
    };
    let contracts = host.registry_contracts().unwrap_or_else(|_| Value::Array(Vec::new()));
    let inputs = serde_json::json!({ "factory_contracts": contracts });
    match nefor_mag::compile_file_with_inputs_and_module_roots(
        Path::new(source_dir), entry, inputs, &module_roots,
    ) {
        Ok(artifact) => finish_compile(out_tx, in_reply_to, host, artifact, None).await,
        Err(e) => send_event(out_tx, mag_error_body(in_reply_to, &e)).await,
    }
}

async fn finish_compile(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    in_reply_to: Option<&str>,
    host: &LuaHost,
    artifact: Value,
    build: Option<Value>,
) -> Result<(), MagError> {
    let hash = match immutable_artifact_hash(&artifact) {
        Ok(hash) => hash,
        Err(error) => return send_event(out_tx, error_body(in_reply_to, &error)).await,
    };
    let decoded = match artifact.get("kind").and_then(Value::as_str) {
        Some("program") => match artifact_program(&artifact) {
            Ok(decoded) => {
                let preflight = host.preflight_program(&decoded.initial, &decoded.operations)?;
                if !preflight.ok {
                    return send_event(out_tx, error_body(in_reply_to,
                        preflight.error.as_deref().unwrap_or("artifact preflight failed"))).await;
                }
                decoded
            }
            Err(error) => return send_event(out_tx, error_body(in_reply_to, &error)).await,
        },
        Some("delta") => match artifact_delta(&artifact) {
            Ok(delta) => DecodedProgram { initial: delta, operations: Vec::new() },
            Err(error) => return send_event(out_tx, error_body(in_reply_to, &error)).await,
        },
        _ => return send_event(out_tx, error_body(in_reply_to,
            "MAG requires a nefor.mag program or delta envelope")).await,
    };
    if let Err(error) = preflight_output_schemas(&decoded) {
        return send_event(out_tx, error_body(in_reply_to, &error)).await;
    }
    let factories = host.registry_names().unwrap_or_default();
    let contracts = host.registry_contracts().unwrap_or_else(|_| Value::Array(Vec::new()));
    let mut reply = loaded_body(in_reply_to, &hash, artifact, &factories, contracts);
    if let Some(build) = build {
        reply.insert("build".into(), build);
    }
    send_event(out_tx, reply).await
}

#[derive(serde::Deserialize)]
struct BuildRequest {
    project_root: PathBuf,
    entry: String,
    #[serde(default)]
    module_roots: Vec<PathBuf>,
    cache_dir: PathBuf,
    #[serde(default)]
    no_cache: bool,
}

fn parse_build_request(body: &Map<String, Value>) -> Result<BuildRequest, String> {
    let request: BuildRequest = serde_json::from_value(Value::Object(body.clone()))
        .map_err(|error| format!("mag.build invalid request: {error}"))?;
    if !request.project_root.is_absolute() || !request.cache_dir.is_absolute() {
        return Err("mag.build project_root and cache_dir must be absolute paths".into());
    }
    if request.entry.is_empty() || Path::new(&request.entry).is_absolute()
        || Path::new(&request.entry).components().any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("mag.build entry must be a non-empty project-relative path without '..'".into());
    }
    if request.module_roots.iter().any(|root| root.as_os_str().is_empty()) {
        return Err("mag.build module_roots entries must be non-empty paths".into());
    }
    Ok(request)
}

// Do not reintroduce serde_json's default 128-level parser limit: the
// compiler's successful artifacts can exceed it (including nested type evidence).
fn decode_build_artifact(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    use serde::Deserialize;
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    decoder.disable_recursion_limit();
    let artifact = Value::deserialize(&mut decoder)?;
    decoder.end()?;
    Ok(artifact)
}

async fn handle_build(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    in_reply_to: Option<&str>,
    host: &LuaHost,
) -> Result<(), MagError> {
    let request = match parse_build_request(body) {
        Ok(request) => request,
        Err(error) => return send_event(out_tx, error_body(in_reply_to, &error)).await,
    };
    let project = match nefor_mag::project_config::prepare(&request.project_root, &request.module_roots) {
        Ok(project) => project,
        Err(error) => return send_event(out_tx, error_body(in_reply_to,
            &format!("{}: {}: {}", error.code, error.path.display(), error.message))).await,
    };
    let contracts = host.registry_contracts().unwrap_or_else(|_| Value::Array(Vec::new()));
    let policy = if request.no_cache {
        nefor_mag::project_cache::CachePolicy::Bypass
    } else {
        nefor_mag::project_cache::CachePolicy::Use
    };
    let output = nefor_mag::project_cache::build_in(nefor_mag::FileCompileRequest {
        source_dir: &project.project_root,
        entry: &request.entry,
        module_roots: &project.module_roots,
        inputs: serde_json::json!({ "factory_contracts": contracts }),
        options: nefor_mag::CompilerOptions::default(),
    }, project.config_version, &request.cache_dir, policy, None);
    match output {
        Ok(output) => {
            let build = serde_json::json!(output.cache);
            match decode_build_artifact(&output.bytes) {
                Ok(artifact) => finish_compile(out_tx, in_reply_to, host, artifact, Some(build)).await,
                Err(error) => send_event(out_tx, error_body(in_reply_to,
                    &format!("mag.build artifact decoding failed: {error}"))).await,
            }
        }
        Err(error) => send_event(out_tx, mag_error_body(in_reply_to, &error)).await,
    }
}

fn load_module_roots(body: &Map<String, Value>, source_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let requested = match body.get("module_roots") {
        None => return Ok(vec![source_dir.to_path_buf()]),
        Some(Value::Array(roots)) => roots,
        Some(_) => return Err("mag.load module_roots must be an array of paths".to_owned()),
    };
    let source = source_dir
        .canonicalize()
        .map_err(|error| format!("mag.load source_dir cannot be resolved: {error}"))?;
    requested
        .iter()
        .map(|root| {
            let raw = root
                .as_str()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| {
                    "mag.load module_roots entries must be non-empty strings".to_owned()
                })?;
            let path = PathBuf::from(raw);
            let relative = !path.is_absolute();
            let candidate = if relative { source.join(path) } else { path };
            let canonical = candidate.canonicalize().map_err(|error| {
                format!(
                    "mag.load module root {} cannot be resolved: {error}",
                    candidate.display()
                )
            })?;
            if relative && !canonical.starts_with(&source) {
                return Err(format!(
                    "mag.load relative module root {} escapes source_dir",
                    raw
                ));
            }
            Ok(canonical)
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunPrincipal {
    Lead,
    Subagent,
    Untrusted,
}

impl RunPrincipal {
    fn as_str(self) -> &'static str {
        match self {
            Self::Lead => "lead",
            Self::Subagent => "subagent",
            Self::Untrusted => "untrusted",
        }
    }
}

fn authoritative_principal(source: &str, declared: Option<&Value>) -> Result<RunPrincipal, String> {
    match (source, declared.and_then(Value::as_str)) {
        ("agentic-loop", Some("lead")) => Ok(RunPrincipal::Lead),
        ("lead-workflow", Some("subagent")) => Ok(RunPrincipal::Subagent),
        ("agentic-loop", _) | ("lead-workflow", _) => Err(format!(
            "mag.execute requires the principal authorized for source {source:?}"
        )),
        // Local composition is trusted, but only the two shipped routes mint
        // instruction-notice principals. Custom/direct execute remains a
        // supported kernel entry point with explicit no-notice semantics.
        _ => Ok(RunPrincipal::Untrusted),
    }
}

fn parse_model_snapshot(
    body: &Map<String, Value>,
    principal: RunPrincipal,
) -> Result<Option<ExecutionModelSnapshot>, String> {
    let Some(raw) = body.get("model_snapshot") else {
        return if principal == RunPrincipal::Subagent {
            Err("subagent mag.execute requires model_snapshot".to_owned())
        } else {
            Ok(None)
        };
    };
    if raw
        .as_object()
        .is_some_and(|snapshot| snapshot.get("reasoning_effort") == Some(&Value::Null))
    {
        return Err("model_snapshot.reasoning_effort cannot be null".to_owned());
    }
    if raw
        .as_object()
        .is_some_and(|snapshot| snapshot.get("provider_options") == Some(&Value::Null))
    {
        return Err("model_snapshot.provider_options cannot be null".to_owned());
    }
    if let Some((name, _)) = raw
        .as_object()
        .and_then(|snapshot| snapshot.get("profiles"))
        .and_then(Value::as_object)
        .and_then(|profiles| {
            profiles.iter().find(|(_, model)| {
                model
                    .as_object()
                    .is_some_and(|model| model.get("reasoning_effort") == Some(&Value::Null))
            })
        })
    {
        return Err(format!(
            "model_snapshot.profiles[{name:?}].reasoning_effort cannot be null"
        ));
    }
    if let Some((name, _)) = raw
        .as_object()
        .and_then(|snapshot| snapshot.get("profiles"))
        .and_then(Value::as_object)
        .and_then(|profiles| {
            profiles.iter().find(|(_, model)| {
                model
                    .as_object()
                    .is_some_and(|model| model.get("provider_options") == Some(&Value::Null))
            })
        })
    {
        return Err(format!(
            "model_snapshot.profiles[{name:?}].provider_options cannot be null"
        ));
    }
    serde_json::from_value::<ExecutionModelSnapshot>(raw.clone())
        .map_err(|error| format!("invalid mag.execute model_snapshot: {error}"))?
        .validate()
        .map(Some)
}

/// Run an inline immutable program envelope through the kernel.
/// Creates the run's own kernel context (begin_run), registers the
/// constellation, delivers the initial messages (each actor constructs lazily
/// at its first firing), streams lifecycle events, and — for a synchronous
/// program — replies `mag.run_result` with the sink's final result + output
/// path in the same turn. An async program (a provider round-trip pending)
/// defers the reply until `mag.run_complete`. Concurrent executes are
/// accepted: each run lives in its own context and a run starting mid-flight
/// touches nothing of the others.
async fn handle_execute(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    source: &str,
    body: &Map<String, Value>,
    in_reply_to: Option<&str>,
    runtime: (&LuaHost, &mut ActiveExecutes, &mut CapabilityBridge),
) -> Result<(), MagError> {
    let (host, active, bridge) = runtime;
    let artifact = match body.get("artifact") {
        Some(artifact) => artifact,
        None => return send_event(out_tx, error_body(in_reply_to,
            "mag.execute requires an inline nefor.mag program envelope")).await,
    };
    let decoded = match artifact_program(artifact) {
        Ok(decoded) => decoded,
        Err(error) => return send_event(out_tx, error_body(in_reply_to, &error)).await,
    };
    let mut execution = decoded;

    // Apply the control plane's per-actor params overlay before spawn. Actor
    // params are kernel-opaque data owned by the factory (docs/ir.md), so an
    // overlay patched at apply time is legitimate control-plane input for
    // ambient runtime values such as system context. Shallow per-actor
    // top-level merge; unknown ids are ignored (a race artifact, not an error).
    if let Some(overlay) = body.get("params_overlay").and_then(Value::as_object) {
        if let Err(error) = apply_params_overlay(&mut execution, overlay) {
            return send_event(out_tx, error_body(in_reply_to, &error)).await;
        }
    }
    if let Err(error) = preflight_output_schemas(&execution) {
        return send_event(out_tx, error_body(in_reply_to, &error)).await;
    }
    let preflight = host.preflight_program(&execution.initial, &execution.operations)?;
    if !preflight.ok {
        return send_event(
            out_tx,
            error_body(in_reply_to, preflight.error.as_deref().unwrap_or("artifact preflight failed")),
        )
        .await;
    }

    let run_id = body
        .get("run_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(default_run_id);
    let run_name = body
        .get("run_name")
        .and_then(Value::as_str)
        .unwrap_or(&run_id);
    let session_id = match body.get("session_id").and_then(Value::as_str) {
        Some(session_id) if !session_id.is_empty() => session_id,
        _ => {
            return send_event(
                out_tx,
                error_body(in_reply_to, "mag.execute requires a non-empty session_id"),
            )
            .await
        }
    };
    let principal = match authoritative_principal(source, body.get("principal")) {
        Ok(principal) => principal,
        Err(error) => return send_event(out_tx, error_body(in_reply_to, &error)).await,
    };
    let model_snapshot = match parse_model_snapshot(body, principal) {
        Ok(snapshot) => snapshot,
        Err(error) => return send_event(out_tx, error_body(in_reply_to, &error)).await,
    };
    let conversation_id = match body.get("conversation_id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ if principal == RunPrincipal::Lead => {
            return send_event(
                out_tx,
                error_body(
                    in_reply_to,
                    "lead mag.execute requires a non-empty conversation_id",
                ),
            )
            .await
        }
        _ => format!("{session_id}/{run_id}"),
    };

    let started_at = Instant::now();
    let begun = host.begin_run_with_principal(
        &run_id,
        run_name,
        Some(session_id),
        Some(principal.as_str()),
        Some(&conversation_id),
        model_snapshot.as_ref(),
    )?;
    // The kernel reaps stale contexts from a previous session at the boundary
    // (begin_run); fail their pending replies before driving the new run.
    settle_reaped(out_tx, host, active, bridge, &begun.reaped).await?;
    if !begun.ok {
        let msg = begun.error.unwrap_or_else(|| "begin_run failed".into());
        return send_event(
            out_tx,
            run_result_failed(in_reply_to, &run_id, &msg, elapsed_ms(started_at)),
        )
        .await;
    }
    let outcome = host.start_program(&run_id, &execution.initial, &execution.operations)?;
    flush_emits(out_tx, host, bridge).await?;

    // A failed apply / rejected initial modification: nothing useful spawned;
    // drop the context.
    if !outcome.ok {
        let msg = outcome.error.unwrap_or_else(|| "start failed".into());
        send_event(
            out_tx,
            run_result_failed(in_reply_to, &run_id, &msg, elapsed_ms(started_at)),
        )
        .await?;
        host.end_run(&run_id, TeardownReason::RunFailed)?;
        return flush_emits(out_tx, host, bridge).await;
    }

    // Synchronous terminal state inside `start`: reply now and tear the run's
    // context down (reaping any still-live actors — e.g. a parallel branch —
    // through the fold).
    let terminal = if let Some(rc) = host.take_run_complete(&run_id)? {
        Some((
            run_result_ok(in_reply_to, &run_id, &rc, elapsed_ms(started_at)),
            TeardownReason::RunComplete,
        ))
    } else {
        host.take_run_failed(&run_id)?.map(|error| {
            (
                run_result_failed(in_reply_to, &run_id, &error, elapsed_ms(started_at)),
                TeardownReason::RunFailed,
            )
        })
    };
    if let Some((reply, reason)) = terminal {
        send_event(out_tx, reply).await?;
        host.end_run(&run_id, reason)?;
        return flush_emits(out_tx, host, bridge).await;
    }

    // Async program: defer the terminal reply until `mag.run_complete` (or
    // `mag.run_failed`) arrives via capability responses.
    active.insert(
        run_id,
        ActiveExecute {
            in_reply_to: in_reply_to.map(str::to_owned),
            started_at,
        },
    );
    Ok(())
}

fn initial_actor_address(id: &str) -> String {
    format!("actor:{}:{id}", id.len())
}

fn template_actor_address(operation_id: &str, slot: &str) -> String {
    format!(
        "operation:{}:{operation_id}:template:{}:{slot}",
        operation_id.len(),
        slot.len()
    )
}

fn actor_inventory(program: &DecodedProgram) -> Vec<(String, &Value)> {
    let mut inventory = Vec::new();
    for actor in program
        .initial
        .get("actors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = actor.get("id").and_then(Value::as_str).unwrap_or("<unknown>");
        inventory.push((initial_actor_address(id), actor));
    }
    for operation in &program.operations {
        let operation_id = operation
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        for actor in operation
            .get("template")
            .and_then(|template| template.get("actors"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let slot = actor
                .get("slot")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            inventory.push((template_actor_address(operation_id, slot), actor));
        }
    }
    inventory
}

fn preflight_output_schemas(program: &DecodedProgram) -> Result<(), String> {
    for (address, actor) in actor_inventory(program) {
        let Some(actor) = actor.as_object() else {
            continue;
        };
        if !matches!(
            actor.get("factory").and_then(Value::as_str),
            Some("structured-output" | "nefor.factory.structured-output")
        ) {
            continue;
        }
        let schema = actor
            .get("params")
            .and_then(Value::as_object)
            .and_then(|params| params.get("schema"))
            .ok_or_else(|| format!("structured-output {address:?}: params.schema is required"))?;
        let schema: nefor_mag::schema::TypeSchema = serde_json::from_value(schema.clone())
            .map_err(|error| {
                format!("structured-output {address:?}: invalid MAG type schema: {error}")
            })?;
        if schema.version != nefor_mag::schema::SCHEMA_VERSION {
            return Err(format!("structured-output {address:?}: unsupported MAG schema version {}", schema.version));
        }
    }
    Ok(())
}

/// Merge a per-actor params overlay into a modification's actors before spawn.
/// The overlay is `{ actor_id: { param: value, ... }, ... }`. Each patch is a
/// shallow top-level merge into the matching actor's `params` (created if
/// absent, replaced if non-object). Actors not named in the overlay are
/// untouched; overlay keys with no matching actor are ignored.
fn apply_actor_patch(address: &str, actor: &mut Value, overlay: &Map<String, Value>) -> Result<(), String> {
    let Some(obj) = actor.as_object_mut() else { return Ok(()); };
    let Some(patch) = overlay.get(address).and_then(Value::as_object) else { return Ok(()); };
    let factory = obj.get("factory").and_then(Value::as_str);
    let protected_params: &[&str] = match factory {
        Some("structured-output" | "nefor.factory.structured-output") => &[
            "model_profile", "schema", "output_type", "error_type",
            "provider_error_type",
        ],
        Some("llm" | "nefor.factory.llm") => &["model_profile"],
        _ => &[],
    };
    if let Some(param) = protected_params.iter().find(|param| patch.contains_key(**param)) {
        return Err(format!(
            "params_overlay for actor {address:?} cannot replace protected compiler-derived param {param:?}"
        ));
    }
    let params = obj.entry("params").or_insert_with(|| Value::Object(Map::new()));
    if !params.is_object() { *params = Value::Object(Map::new()); }
    if let Some(params) = params.as_object_mut() {
        for (key, value) in patch { params.insert(key.clone(), value.clone()); }
    }
    Ok(())
}

fn apply_params_overlay(program: &mut DecodedProgram, overlay: &Map<String, Value>) -> Result<(), String> {
    for actor in program
        .initial
        .get_mut("actors")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        let id = actor.get("id").and_then(Value::as_str).unwrap_or("<unknown>").to_owned();
        apply_actor_patch(&initial_actor_address(&id), actor, overlay)?;
    }
    for operation in &mut program.operations {
        let operation_id = operation
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>")
            .to_owned();
        for actor in operation
            .get_mut("template")
            .and_then(|template| template.get_mut("actors"))
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let slot = actor
                .get("slot")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>")
                .to_owned();
            apply_actor_patch(&template_actor_address(&operation_id, &slot), actor, overlay)?;
        }
    }
    Ok(())
}

/// Named rejection for a `mag.apply` outside a live run. The control plane's
/// apply authority (docs/ir.md, "Kernel operations": the plane reaches
/// `actors`/`kills`/`messages` directly) is scoped to an active session with an
/// in-flight run — there is no constellation to modify otherwise.
const APPLY_NO_LIVE_RUN: &str =
    "mag.apply rejected: no live run (control-plane apply requires an active \
     session with an in-flight run — docs/ir.md, Kernel operations)";

/// Named rejection for a run-id-less `mag.apply` while several runs are live:
/// the plane must say which constellation it is modifying.
const APPLY_AMBIGUOUS_RUN: &str =
    "mag.apply rejected: several runs are live; pass run_id to name the \
     target constellation";

/// Apply one graph modification directly to the kernel — the control
/// plane's mid-run kernel op (docs/ir.md, "Kernel operations"). The initial
/// modification runs via `mag.execute`; this is the *later* path: a
/// `{ kills = [...] }` modification kills an in-flight actor (the factory's
/// abort envelope reaches the bus, correlations drop, the late reply voids), a
/// `{ messages = [...] }` delivers a signal, a `{ actors = [...] }` registers
/// into the live constellation (constructing at first firing).
///
/// Guard policy: an apply is accepted only while a run is live (`active`), and
/// every accepted apply is logged with its `source`. The target run is the
/// request's `run_id`; a run-id-less apply falls back to the single live run
/// and is rejected as ambiguous when several are live. Outside any live run
/// the apply is rejected with [`APPLY_NO_LIVE_RUN`] — the control-plane
/// authority ir.md grants the plane exists only *over a running
/// constellation*, so a session-less apply has nothing to act on. Lifecycle
/// events the apply emits stream on the wire; a completion it triggers
/// settles that run.
async fn handle_apply(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    in_reply_to: Option<&str>,
    host: &LuaHost,
    active: &mut ActiveExecutes,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    if body.contains_key("model_snapshot") {
        return send_event(
            out_tx,
            error_body(in_reply_to, "mag.apply cannot replace a run model_snapshot"),
        )
        .await;
    }
    // Resolve the target run: explicit run_id, else the single live run.
    let run_id = match body.get("run_id").and_then(Value::as_str) {
        Some(rid) if active.contains_key(rid) => rid.to_owned(),
        Some(rid) => {
            return send_event(
                out_tx,
                applied_body(
                    in_reply_to,
                    false,
                    Some(&format!("mag.apply rejected: run '{rid}' is not live")),
                ),
            )
            .await
        }
        None => match active.len() {
            0 => {
                return send_event(
                    out_tx,
                    applied_body(in_reply_to, false, Some(APPLY_NO_LIVE_RUN)),
                )
                .await
            }
            1 => match active.keys().next() {
                Some(run_id) => run_id.clone(),
                None => {
                    return send_event(
                        out_tx,
                        applied_body(in_reply_to, false, Some(APPLY_NO_LIVE_RUN)),
                    )
                    .await
                }
            },
            _ => {
                return send_event(
                    out_tx,
                    applied_body(in_reply_to, false, Some(APPLY_AMBIGUOUS_RUN)),
                )
                .await
            }
        },
    };

    let artifact = match body.get("artifact") {
        Some(artifact) => artifact,
        None => return send_event(out_tx, error_body(in_reply_to,
            "mag.apply requires an inline nefor.mag delta envelope")).await,
    };
    let mut modification = match artifact_delta(artifact) {
        Ok(delta) => delta,
        Err(error) => return send_event(out_tx, error_body(in_reply_to, &error)).await,
    };

    // Mid-run spawns use the same control-plane-resolved actor parameters as
    // initial execution. This keeps dynamically added LLM actors independent
    // of whether they entered through `mag.execute` or `mag.apply`.
    let mut delta_program = DecodedProgram { initial: modification, operations: Vec::new() };
    if let Some(overlay) = body.get("params_overlay").and_then(Value::as_object) {
        if let Err(error) = apply_params_overlay(&mut delta_program, overlay) {
            return send_event(out_tx, error_body(in_reply_to, &error)).await;
        }
    }
    if let Err(error) = preflight_output_schemas(&delta_program) {
        return send_event(out_tx, error_body(in_reply_to, &error)).await;
    }
    modification = delta_program.initial;

    // Audit every accepted apply with its declared source (docs/ir.md: the
    // modification log is the run — the plane's mid-run ops are part of it).
    let source = body
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("<unspecified>");
    tracing::info!(source = %source, run_id = %run_id, "mag.apply accepted");

    let state = host.apply(&run_id, &modification)?;
    // Forward lifecycle events and capability cancellations before acknowledging
    // the apply. The bridge selects canonical provider or tool-gate cancellation
    // from its request ownership.
    flush_emits(out_tx, host, bridge).await?;
    send_event(
        out_tx,
        applied_body(in_reply_to, state.ok, state.error.as_deref()),
    )
    .await?;
    // A modification that completes the run (e.g. a send that unblocks the sink)
    // settles that run's in-flight execute reply.
    settle_run(out_tx, host, active, bridge, &run_id).await
}

/// Kill one live run: end its kernel context — reaping its actors through
/// the fold, so kill handlers run and capability cancellations reach the bus —
/// then settle the pending execute with `mag.run_result status:"killed"`. The
/// envelopes are flushed BEFORE the terminal reply so a consumer that
/// treats the reply as "turn closed" observes the aborts first. A kill for
/// a run without a live execute is a logged no-op.
async fn handle_kill_run(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    host: &LuaHost,
    active: &mut ActiveExecutes,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    let run_id = match body.get("run_id").and_then(Value::as_str) {
        Some(r) => r,
        None => return Ok(()),
    };
    let a = match active.remove(run_id) {
        Some(a) => a,
        None => {
            tracing::info!(run_id = %run_id, "mag.kill_run for a run that is not live; no-op");
            return Ok(());
        }
    };
    host.end_run(run_id, TeardownReason::Killed)?;
    flush_emits(out_tx, host, bridge).await?;
    send_event(
        out_tx,
        run_result_killed(
            a.in_reply_to.as_deref(),
            run_id,
            elapsed_ms(a.started_at),
        ),
    )
    .await
}

async fn handle_kill_all_runs(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    host: &LuaHost,
    active: &mut ActiveExecutes,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    let mut run_ids: Vec<String> = active.keys().cloned().collect();
    run_ids.sort();
    for run_id in run_ids {
        let Some(a) = active.remove(&run_id) else {
            continue;
        };
        host.end_run(&run_id, TeardownReason::Killed)?;
        flush_emits(out_tx, host, bridge).await?;
        send_event(
            out_tx,
            run_result_killed(
                a.in_reply_to.as_deref(),
                &run_id,
                elapsed_ms(a.started_at),
            ),
        )
        .await?;
    }
    Ok(())
}

async fn handle_steer_run(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    host: &LuaHost,
    active: &ActiveExecutes,
) -> Result<(), MagError> {
    let in_reply_to = body.get("id").and_then(Value::as_str);
    let run_id = body.get("run_id").and_then(Value::as_str).unwrap_or("");
    let actor_id = body.get("actor_id").and_then(Value::as_str).unwrap_or("");
    let message = body.get("message").cloned().unwrap_or(Value::Null);
    let accepted = active.contains_key(run_id)
        && !actor_id.is_empty()
        && host.steer_run(run_id, actor_id, &message)?;
    let mut ack = Map::new();
    ack.insert("kind".into(), Value::String(RUN_STEERED_KIND.into()));
    if let Some(id) = in_reply_to {
        ack.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    ack.insert("run_id".into(), Value::String(run_id.to_owned()));
    ack.insert("accepted".into(), Value::Bool(accepted));
    send_event(out_tx, ack).await
}

async fn handle_resume_actor(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    host: &LuaHost,
    active: &mut ActiveExecutes,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    let run_id = body.get("run_id").and_then(Value::as_str).unwrap_or("");
    let actor_id = body.get("actor_id").and_then(Value::as_str).unwrap_or("");
    let message = body.get("message").cloned().unwrap_or(Value::Null);
    let accepted = active.contains_key(run_id)
        && !actor_id.is_empty()
        && host.resume_actor(run_id, actor_id, &message)?;
    if accepted {
        flush_emits(out_tx, host, bridge).await?;
        settle_run(out_tx, host, active, bridge, run_id).await?;
    }
    let mut ack = Map::new();
    ack.insert("kind".into(), Value::String(ACTOR_RESUMED_KIND.into()));
    ack.insert("run_id".into(), Value::String(run_id.to_owned()));
    ack.insert("actor_id".into(), Value::String(actor_id.to_owned()));
    ack.insert("accepted".into(), Value::Bool(accepted));
    send_event(out_tx, ack).await
}

/// Interrupt one live run. `mag.interrupt_run` carries an optional `terminate`
/// flag that selects between two semantics:
///
/// * `terminate` absent/false — GRACEFUL (the lead's OWN turn). Settle the
///   in-flight capabilities as "interrupted by user" and cancel the real work;
///   the run SURVIVES, so — unlike `handle_kill_run` — the `active` entry is
///   kept. Flushing forwards the cancels and any re-fire the settle produced
///   (the lead llm's fresh provider round). A provider-leg interrupt fails the
///   run synchronously, so `settle_run` after the flush closes it; the tool-leg
///   case leaves the run pending on its re-fire and `settle_run` is a no-op.
///
/// * `terminate == true` — TERMINATING (a dispatched sub-run). The run is
///   ephemeral, so the interrupt STOPS it: cancel the in-flight work (a
///   `tool.cancel` per open correlation → bash dies via killpg, a nested
///   sub-run is interrupted down the chain) but deliver NO reply, then END the
///   run FAILED. The run's llm never re-fires to a "Completed" answer; the
///   terminal `mag.run_result status:"failed"` relays "interrupted by user" to
///   the lead. Mirrors `handle_kill_run`'s reap-then-terminal-reply ordering.
///
/// A run that is not live is a logged no-op.
async fn handle_interrupt_run(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    host: &LuaHost,
    active: &mut ActiveExecutes,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    let run_id = match body.get("run_id").and_then(Value::as_str) {
        Some(r) => r.to_owned(),
        None => return Ok(()),
    };
    let terminate = body
        .get("terminate")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if terminate {
        // A dispatched sub-run is reaped immediately. Actor teardown consumes
        // each open correlation and emits its one generic cancellation before
        // removal, so there is no separate pre-cancel path here.
        let a = match active.remove(&run_id) {
            Some(a) => a,
            None => {
                tracing::info!(run_id = %run_id, "mag.interrupt_run(terminate) for a run that is not live; no-op");
                return Ok(());
            }
        };
        host.end_run(&run_id, TeardownReason::RunFailed)?;
        tracing::info!(run_id = %run_id, "mag.interrupt_run(terminate): run ended failed");
        flush_emits(out_tx, host, bridge).await?;
        return send_event(
            out_tx,
            run_result_failed(
                a.in_reply_to.as_deref(),
                &run_id,
                INTERRUPT_FAILURE,
                elapsed_ms(a.started_at),
            ),
        )
        .await;
    }

    if !active.contains_key(&run_id) {
        tracing::info!(run_id = %run_id, "mag.interrupt_run for a run that is not live; no-op");
        return Ok(());
    }
    let settled = host.interrupt_run(&run_id, INTERRUPT_FAILURE)?;
    tracing::info!(run_id = %run_id, settled, "mag.interrupt_run: in-flight capabilities settled as interrupted");
    flush_emits(out_tx, host, bridge).await?;
    // The run stays alive on the tool leg (re-fire pending); a provider-leg
    // interrupt may have failed it synchronously — settle if so.
    settle_run(out_tx, host, active, bridge, &run_id).await
}

async fn handle_tool_stream(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    host: &LuaHost,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    let id = match body.get("id").and_then(Value::as_str) {
        Some(id) => id,
        None => return Ok(()),
    };
    let stream = match body.get("stream").and_then(Value::as_str) {
        Some("stdout" | "stderr") => body
            .get("stream")
            .and_then(Value::as_str)
            .unwrap_or("stdout"),
        _ => return Ok(()),
    };
    let text = match body.get("text").and_then(Value::as_str) {
        Some(text) => text,
        None => return Ok(()),
    };
    let _ = host.bus_observation(
        id,
        "append",
        "terminal_events",
        &serde_json::json!({ "kind": stream, "text": text }),
    )?;
    flush_emits(out_tx, host, bridge).await
}

/// Route a capability response into the kernel (unblocking a deferred
/// activation in the owning run's context), forward whatever it produced, and
/// settle that run if it completed. Correlation ids are run-scoped, so the
/// kernel names the run the response advanced.
async fn handle_tool_result(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    host: &LuaHost,
    active: &mut ActiveExecutes,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    let id = match body.get("id").and_then(Value::as_str) {
        Some(id) => id,
        None => return Ok(()),
    };
    // Providers/tools reply with `output`; some producers use `result`.
    let result = body.get("output").or_else(|| body.get("result"));
    let error = body.get("error").and_then(Value::as_str);
    let completion_delivery = body.get("completion_delivery").and_then(Value::as_str);
    let advanced = host.bus_response(id, result, error, completion_delivery)?;
    flush_emits(out_tx, host, bridge).await?;
    match advanced {
        Some(run_id) => settle_run(out_tx, host, active, bridge, &run_id).await,
        None => Ok(()),
    }
}

/// Route one canonical terminal provider event into the kernel as the
/// correlated capability response. The reply may re-fire the requesting actor
/// or complete the run, so forward whatever the kernel produced and settle.
async fn handle_provider_reply(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    reply: bridge::ProviderReply,
    host: &LuaHost,
    active: &mut ActiveExecutes,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    let advanced = host.bus_response(
        &reply.request_id,
        reply.result.as_ref(),
        reply.error.as_deref(),
        None,
    )?;
    flush_emits(out_tx, host, bridge).await?;
    match advanced {
        Some(run_id) => settle_run(out_tx, host, active, bridge, &run_id).await,
        None => Ok(()),
    }
}

/// A run id for an execute that didn't carry one. Millisecond-stamped; the
/// control plane usually supplies its own.
fn default_run_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("mag-run-{ms}")
}

// ---- static body constructors ----------------------------------------------

fn hello_body(kernel: Option<&str>, factories: &[String], contracts: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(format!("{PLUGIN_NAME}.hello")));
    m.insert("version".into(), Value::String(PLUGIN_VERSION.into()));
    if let Some(k) = kernel {
        m.insert("kernel".into(), Value::String(k.to_owned()));
    }
    // The kernel registry's factory names — the control plane's pre-execute
    // validation source of truth, available from startup.
    m.insert(
        "factories".into(),
        Value::Array(factories.iter().cloned().map(Value::String).collect()),
    );
    m.insert("factory_contracts".into(), contracts);
    m
}

fn pong_body(in_reply_to: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(PONG_KIND.into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    m
}

fn loaded_body(
    in_reply_to: Option<&str>,
    hash: &str,
    artifact: Value,
    factories: &[String],
    contracts: Value,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(LOADED_KIND.into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    m.insert("hash".into(), Value::String(hash.to_owned()));
    m.insert("artifact".into(), artifact);
    // The kernel registry's factory names — the control plane's validation
    // source of truth for reasoner/factory types.
    m.insert(
        "factories".into(),
        Value::Array(factories.iter().cloned().map(Value::String).collect()),
    );
    m.insert("factory_contracts".into(), contracts);
    m
}

/// Interpret a raw MAG artifact as Nefor's graph-modification IR. MAG itself
/// neither knows nor validates this application-owned schema.
fn immutable_artifact_hash(artifact: &Value) -> Result<String, String> {
    let bytes = serde_json::to_vec(artifact)
        .map_err(|error| format!("mag.load artifact serialization failed: {error}"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn exact_object<'a>(
    value: &'a Value,
    required: &[&str],
    context: &str,
) -> Result<&'a Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{context} must be an object"))?;
    for key in object.keys() {
        if !required.contains(&key.as_str()) {
            return Err(format!("{context} has unknown field {key}"));
        }
    }
    for key in required {
        if !object.contains_key(*key) {
            return Err(format!("{context} requires {key}"));
        }
    }
    Ok(object)
}

fn graph_modification(
    artifact: &Value,
    fields: &[&str],
    context: &str,
) -> Result<Value, String> {
    Ok(Value::Object(
        exact_object(artifact, fields, context)?.clone(),
    ))
}

fn unpack_packed(value: &mut Value, context: &str) -> Result<(), String> {
    let object = value
        .as_object()
        .filter(|object| object.len() == 2)
        .filter(|object| object.get("$mag").and_then(Value::as_str) == Some("packed-value"))
        .ok_or_else(|| format!("{context} must be a compiler-owned packed value"))?;
    *value = object
        .get("value")
        .cloned()
        .ok_or_else(|| format!("{context} packed value requires value"))?;
    Ok(())
}

fn unpack_modification(modification: &mut Value, context: &str) -> Result<(), String> {
    for (index, actor) in modification
        .get_mut("actors")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let params = actor
            .get_mut("params")
            .ok_or_else(|| format!("{context}.actors[{index}].params is required"))?;
        unpack_packed(params, &format!("{context}.actors[{index}].params"))?;
    }
    for (index, message) in modification
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let content = message
            .get_mut("content")
            .ok_or_else(|| format!("{context}.messages[{index}].content is required"))?;
        unpack_packed(content, &format!("{context}.messages[{index}].content"))?;
    }
    Ok(())
}

fn unpack_operations(operations: &mut [Value]) -> Result<(), String> {
    for (operation_index, operation) in operations.iter_mut().enumerate() {
        if let Some(captures) = operation.get_mut("captures").and_then(Value::as_object_mut) {
            for (capture_id, capture) in captures {
                let value = capture.get_mut("value").ok_or_else(|| {
                    format!(
                        "mag.execute program.operations[{operation_index}].captures.{capture_id}.value is required"
                    )
                })?;
                unpack_packed(
                    value,
                    &format!(
                        "mag.execute program.operations[{operation_index}].captures.{capture_id}.value"
                    ),
                )?;
            }
        }
        if let Some(template) = operation.get_mut("template") {
            let context = format!("mag.execute program.operations[{operation_index}].template");
            for (index, actor) in template.get_mut("actors").and_then(Value::as_array_mut).into_iter().flatten().enumerate() {
                let params = actor.get_mut("params").ok_or_else(|| format!("{context}.actors[{index}].params is required"))?;
                unpack_packed(params, &format!("{context}.actors[{index}].params"))?;
            }
            for (index, message) in template.get_mut("messages").and_then(Value::as_array_mut).into_iter().flatten().enumerate() {
                let label = format!("{context}.messages[{index}].content");
                let content = message.get_mut("content").ok_or_else(|| format!("{label} is required"))?;
                let payload = exact_object(content, &["constructor", "value"], &label)?;
                match payload.get("constructor").and_then(Value::as_str) {
                    Some("Static") => {
                        let value = content.get_mut("value").ok_or_else(|| format!("{label}.value is required"))?;
                        unpack_packed(value, &format!("{label}.value"))?;
                    }
                    Some("Expression") if payload.get("value").and_then(Value::as_str).is_some() => {}
                    _ => return Err(format!("{label} must be a Static packed payload or Expression reference")),
                }
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ActorEndpointValue { id: ActorId }

#[allow(dead_code)]
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(tag = "constructor", content = "value", deny_unknown_fields)]
enum Endpoint { ActorEndpoint(ActorEndpointValue) }

#[allow(dead_code)]
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(transparent)]
struct ActorId(String);

#[allow(dead_code)]
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPort {
    endpoint: Endpoint,
    #[serde(rename = "type")]
    semantic_type: Value,
    type_id: String,
    wire: String,
}

#[allow(dead_code)]
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundaryLeaf { port: StoredPort, steps: Vec<Value> }

#[allow(dead_code)]
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Flow { steps: Vec<Value> }

#[allow(dead_code)]
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBoundary {
    #[serde(rename = "type")]
    semantic_type: Value,
    type_id: String,
    leaves: Vec<BoundaryLeaf>,
    through: Vec<Flow>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TopologyRoute {
    id: String,
    from: StoredPort,
    to: StoredPort,
    transforms: Vec<Value>,
}

#[allow(dead_code)]
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TopologyMessage {
    to: StoredPort,
    transforms: Vec<Value>,
    semantic_type: Value,
    semantic_type_id: String,
    content: Value,
}

fn validate_topology_shapes(modification: &Value, context: &str) -> Result<(), String> {
    let object = modification.as_object().ok_or_else(|| format!("{context} must be an object"))?;
    let actors = object.get("actors").and_then(Value::as_array)
        .ok_or_else(|| format!("{context}.actors must be an array"))?;
    for (index, actor) in actors.iter().enumerate() {
        let label = format!("{context}.actors[{index}]");
        let fields = exact_object(actor, &["id", "factory", "type_arguments", "params", "input", "outputs"], &label)?;
        serde_json::from_value::<StoredPort>(fields["input"].clone())
            .map_err(|error| format!("{label}.input: {error}"))?;
        let outputs = fields["outputs"].as_array().ok_or_else(|| format!("{label}.outputs must be an array"))?;
        for output in outputs {
            serde_json::from_value::<StoredPort>(output.clone()).map_err(|error| format!("{label}.outputs: {error}"))?;
        }
    }
    if let Some(result) = object.get("result") {
        let fields = exact_object(result, &["from"], &format!("{context}.result"))?;
        serde_json::from_value::<StoredBoundary>(fields["from"].clone())
            .map_err(|error| format!("{context}.result.from: {error}"))?;
    }
    for (field, target) in [("routes", "route"), ("messages", "message")] {
        let values = object.get(field).and_then(Value::as_array)
            .ok_or_else(|| format!("{context}.{field} must be an array"))?;
        for (index, value) in values.iter().enumerate() {
            let result = if field == "routes" {
                serde_json::from_value::<TopologyRoute>(value.clone()).map(|_| ())
            } else {
                serde_json::from_value::<TopologyMessage>(value.clone()).map(|_| ())
            };
            result.map_err(|error| format!("{context}.{field}[{index}] has invalid {target}: {error}"))?;
        }
    }
    Ok(())
}

struct DecodedProgram {
    initial: Value,
    operations: Vec<Value>,
}

fn artifact_program(artifact: &Value) -> Result<DecodedProgram, String> {
    let object = exact_object(
        artifact,
        &["format", "version", "kind", "program"],
        "mag.execute artifact",
    )?;
    if object.get("format").and_then(Value::as_str) != Some("nefor.mag") {
        return Err("mag.execute artifact has an unsupported format".to_owned());
    }
    if object.get("version").and_then(Value::as_u64) != Some(4) {
        return Err("mag.execute artifact has an unsupported nefor.mag version".to_owned());
    }
    if object.get("kind").and_then(Value::as_str) != Some("program") {
        return Err("mag.execute requires a nefor.mag program envelope".to_owned());
    }
    let program_value = object
        .get("program")
        .ok_or_else(|| "mag.execute program envelope requires a payload".to_owned())?;
    let program = exact_object(
        program_value,
        &["initial", "operations"],
        "mag.execute program",
    )?;
    let operations = program
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| "mag.execute program.operations must be an array".to_owned())?;
    let operation_fields = ["id", "on", "captures", "expressions", "template"];
    for (index, operation) in operations.iter().enumerate() {
        exact_object(
            operation,
            &operation_fields,
            &format!("mag.execute program.operations[{index}]"),
        )?;
    }
    let mut initial = graph_modification(
        program
            .get("initial")
            .ok_or_else(|| "mag.execute program envelope requires initial".to_owned())?,
        &[
            "types",
            "actors",
            "routes",
            "messages",
            "nodes",
            "kills",
            "result",
        ],
        "mag.execute program initial",
    )?;
    let mut operations = operations.clone();
    unpack_modification(&mut initial, "mag.execute program initial")?;
    validate_topology_shapes(&initial, "mag.execute program initial")?;
    unpack_operations(&mut operations)?;
    Ok(DecodedProgram {
        initial,
        operations,
    })
}

fn artifact_delta(artifact: &Value) -> Result<Value, String> {
    let object = exact_object(
        artifact,
        &["format", "version", "kind", "delta"],
        "mag.apply artifact",
    )?;
    if object.get("format").and_then(Value::as_str) != Some("nefor.mag") {
        return Err("mag.apply artifact has an unsupported format".to_owned());
    }
    if object.get("version").and_then(Value::as_u64) != Some(4) {
        return Err("mag.apply artifact has an unsupported nefor.mag version".to_owned());
    }
    if object.get("kind").and_then(Value::as_str) != Some("delta") {
        return Err("mag.apply requires a nefor.mag delta envelope".to_owned());
    }
    let mut delta = graph_modification(
        object
            .get("delta")
            .ok_or_else(|| "mag.apply delta envelope requires delta".to_owned())?,
        &[
            "types",
            "actors",
            "routes",
            "messages",
            "nodes",
            "kills",
        ],
        "mag.apply delta",
    )?;
    unpack_modification(&mut delta, "mag.apply delta")?;
    validate_topology_shapes(&delta, "mag.apply delta")?;
    Ok(delta)
}

#[cfg(test)]
#[allow(dead_code)]
fn artifact_modification(artifact: &Value) -> Result<Value, String> {
    artifact_program(artifact).map(|program| program.initial)
}

/// Terminal run reply on success: status, the declared boundary result INLINE
/// (`result` — text/kind/structured payload, exactly what the boundary signalled
/// on `mag.run_complete`), and the persisted output PATH when the kernel's
/// writer landed one. `persisted` reflects an actual write (an output path
/// exists), not merely a wired writer.
fn run_result_ok(
    in_reply_to: Option<&str>,
    run_id: &str,
    rc: &RunCompletion,
    duration_ms: u64,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(RUN_RESULT_KIND.into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    m.insert("run_id".into(), Value::String(run_id.to_owned()));
    m.insert("status".into(), Value::String("completed".into()));
    m.insert("duration_ms".into(), Value::from(duration_ms));
    m.insert("persisted".into(), Value::Bool(rc.persisted));
    if let Some(path) = &rc.output_path {
        m.insert("output_path".into(), Value::String(path.clone()));
    }
    if let Some(result) = &rc.result {
        m.insert("result".into(), result.clone());
    }
    m
}

/// Terminal run reply on failure: status + the error naming what went wrong
/// (rejected modification, unhandled actor failure).
fn run_result_failed(
    in_reply_to: Option<&str>,
    run_id: &str,
    error: &str,
    duration_ms: u64,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(RUN_RESULT_KIND.into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    m.insert("run_id".into(), Value::String(run_id.to_owned()));
    m.insert("status".into(), Value::String("failed".into()));
    m.insert("duration_ms".into(), Value::from(duration_ms));
    m.insert("error".into(), Value::String(error.to_owned()));
    m
}

/// Terminal run reply for a control-plane kill (`mag.kill_run`): the pending
/// execute settles as status "killed". Distinct from "failed" so consumers
/// treat it as "turn aborted" (no history append, no error surface), not as
/// an error.
fn run_result_killed(
    in_reply_to: Option<&str>,
    run_id: &str,
    duration_ms: u64,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(RUN_RESULT_KIND.into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    m.insert("run_id".into(), Value::String(run_id.to_owned()));
    m.insert("status".into(), Value::String("killed".into()));
    m.insert("duration_ms".into(), Value::from(duration_ms));
    m
}

fn elapsed_ms(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Acknowledge a `mag.apply`: whether the fold accepted the modification, plus
/// the rejection error when it did not. The applied modification's own
/// lifecycle events (`mag.modification_applied` / `mag.actor_killed` / …) carry
/// the detail; this is just the control-plane ack.
fn applied_body(in_reply_to: Option<&str>, ok: bool, error: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(APPLIED_KIND.into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    m.insert("ok".into(), Value::Bool(ok));
    if let Some(e) = error {
        m.insert("error".into(), Value::String(e.to_owned()));
    }
    m
}

fn mag_error_body(
    in_reply_to: Option<&str>,
    error: &nefor_mag::error::MagError,
) -> Map<String, Value> {
    let mut body = error_body(in_reply_to, &error.to_string());
    if let nefor_mag::error::MagError::Syntax(diagnostic) = error {
        if let Ok(value) = serde_json::to_value(diagnostic) {
            body.insert("diagnostic".into(), value);
        }
    }
    body
}

fn error_body(in_reply_to: Option<&str>, message: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(ERROR_KIND.into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    m.insert("message".into(), Value::String(message.to_owned()));
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
) -> Result<(), MagError> {
    out_tx
        .send(PluginOutgoing::event(body))
        .await
        .map_err(|_| TransportError::WriterClosed)?;
    Ok(())
}


async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), MagError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| TransportError::WriterClosed)?;
    Ok(())
}
