//! mag — NCP v0.1 plugin hosting the MAG actor-kernel runtime.
//!
//! Completes the NCP ready handshake, hosts a Lua VM that loads the kernel from
//! the config-resolved Lua path, and answers the mag protocol: `mag.ping`
//! (liveness), `mag.load` (evaluate a program, cache it, reply with the initial
//! modification + the registry's factory names), `mag.eval` (apply a rule fn),
//! and `mag.execute` (run the resident/inline program through the kernel —
//! register the constellation, deliver its initial messages (actors construct
//! lazily at first firing), stream lifecycle events, and reply
//! `mag.run_result` with the sink's final result + output path).
//!
//! Layering mirrors the sibling plugins (`reasoner-graph`, `tool-gate`):
//! - `main.rs` — entry, handshake, dispatch loop, execute drive, bus encoding.
//! - `kernel.rs` — the embedded Lua VM, host bindings, and kernel driving.
//! - `error.rs` — `MagError` domain error hierarchy.

mod bridge;
mod error;
mod kernel;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nefor_mag::LoadedProgram;
use nefor_plugin_sdk::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, TransportError};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::bridge::CapabilityBridge;
use crate::error::MagError;
use crate::kernel::{LuaHost, RunCompletion, TeardownReason};

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
    program: Option<Arc<LoadedProgram>>,
}

/// The in-flight async runs, keyed by run_id.
type ActiveExecutes = HashMap<String, ActiveExecute>;

fn run_program<'a>(
    active: &'a ActiveExecutes,
    current: Option<&'a LoadedProgram>,
    run_id: &str,
) -> Option<&'a LoadedProgram> {
    active
        .get(run_id)
        .and_then(|execute| execute.program.as_deref())
        .or(current)
}

/// Outbound/inbound channel capacity for the stdio transport tasks.
const CHANNEL_CAP: usize = 128;

/// NCP version this plugin speaks.
const PROTOCOL_VERSION: &str = "0.1";

/// Plugin name (bus identity is assigned by the engine from spawn-config;
/// this is what we prefix our own event kinds with).
const PLUGIN_NAME: &str = "mag";

/// Plugin version, advertised in `mag.hello`.
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
const GRAPH_MODIFICATION_FORMAT: &str = "nefor.graph-modification/v1";
const GRAPH_DELTA_FORMAT: &str = "nefor.graph-delta/v1";

/// Liveness ping we answer, and the reply kind.
const PING_KIND: &str = "mag.ping";
const PONG_KIND: &str = "mag.pong";

/// Load a MAG program in-process and cache its resident environment for the
/// session; reply with the initial modification (`mag.loaded`).
const LOAD_KIND: &str = "mag.load";
const LOADED_KIND: &str = "mag.loaded";

/// Evaluate a named rule fn against a node output over the cached program;
/// reply with the produced modification (`mag.modification`).
const EVAL_KIND: &str = "mag.eval";

/// Reply kind for a load/eval failure. A rejected modification or a load error
/// is data on the bus, not a plugin crash (ir.md: "the run continues").
const ERROR_KIND: &str = "mag.error";

/// Run the resident (or an inline) program's initial modification through the
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

/// Apply one graph modification directly to the resident kernel — the
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
/// handlers run, so in-flight provider requests are cancelled — the dying
/// llm's `<provider>.chat.cancel` reaches the bus) and settles the pending
/// execute as `mag.run_result status:"killed"`. A kill for a run that is
/// not live is a logged no-op (monotone lifecycles: kill on dead, no-op).
const KILL_RUN_KIND: &str = "mag.kill_run";
const KILL_ALL_RUNS_KIND: &str = "mag.kill_all_runs";
const STEER_RUN_KIND: &str = "mag.steer_run";
const RUN_STEERED_KIND: &str = "mag.run_steered";

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

/// Fallback tool-gate bus name when the spawn config passes no `--tool-gate`.
/// The gate's name is composition-owned (starter/init.lua names the gate when
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
    let contracts = host.registry_contracts().unwrap_or_else(|error| {
        tracing::warn!(%error, "failed to serialize registry contracts");
        Value::Array(Vec::new())
    });
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
    // The session's resident program: loaded once by `mag.load`, then the
    // source of the cached environment every `mag.eval` evaluates against and
    // the default program `mag.execute` runs.
    let mut program: Option<Arc<LoadedProgram>> = None;
    // The in-flight async runs, keyed by run_id (deferred-completion path).
    // Concurrent `mag.execute` requests each hold one entry; each settles
    // independently against its own run-scoped kernel context.
    let mut active: ActiveExecutes = HashMap::new();
    // Capability bridge: drives each provider-class `tool.invoke` as a
    // `chat.*` conversation (correlating the streamed result back; state keyed
    // by chat_id) and rewrites each tool-class `tool.invoke` onto the
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
                                &mut program,
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
/// is rewritten into its `chat.*` conversation, a tool-class `tool.invoke` onto
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

fn drain_rule_triggers(
    host: &LuaHost,
    program: Option<&LoadedProgram>,
    run_id: &str,
) -> Result<(), MagError> {
    while let Some(trigger) = host.take_rule_trigger(run_id)? {
        let Some(program) = program else {
            host.fail_run(
                run_id,
                "rule firing requires the resident program that declared the rule",
            )?;
            break;
        };
        let result = nefor_mag::eval_fn(program, &trigger.function, trigger.value)
            .map_err(|error| format!("rule {:?} evaluation failed: {error}", trigger.rule_id))
            .and_then(|artifact| {
                serde_json::to_value(artifact).map_err(|error| {
                    format!("rule {:?} artifact serialize: {error}", trigger.rule_id)
                })
            })
            .and_then(|artifact| artifact_data(&artifact, GRAPH_DELTA_FORMAT, "rule delta"))
            .and_then(|delta| {
                let outcome = host.apply(run_id, &delta).map_err(|error| {
                    format!("rule {:?} delta apply failed: {error}", trigger.rule_id)
                })?;
                if outcome.ok {
                    tracing::info!(
                        run_id,
                        rule_id = %trigger.rule_id,
                        source_actor = %trigger.source_actor,
                        source_wire = %trigger.source_wire,
                        emission_seq = trigger.emission_seq,
                        "rule delta applied"
                    );
                    Ok(())
                } else {
                    Err(format!(
                        "rule {:?} delta rejected: {}",
                        trigger.rule_id,
                        outcome.error.unwrap_or_else(|| "unknown rejection".into())
                    ))
                }
            });
        if let Err(error) = result {
            host.fail_run(run_id, &error)?;
            break;
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
    program: Option<&LoadedProgram>,
    run_id: &str,
) -> Result<(), MagError> {
    if !active.contains_key(run_id) {
        return Ok(());
    }
    let pinned_program = run_program(active, program, run_id);
    drain_rule_triggers(host, pinned_program, run_id)?;
    flush_emits(out_tx, host, bridge).await?;
    // The teardown reason rides the reap's `mag.actor_killed` events so
    // consumers can tell a completed run's bookkeeping sweep from a real
    // termination.
    let (mut reply, reason) = if let Some(rc) = host.take_run_complete(run_id)? {
        (
            run_result_ok(None, run_id, &rc),
            TeardownReason::RunComplete,
        )
    } else if let Some(error) = host.take_run_failed(run_id)? {
        (
            run_result_failed(None, run_id, &error),
            TeardownReason::RunFailed,
        )
    } else {
        return Ok(());
    };
    let a = active.remove(run_id).expect("checked above");
    if let Some(id) = a.in_reply_to {
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
                ),
            )
            .await?;
        }
    }
    Ok(())
}

/// Handle one inbound event body. We answer `mag.ping` (liveness), `mag.load`
/// (load a program, cache it, reply with the initial modification), and
/// `mag.eval` (apply a rule fn over the cached program). Everything else on the
/// broadcast bus is not ours and drops silently.
async fn handle_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    source: &str,
    body: &Map<String, Value>,
    program: &mut Option<Arc<LoadedProgram>>,
    host: &LuaHost,
    active: &mut ActiveExecutes,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(k) => k,
        None => return Ok(()),
    };
    // Provider bridge inbound: a driven chat's streamed completion result (or
    // error), correlated by chat_id. Feeds the single final result back through
    // the kernel's capability channel. A reply for a chat we don't drive resolves
    // to nothing and is ignored inside the bridge, so this is safe to check ahead
    // of the mag protocol arms (the suffixes never collide with a mag.* kind).
    if CapabilityBridge::is_provider_reply(kind) {
        return handle_provider_reply(out_tx, kind, body, program.as_deref(), host, active, bridge)
            .await;
    }
    let in_reply_to = body.get("id").and_then(Value::as_str);
    match kind {
        PING_KIND => send_event(out_tx, pong_body(in_reply_to)).await,
        LOAD_KIND => handle_load(out_tx, body, in_reply_to, program, host).await,
        EVAL_KIND => handle_eval(out_tx, body, in_reply_to, program).await,
        EXECUTE_KIND => {
            handle_execute(
                out_tx,
                source,
                body,
                in_reply_to,
                program,
                (host, active, bridge),
            )
            .await
        }
        APPLY_KIND => {
            handle_apply(
                out_tx,
                body,
                in_reply_to,
                program.as_deref(),
                host,
                active,
                bridge,
            )
            .await
        }
        KILL_RUN_KIND => handle_kill_run(out_tx, body, host, active, bridge).await,
        KILL_ALL_RUNS_KIND => handle_kill_all_runs(out_tx, host, active, bridge).await,
        STEER_RUN_KIND => handle_steer_run(out_tx, body, host, active).await,
        INTERRUPT_RUN_KIND => {
            handle_interrupt_run(out_tx, body, program.as_deref(), host, active, bridge).await
        }
        // A capability response correlated to a kernel-minted request id.
        // Unknown ids are dropped inside the kernel (no open correlation), so
        // forwarding every tool.result while any run is live is safe.
        TOOL_RESULT_KIND if !active.is_empty() => {
            handle_tool_result(out_tx, body, program.as_deref(), host, active, bridge).await
        }
        _ => Ok(()),
    }
}

/// Load `source_dir/entry` in-process, cache the resident program for the
/// session, and reply with its initial modification. A load failure replies
/// `mag.error` and leaves any previously-loaded program untouched.
async fn handle_load(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    in_reply_to: Option<&str>,
    program: &mut Option<Arc<LoadedProgram>>,
    host: &LuaHost,
) -> Result<(), MagError> {
    let source_dir = match body.get("source_dir").and_then(Value::as_str) {
        Some(s) => s,
        None => {
            return send_event(
                out_tx,
                error_body(in_reply_to, "mag.load missing source_dir"),
            )
            .await
        }
    };
    let entry = match body.get("entry").and_then(Value::as_str) {
        Some(s) => s,
        None => return send_event(out_tx, error_body(in_reply_to, "mag.load missing entry")).await,
    };
    let module_roots = match load_module_roots(body, Path::new(source_dir)) {
        Ok(roots) => roots,
        Err(error) => return send_event(out_tx, error_body(in_reply_to, &error)).await,
    };

    let contracts = host
        .registry_contracts()
        .unwrap_or_else(|_| Value::Array(Vec::new()));
    let inputs = serde_json::json!({ "foreign_contracts": contracts });
    match nefor_mag::load_with_inputs_and_module_roots(
        Path::new(source_dir),
        entry,
        inputs,
        &module_roots,
    ) {
        Ok(loaded) => {
            let artifact = match serde_json::to_value(&loaded.artifact) {
                Ok(value) => value,
                Err(error) => {
                    return send_event(
                        out_tx,
                        error_body(in_reply_to, &format!("artifact serialize: {error}")),
                    )
                    .await
                }
            };
            if let Err(error) = validate_loaded_rules(&loaded, &artifact) {
                return send_event(out_tx, error_body(in_reply_to, &error)).await;
            }
            // The registry's factory names ride along so the control plane can
            // validate reasoner/factory types against the kernel's source of
            // truth instead of a hand-synced allowlist.
            let factories = host.registry_names().unwrap_or_default();
            let contracts = host
                .registry_contracts()
                .unwrap_or_else(|_| Value::Array(Vec::new()));
            let reply = loaded_body(in_reply_to, &loaded.hash, artifact, &factories, contracts);
            *program = Some(Arc::new(loaded));
            send_event(out_tx, reply).await
        }
        Err(e) => send_event(out_tx, error_body(in_reply_to, &e.to_string())).await,
    }
}

fn validate_loaded_rules(program: &LoadedProgram, artifact: &Value) -> Result<(), String> {
    if artifact.get("format").and_then(Value::as_str) != Some(GRAPH_MODIFICATION_FORMAT) {
        return Ok(());
    }
    let data = artifact_data(artifact, GRAPH_MODIFICATION_FORMAT, "loaded program")?;
    let rules = data
        .get("rules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for rule in rules {
        let id = rule
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<malformed>");
        let function = rule
            .get("fn")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("rule {id:?} missing function name"))?;
        let input = rule
            .get("on")
            .and_then(|on| on.get("type"))
            .ok_or_else(|| format!("rule {id:?} missing source semantic type"))?;
        nefor_mag::validate_rule_fn_input(program, function, input)
            .map_err(|error| format!("rule {id:?}: {error}"))?;
    }
    Ok(())
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

/// Run a program through the kernel. The modification is taken inline from the
/// request (`modification` — the control plane reaches kernel ops directly,
/// ir.md) or, absent that, from the session's resident program (`mag.load`).
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
    program: &Option<Arc<LoadedProgram>>,
    runtime: (&LuaHost, &mut ActiveExecutes, &mut CapabilityBridge),
) -> Result<(), MagError> {
    let (host, active, bridge) = runtime;
    let mut modification: Value = match body.get("artifact") {
        Some(artifact) => match artifact_modification(artifact) {
            Ok(m) => m,
            Err(error) => return send_event(out_tx, error_body(in_reply_to, &error)).await,
        },
        None => match program {
            Some(p) => match serde_json::to_value(&p.artifact) {
                Ok(artifact) => match artifact_modification(&artifact) {
                    Ok(m) => m,
                    Err(error) => return send_event(out_tx, error_body(in_reply_to, &error)).await,
                },
                Err(e) => {
                    return send_event(
                        out_tx,
                        error_body(in_reply_to, &format!("resident artifact serialize: {e}")),
                    )
                    .await
                }
            },
            None => {
                return send_event(
                    out_tx,
                    error_body(
                        in_reply_to,
                        "mag.execute before any mag.load and no inline artifact",
                    ),
                )
                .await
            }
        },
    };
    if body.get("artifact").is_some()
        && modification
            .get("rules")
            .and_then(Value::as_array)
            .is_some_and(|rules| !rules.is_empty())
    {
        return send_event(
            out_tx,
            error_body(
                in_reply_to,
                "inline execution with rules is rejected; load the declaring resident MAG program",
            ),
        )
        .await;
    }

    // Apply the control plane's per-actor params overlay before spawn. Actor
    // params are kernel-opaque data owned by the factory (docs/ir.md), so an
    // overlay patched at apply time is legitimate control-plane input — it is
    // how the lead threads resolved profile params (provider/model/reasoning)
    // into the program without re-authoring the modification. Shallow per-actor
    // top-level merge; unknown ids are ignored (a race artifact, not an error).
    if let Some(overlay) = body.get("params_overlay").and_then(Value::as_object) {
        if let Err(error) = apply_params_overlay(&mut modification, overlay) {
            return send_event(out_tx, error_body(in_reply_to, &error)).await;
        }
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

    let begun = host.begin_run_with_principal(
        &run_id,
        run_name,
        Some(session_id),
        Some(principal.as_str()),
    )?;
    // The kernel reaps stale contexts from a previous session at the boundary
    // (begin_run); fail their pending replies before driving the new run.
    settle_reaped(out_tx, host, active, bridge, &begun.reaped).await?;
    if !begun.ok {
        let msg = begun.error.unwrap_or_else(|| "begin_run failed".into());
        return send_event(out_tx, run_result_failed(in_reply_to, &run_id, &msg)).await;
    }
    let outcome = host.start(&run_id, &modification)?;
    drain_rule_triggers(host, program.as_deref(), &run_id)?;
    flush_emits(out_tx, host, bridge).await?;

    // A failed apply / rejected initial modification: nothing useful spawned;
    // drop the context.
    if !outcome.ok {
        let msg = outcome.error.unwrap_or_else(|| "start failed".into());
        send_event(out_tx, run_result_failed(in_reply_to, &run_id, &msg)).await?;
        host.end_run(&run_id, TeardownReason::RunFailed)?;
        return flush_emits(out_tx, host, bridge).await;
    }

    // Synchronous terminal state inside `start`: reply now and tear the run's
    // context down (reaping any still-live actors — e.g. a parallel branch —
    // through the fold).
    let terminal = if let Some(rc) = host.take_run_complete(&run_id)? {
        Some((
            run_result_ok(in_reply_to, &run_id, &rc),
            TeardownReason::RunComplete,
        ))
    } else {
        host.take_run_failed(&run_id)?.map(|error| {
            (
                run_result_failed(in_reply_to, &run_id, &error),
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
            program: program.clone(),
        },
    );
    Ok(())
}

/// Merge a per-actor params overlay into a modification's actors before spawn.
/// The overlay is `{ actor_id: { param: value, ... }, ... }`. Each patch is a
/// shallow top-level merge into the matching actor's `params` (created if
/// absent, replaced if non-object). Actors not named in the overlay are
/// untouched; overlay keys with no matching actor are ignored.
fn apply_params_overlay(
    modification: &mut Value,
    overlay: &Map<String, Value>,
) -> Result<(), String> {
    let actors = match modification.get_mut("actors").and_then(Value::as_array_mut) {
        Some(a) => a,
        None => return Ok(()),
    };
    for actor in actors.iter_mut() {
        let obj = match actor.as_object_mut() {
            Some(o) => o,
            None => continue,
        };
        let id = match obj.get("id").and_then(Value::as_str) {
            Some(id) => id.to_owned(),
            None => continue,
        };
        let patch = match overlay.get(&id).and_then(Value::as_object) {
            Some(p) => p,
            None => continue,
        };
        let factory = obj.get("factory").and_then(Value::as_str);
        let protected_params: &[&str] = match factory {
            Some("structured-output" | "nefor.factory.structured-output") => &[
                "schema",
                "output_type",
                "error_type",
                "provider_error_type",
                "validation_error_type",
            ],
            _ => &[],
        };
        if let Some(param) = protected_params
            .iter()
            .find(|param| patch.contains_key(**param))
        {
            return Err(format!(
                "params_overlay for actor {id:?} cannot replace protected compiler-derived param {param:?}"
            ));
        }
        let params = obj
            .entry("params")
            .or_insert_with(|| Value::Object(Map::new()));
        if !params.is_object() {
            *params = Value::Object(Map::new());
        }
        if let Some(pobj) = params.as_object_mut() {
            for (k, v) in patch {
                pobj.insert(k.clone(), v.clone());
            }
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

/// Apply one graph modification directly to the resident kernel — the control
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
    program: Option<&LoadedProgram>,
    host: &LuaHost,
    active: &mut ActiveExecutes,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
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
            1 => active.keys().next().expect("len checked").clone(),
            _ => {
                return send_event(
                    out_tx,
                    applied_body(in_reply_to, false, Some(APPLY_AMBIGUOUS_RUN)),
                )
                .await
            }
        },
    };

    let modification = match body.get("modification") {
        Some(m @ Value::Object(_)) => m.clone(),
        _ => {
            return send_event(
                out_tx,
                error_body(in_reply_to, "mag.apply modification must be an object"),
            )
            .await
        }
    };

    // Audit every accepted apply with its declared source (docs/ir.md: the
    // modification log is the run — the plane's mid-run ops are part of it).
    let source = body
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("<unspecified>");
    tracing::info!(source = %source, run_id = %run_id, "mag.apply accepted");

    let state = host.apply(&run_id, &modification)?;
    // Forward the lifecycle events + any abort/cancel envelopes the apply queued.
    // A kill hands the dying `llm` its final message — the `<provider>.chat.cancel`
    // envelope keyed by the chat_id handle (factories/llm.lua handle_kill) — which
    // takes the raw-emit path to the kernel's queue (routing.lua on_emit) and
    // reaches the bus here verbatim: not a `tool.invoke`, so the bridge forwards
    // it untouched to the provider that owns that chat.
    flush_emits(out_tx, host, bridge).await?;
    send_event(
        out_tx,
        applied_body(in_reply_to, state.ok, state.error.as_deref()),
    )
    .await?;
    // A modification that completes the run (e.g. a send that unblocks the sink)
    // settles that run's in-flight execute reply.
    settle_run(out_tx, host, active, bridge, program, &run_id).await
}

/// Kill one live run: end its kernel context — reaping its actors through
/// the fold, so kill handlers run and their abort/cancel envelopes (a
/// mid-flight llm's `<provider>.chat.cancel`) reach the bus — then settle
/// the pending execute with `mag.run_result status:"killed"`. The cancel
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
    send_event(out_tx, run_result_killed(a.in_reply_to.as_deref(), run_id)).await
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
        send_event(out_tx, run_result_killed(a.in_reply_to.as_deref(), &run_id)).await?;
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
    program: Option<&LoadedProgram>,
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
        // A dispatched sub-run: cancel the in-flight work then reap the run so
        // no actor re-fires, and settle the pending execute FAILED. The cancels
        // (our explicit `tool.cancel` + the reap's kill-time aborts) flush
        // BEFORE the terminal reply — a consumer that treats the reply as "run
        // closed" observes the aborts first.
        let a = match active.remove(&run_id) {
            Some(a) => a,
            None => {
                tracing::info!(run_id = %run_id, "mag.interrupt_run(terminate) for a run that is not live; no-op");
                return Ok(());
            }
        };
        let (cancelled, _) = host.interrupt_run(&run_id, INTERRUPT_FAILURE, true)?;
        host.end_run(&run_id, TeardownReason::RunFailed)?;
        tracing::info!(run_id = %run_id, cancelled, "mag.interrupt_run(terminate): in-flight cancelled, run ended failed");
        flush_emits(out_tx, host, bridge).await?;
        return send_event(
            out_tx,
            run_result_failed(a.in_reply_to.as_deref(), &run_id, INTERRUPT_FAILURE),
        )
        .await;
    }

    if !active.contains_key(&run_id) {
        tracing::info!(run_id = %run_id, "mag.interrupt_run for a run that is not live; no-op");
        return Ok(());
    }
    let (settled, _) = host.interrupt_run(&run_id, INTERRUPT_FAILURE, false)?;
    tracing::info!(run_id = %run_id, settled, "mag.interrupt_run: in-flight capabilities settled as interrupted");
    flush_emits(out_tx, host, bridge).await?;
    // The run stays alive on the tool leg (re-fire pending); a provider-leg
    // interrupt may have failed it synchronously — settle if so.
    settle_run(out_tx, host, active, bridge, program, &run_id).await
}

/// Route a capability response into the kernel (unblocking a deferred
/// activation in the owning run's context), forward whatever it produced, and
/// settle that run if it completed. Correlation ids are run-scoped, so the
/// kernel names the run the response advanced.
async fn handle_tool_result(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    program: Option<&LoadedProgram>,
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
    let advanced = host.bus_response(id, result, error)?;
    flush_emits(out_tx, host, bridge).await?;
    match advanced {
        Some(run_id) => settle_run(out_tx, host, active, bridge, program, &run_id).await,
        None => Ok(()),
    }
}

/// Route a provider bridge reply (a driven chat's `chat.complete.result` or
/// `chat.error`) into the kernel as the correlated capability response. The
/// single final result feeds `kernel.bus_response`; a `chat.delete` then frees
/// the provider-side chat. The reply may re-fire the requesting actor (a next
/// turn → a fresh provider `tool.invoke` the bridge drives again) or complete the
/// run, so forward whatever it produced and settle. A reply for a chat we don't
/// drive resolves to `None` and is a no-op.
async fn handle_provider_reply(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    kind: &str,
    body: &Map<String, Value>,
    program: Option<&LoadedProgram>,
    host: &LuaHost,
    active: &mut ActiveExecutes,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    let reply = match bridge.take_reply(kind, body) {
        Some(r) => r,
        None => return Ok(()),
    };
    let advanced = host.bus_response(
        &reply.request_id,
        reply.result.as_ref(),
        reply.error.as_deref(),
    )?;
    send_event(
        out_tx,
        bridge::delete_envelope(&reply.provider, &reply.chat_id),
    )
    .await?;
    flush_emits(out_tx, host, bridge).await?;
    match advanced {
        Some(run_id) => settle_run(out_tx, host, active, bridge, program, &run_id).await,
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

/// Evaluate the named rule fn against `input` over the cached program, replying
/// with the produced modification. No cached program, an unknown fn, a budget
/// overrun, or an ill-shaped result all reply `mag.error`.
async fn handle_eval(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    in_reply_to: Option<&str>,
    program: &mut Option<Arc<LoadedProgram>>,
) -> Result<(), MagError> {
    let name = match body.get("name").and_then(Value::as_str) {
        Some(s) => s,
        None => return send_event(out_tx, error_body(in_reply_to, "mag.eval missing name")).await,
    };
    let input = body.get("input").cloned().unwrap_or(Value::Null);

    let loaded = match program {
        Some(p) => p,
        None => {
            return send_event(
                out_tx,
                error_body(
                    in_reply_to,
                    "mag.eval before any mag.load: no resident program",
                ),
            )
            .await
        }
    };

    match nefor_mag::eval_fn(loaded, name, input) {
        Ok(artifact) => {
            let reply = match serde_json::to_value(&artifact) {
                Ok(value) => artifact_body(in_reply_to, value),
                Err(e) => error_body(in_reply_to, &format!("artifact serialize: {e}")),
            };
            send_event(out_tx, reply).await
        }
        Err(e) => send_event(out_tx, error_body(in_reply_to, &e.to_string())).await,
    }
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
    m.insert("foreign_contracts".into(), contracts);
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
    m.insert("foreign_contracts".into(), contracts);
    m
}

/// Validate the generic host artifact boundary and bind qualified foreign
/// actor identities to the kernel's existing `factory` field. The kernel sees
/// the same graph-modification IR and therefore retains all firing, lifecycle,
/// and defensive contract checks unchanged.
fn artifact_data(artifact: &Value, expected_format: &str, context: &str) -> Result<Value, String> {
    let object = artifact
        .as_object()
        .ok_or_else(|| format!("{context} artifact must be an object"))?;
    let format = object
        .get("format")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{context} artifact missing string format"))?;
    if format != expected_format {
        return Err(format!(
            "{context} must use artifact format {expected_format:?}, got {format:?}"
        ));
    }
    let mut data = object
        .get("data")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| format!("{context} artifact data must be an object"))?;
    if let Some(actors) = data.get_mut("actors").and_then(Value::as_array_mut) {
        for actor in actors {
            let spec = actor
                .as_object_mut()
                .ok_or_else(|| "artifact actors must be objects".to_owned())?;
            if let Some(foreign) = spec.remove("foreign") {
                if !foreign.is_string() {
                    return Err("artifact actor foreign identity must be a string".to_owned());
                }
                if spec.insert("factory".to_owned(), foreign).is_some() {
                    return Err("artifact actor cannot declare both foreign and factory".to_owned());
                }
            }
        }
    }
    Ok(Value::Object(data))
}

fn artifact_modification(artifact: &Value) -> Result<Value, String> {
    artifact_data(artifact, GRAPH_MODIFICATION_FORMAT, "mag.execute")
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
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(RUN_RESULT_KIND.into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    m.insert("run_id".into(), Value::String(run_id.to_owned()));
    m.insert("status".into(), Value::String("completed".into()));
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
fn run_result_failed(in_reply_to: Option<&str>, run_id: &str, error: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(RUN_RESULT_KIND.into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    m.insert("run_id".into(), Value::String(run_id.to_owned()));
    m.insert("status".into(), Value::String("failed".into()));
    m.insert("error".into(), Value::String(error.to_owned()));
    m
}

/// Terminal run reply for a control-plane kill (`mag.kill_run`): the pending
/// execute settles as status "killed". Distinct from "failed" so consumers
/// treat it as "turn aborted" (no history append, no error surface), not as
/// an error.
fn run_result_killed(in_reply_to: Option<&str>, run_id: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(RUN_RESULT_KIND.into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    m.insert("run_id".into(), Value::String(run_id.to_owned()));
    m.insert("status".into(), Value::String("killed".into()));
    m
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

fn artifact_body(in_reply_to: Option<&str>, artifact: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("mag.artifact".into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    m.insert("artifact".into(), artifact);
    m
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn execute_principal_validates_shipped_routes_and_defaults_custom_routes_untrusted() {
        assert_eq!(
            authoritative_principal("agentic-loop", Some(&serde_json::json!("lead"))),
            Ok(RunPrincipal::Lead)
        );
        assert_eq!(
            authoritative_principal("lead-workflow", Some(&serde_json::json!("subagent"))),
            Ok(RunPrincipal::Subagent)
        );
        for (source, principal) in [
            ("agentic-loop", None),
            ("agentic-loop", Some(serde_json::json!("subagent"))),
            ("lead-workflow", Some(serde_json::json!("lead"))),
        ] {
            assert!(
                authoritative_principal(source, principal.as_ref()).is_err(),
                "shipped source {source:?} must reject {principal:?}"
            );
        }
        for (source, principal) in [
            ("engine", None),
            ("engine", Some(serde_json::json!("lead"))),
            ("direct-tool", Some(serde_json::json!("lead"))),
            ("custom-runner", Some(serde_json::json!("subagent"))),
        ] {
            assert_eq!(
                authoritative_principal(source, principal.as_ref()),
                Ok(RunPrincipal::Untrusted),
                "custom source {source:?} must execute without notice authority"
            );
        }
    }

    #[test]
    fn hello_body_advertises_version_and_kernel() {
        let contracts = serde_json::json!([{"identity": "nefor.factory.llm"}]);
        let b = hello_body(
            Some("mag-kernel"),
            &["sink".to_owned(), "llm".to_owned()],
            contracts,
        );
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.hello"));
        assert_eq!(
            b.get("version").and_then(Value::as_str),
            Some(PLUGIN_VERSION)
        );
        assert_eq!(b.get("kernel").and_then(Value::as_str), Some("mag-kernel"));
        let factories = b
            .get("factories")
            .and_then(Value::as_array)
            .expect("hello advertises factories");
        assert!(factories.iter().any(|f| f.as_str() == Some("sink")));
        assert_eq!(b["foreign_contracts"][0]["identity"], "nefor.factory.llm");
    }

    #[test]
    fn hello_body_omits_kernel_when_absent() {
        let b = hello_body(None, &[], Value::Array(Vec::new()));
        assert!(b.get("kernel").is_none());
        // Factories always present, even when empty.
        assert_eq!(
            b.get("factories").and_then(Value::as_array).map(Vec::len),
            Some(0)
        );
    }

    #[test]
    fn pong_body_echoes_in_reply_to() {
        let b = pong_body(Some("ping-1"));
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.pong"));
        assert_eq!(b.get("in_reply_to").and_then(Value::as_str), Some("ping-1"));
    }

    #[test]
    fn goodbye_body_carries_reason() {
        let b = goodbye_body();
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.goodbye"));
        assert!(b.get("reason").and_then(Value::as_str).is_some());
    }

    #[test]
    fn loaded_body_carries_hash_and_artifact() {
        let b = loaded_body(
            Some("load-1"),
            "sha256:abc",
            serde_json::json!({"format": GRAPH_MODIFICATION_FORMAT, "data": {"actors": []}}),
            &["stub".to_owned(), "sink".to_owned()],
            serde_json::json!([{"identity": "nefor.factory.stub"}]),
        );
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.loaded"));
        assert_eq!(b.get("in_reply_to").and_then(Value::as_str), Some("load-1"));
        assert_eq!(b.get("hash").and_then(Value::as_str), Some("sha256:abc"));
        assert!(b.get("artifact").and_then(Value::as_object).is_some());
        let factories = b
            .get("factories")
            .and_then(Value::as_array)
            .expect("factories");
        assert!(factories.iter().any(|f| f.as_str() == Some("sink")));
        assert_eq!(b["foreign_contracts"][0]["identity"], "nefor.factory.stub");
    }

    #[test]
    fn load_module_roots_default_to_source_dir_only() {
        let source = std::env::temp_dir().join(format!("mag-roots-default-{}", std::process::id()));
        std::fs::create_dir_all(&source).expect("source dir");
        let roots = load_module_roots(&Map::new(), &source).expect("default roots");
        assert_eq!(roots, vec![source.clone()]);
        std::fs::remove_dir_all(source).ok();
    }

    #[test]
    fn load_module_roots_accept_explicit_absolute_and_contained_roots() {
        let source =
            std::env::temp_dir().join(format!("mag-roots-explicit-{}", std::process::id()));
        let contained = source.join("lib");
        let external =
            std::env::temp_dir().join(format!("mag-roots-external-{}", std::process::id()));
        std::fs::create_dir_all(&contained).expect("contained root");
        std::fs::create_dir_all(&external).expect("external root");
        let body = serde_json::json!({"module_roots": ["lib", external.to_string_lossy()]})
            .as_object()
            .cloned()
            .expect("body");
        let roots = load_module_roots(&body, &source).expect("explicit roots");
        assert_eq!(
            roots[0],
            contained.canonicalize().expect("contained canonical")
        );
        assert_eq!(
            roots[1],
            external.canonicalize().expect("external canonical")
        );
        std::fs::remove_dir_all(source).ok();
        std::fs::remove_dir_all(external).ok();
    }

    #[test]
    fn load_module_roots_reject_relative_escape() {
        let source = std::env::temp_dir().join(format!("mag-roots-escape-{}", std::process::id()));
        std::fs::create_dir_all(&source).expect("source dir");
        let body = serde_json::json!({"module_roots": [".."]})
            .as_object()
            .cloned()
            .expect("body");
        assert!(load_module_roots(&body, &source)
            .expect_err("escape rejected")
            .contains("escapes source_dir"));
        std::fs::remove_dir_all(source).ok();
    }

    #[test]
    fn artifact_boundary_normalizes_qualified_foreign_identity() {
        let artifact = serde_json::json!({
            "format": GRAPH_MODIFICATION_FORMAT,
            "data": {
                "actors": [{"id": "answer", "foreign": "nefor.factory.llm"}],
                "messages": [], "kills": [], "rules": []
            }
        });
        let modification = artifact_modification(&artifact).expect("valid artifact");
        assert_eq!(modification["actors"][0]["factory"], "nefor.factory.llm");
        assert!(modification["actors"][0].get("foreign").is_none());
    }

    #[test]
    fn artifact_boundary_preserves_structural_result_metadata() {
        let artifact = serde_json::json!({
            "format": GRAPH_MODIFICATION_FORMAT,
            "data": {
                "actors": [{"id": "answer", "foreign": "nefor.factory.llm", "routes": {}}],
                "messages": [], "kills": [], "rules": [],
                "result": {"from": {
                    "actor": "answer",
                    "type": "audit.CodeAudit",
                    "wire": "generic-provider.FinalAnswer"
                }}
            }
        });
        let modification = artifact_modification(&artifact).expect("valid artifact");
        assert_eq!(modification["result"]["from"]["actor"], "answer");
        assert_eq!(
            modification["result"]["from"]["wire"],
            "generic-provider.FinalAnswer"
        );
        assert_eq!(modification["actors"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn artifact_boundary_rejects_unknown_format() {
        let artifact = serde_json::json!({"format": "other/v1", "data": {}});
        assert!(artifact_modification(&artifact)
            .expect_err("format must be rejected")
            .contains("must use artifact format"));
    }

    #[test]
    fn resident_rule_expands_a_typed_value_into_an_atomic_delta() {
        let root = std::env::temp_dir().join(format!("mag-rule-expansion-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("workspace");
        fs::write(
            root.join("main.mag"),
            r#"
              (type Task {:task String})
              (type ExpandedResult {:greeting String})
              (def task-name (fn [[task Task]] -> String (get task "task")))
              (def expand (fn [[task Task]] -> Artifact
                (artifact "nefor.graph-delta/v1"
                  {:actors []
                   :messages [{:to "middle" :content {:kind "stub.In" :task (task-name task)}}]
                   :kills []
                   :rules []})))
              (def finish (fn [[task Task]] -> Artifact
                (artifact "nefor.graph-delta/v1"
                  {:actors []
                   :messages [{:to "result" :content {:kind "stub.In" :task task}}]
                   :kills []
                   :rules []})))
              (artifact "nefor.graph-modification/v1"
                {:actors [
                  {:id "source" :foreign "nefor.factory.stub"
                   :params {:value {:task "one"}} :routes {}}
                  {:id "middle" :foreign "nefor.factory.stub"
                   :params {:value {:task "nested"}} :routes {}}
                  {:id "result" :foreign "nefor.factory.stub"
                   :params {:greeting "expanded"} :routes {}}]
                 :messages [{:to "source" :content {:kind "stub.In"}}]
                 :kills []
                 :rules [{:id "expand" :on {:actor "source" :type (type-evidence (type-tag Task)) :wire "stub.Out"}
                          :fn "expand"}
                         {:id "finish" :on {:actor "middle" :type (type-evidence (type-tag Task)) :wire "stub.Out"}
                          :fn "finish"}]
                 :result {:from {:actor "result" :type (type-evidence (type-tag ExpandedResult)) :wire "stub.Out"}}})
            "#,
        )
        .expect("program");
        let module_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../starter/mag/lib");
        let program = nefor_mag::load_with_inputs_and_module_roots(
            &root,
            "main.mag",
            serde_json::json!({}),
            &[root.clone(), module_root],
        )
        .expect("load program");
        let artifact = serde_json::to_value(&program.artifact).expect("artifact json");
        validate_loaded_rules(&program, &artifact).expect("rules valid");
        let modification = artifact_modification(&artifact).expect("normalize graph");

        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let host = LuaHost::load_kernel(
            &manifest.join("lua/mag-kernel/init.lua"),
            Some(&manifest.join("../../lua")),
        )
        .expect("kernel");
        assert!(
            host.begin_run("rule-e2e", "rule-e2e", None)
                .expect("begin")
                .ok
        );
        let started = host.start("rule-e2e", &modification).expect("start");
        assert!(started.ok, "start failed: {:?}", started.error);
        assert!(host
            .take_run_complete("rule-e2e")
            .expect("premature completion")
            .is_none());

        drain_rule_triggers(&host, Some(&program), "rule-e2e").expect("drain rules");
        let completion = host
            .take_run_complete("rule-e2e")
            .expect("completion")
            .unwrap_or_else(|| {
                let failure = host.take_run_failed("rule-e2e").expect("rule run failure");
                panic!("delta did not fire static result actor: {failure:?}")
            });
        assert_eq!(
            completion
                .result
                .as_ref()
                .and_then(|result| result["greeting"].as_str()),
            Some("expanded")
        );
        assert!(host
            .take_rule_trigger("rule-e2e")
            .expect("quiescent")
            .is_none());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn params_overlay_patches_named_actors_only() {
        let mut modification = serde_json::json!({
            "actors": [
                { "id": "build", "factory": "llm", "params": { "prompt": "x" }, "routes": {} },
                { "id": "sink",  "factory": "sink", "params": {}, "routes": {} }
            ]
        });
        let overlay = serde_json::json!({
            "build": { "provider": "chatgpt", "model": "gpt-5.5", "reasoning_effort": "high" }
        });
        apply_params_overlay(&mut modification, overlay.as_object().unwrap()).unwrap();

        let actors = modification["actors"].as_array().unwrap();
        let build = &actors[0]["params"];
        assert_eq!(build["prompt"].as_str(), Some("x"), "existing param kept");
        assert_eq!(
            build["provider"].as_str(),
            Some("chatgpt"),
            "overlay merged"
        );
        assert_eq!(build["reasoning_effort"].as_str(), Some("high"));
        // The unnamed actor is untouched.
        assert!(actors[1]["params"].as_object().unwrap().is_empty());
    }

    #[test]
    fn params_overlay_creates_params_when_missing_or_non_object() {
        let mut modification = serde_json::json!({
            "actors": [ { "id": "a", "factory": "llm" } ]
        });
        let overlay = serde_json::json!({ "a": { "model": "m" } });
        apply_params_overlay(&mut modification, overlay.as_object().unwrap()).unwrap();
        assert_eq!(
            modification["actors"][0]["params"]["model"].as_str(),
            Some("m")
        );
    }

    #[test]
    fn params_overlay_cannot_replace_compiler_derived_params() {
        let original = serde_json::json!({"version": 1, "root": {"kind": "string"}});
        let mut modification = serde_json::json!({
            "actors": [
                {
                    "id": "typed",
                    "factory": "nefor.factory.structured-output",
                    "params": {
                        "schema": original,
                        "provider": "mock-provider",
                        "output_type": "output-id",
                        "error_type": "agent-error-id",
                        "provider_error_type": "provider-id",
                        "validation_error_type": "validation-id"
                    }
                }
            ]
        });
        for (actor, param, value) in [
            (
                "typed",
                "schema",
                serde_json::json!({"version": 1, "root": {"kind": "string"}}),
            ),
            ("typed", "output_type", serde_json::json!("forged")),
            ("typed", "error_type", serde_json::json!("forged")),
            ("typed", "provider_error_type", serde_json::json!("forged")),
            (
                "typed",
                "validation_error_type",
                serde_json::json!("forged"),
            ),
        ] {
            let overlay = serde_json::json!({(actor): {(param): value}});
            let error =
                apply_params_overlay(&mut modification, overlay.as_object().unwrap()).unwrap_err();
            assert!(error.contains(&format!("protected compiler-derived param {param:?}")));
        }
        assert_eq!(modification["actors"][0]["params"]["schema"], original);
        assert_eq!(
            modification["actors"][0]["params"]["provider_error_type"],
            "provider-id"
        );
        assert_eq!(
            modification["actors"][0]["params"]["validation_error_type"],
            "validation-id"
        );
        assert_eq!(
            modification["actors"][0]["params"]["output_type"],
            "output-id"
        );
        assert_eq!(
            modification["actors"][0]["params"]["error_type"],
            "agent-error-id"
        );
    }

    #[test]
    fn error_body_carries_message() {
        let b = error_body(Some("req-9"), "boom");
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.error"));
        assert_eq!(b.get("in_reply_to").and_then(Value::as_str), Some("req-9"));
        assert_eq!(b.get("message").and_then(Value::as_str), Some("boom"));
    }
}
