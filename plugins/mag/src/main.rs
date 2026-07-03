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

use std::path::{Path, PathBuf};

use nefor_mag::LoadedProgram;
use nefor_plugin_sdk::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, TransportError};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::bridge::CapabilityBridge;
use crate::error::MagError;
use crate::kernel::{LuaHost, RunCompletion};

/// A run driven asynchronously to completion: the execute reply is deferred
/// until the resident program signals `mag.run_complete` or `mag.run_failed`
/// (via inbound capability responses that unblock deferred activations).
/// Synchronous programs (the shipped factories) finish inside the
/// `mag.execute` call and never register one.
struct ActiveExecute {
    /// The `mag.execute` request id to correlate the terminal reply to.
    in_reply_to: Option<String>,
    /// The run's id, echoed on the reply.
    run_id: String,
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
const MODIFICATION_KIND: &str = "mag.modification";

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
    let kernel_path = resolve_kernel_path()?;
    let lua_root = arg_value("--lua-root").map(PathBuf::from);
    tracing::info!(path = %kernel_path.display(), "loading mag kernel");
    let host = LuaHost::load_kernel(&kernel_path, lua_root.as_deref())?;

    // Advertise the registry's factory names on hello so the control plane has
    // a validation snapshot from plugin startup — the source of truth for
    // reasoner/factory types on both execute paths (replaces a hand-synced
    // allowlist; docs/ir.md division-of-responsibility).
    let factories = host.registry_names().unwrap_or_default();
    send_event(
        &out_tx,
        hello_body(host.kernel_name().as_deref(), &factories),
    )
    .await?;

    run_dispatch_loop(&out_tx, &mut in_rx, &host).await?;

    let _ = out_tx.send(PluginOutgoing::event(goodbye_body())).await;
    Ok(())
}

/// Resolve where the kernel entry Lua lives, highest precedence first:
///
/// 1. `--kernel <path>` (or `-k`) argv — how the starter passes it.
/// 2. `NEFOR_DEV_DIR/starter/mag-kernel/init.lua` — in-checkout dev mode.
/// 3. `NEFOR_CONFIG_DIR/mag-kernel/init.lua` — installed-config default.
///
/// This mirrors the ecosystem's `NEFOR_DEV_DIR`-first search convention.
/// The engine exports `NEFOR_CONFIG_DIR` into every spawned plugin's env,
/// so (3) works even when the starter passes no explicit flag.
fn resolve_kernel_path() -> Result<PathBuf, MagError> {
    if let Some(path) = arg_value("--kernel").or_else(|| arg_value("-k")) {
        return Ok(PathBuf::from(path));
    }

    if let Some(dev) = std::env::var_os("NEFOR_DEV_DIR") {
        let candidate = PathBuf::from(dev).join("starter/mag-kernel/init.lua");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    if let Some(cfg) = std::env::var_os("NEFOR_CONFIG_DIR") {
        let candidate = PathBuf::from(cfg).join("mag-kernel/init.lua");
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
    let mut program: Option<LoadedProgram> = None;
    // The in-flight async run, if any (deferred-completion path).
    let mut active: Option<ActiveExecute> = None;
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
                            handle_event(out_tx, map, &mut program, host, &mut active, &mut bridge)
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

/// If the resident run signalled a terminal state, send the terminal reply and
/// clear the in-flight slot: completion carries the sink's final result plus
/// its output PATH; an unhandled actor failure (the kernel's `mag.run_failed`
/// escalation) fails the run with the failure detail surfaced.
async fn settle_if_complete(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    host: &LuaHost,
    active: &mut Option<ActiveExecute>,
) -> Result<(), MagError> {
    if active.is_none() {
        return Ok(());
    }
    if let Some(rc) = host.take_run_complete()? {
        let a = active.take().expect("active checked above");
        send_event(
            out_tx,
            run_result_ok(a.in_reply_to.as_deref(), &a.run_id, &rc),
        )
        .await?;
        return Ok(());
    }
    if let Some(error) = host.take_run_failed()? {
        let a = active.take().expect("active checked above");
        send_event(
            out_tx,
            run_result_failed(a.in_reply_to.as_deref(), &a.run_id, &error),
        )
        .await?;
    }
    Ok(())
}

/// Handle one inbound event body. We answer `mag.ping` (liveness), `mag.load`
/// (load a program, cache it, reply with the initial modification), and
/// `mag.eval` (apply a rule fn over the cached program). Everything else on the
/// broadcast bus is not ours and drops silently.
async fn handle_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    program: &mut Option<LoadedProgram>,
    host: &LuaHost,
    active: &mut Option<ActiveExecute>,
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
        return handle_provider_reply(out_tx, kind, body, host, active, bridge).await;
    }
    let in_reply_to = body.get("id").and_then(Value::as_str);
    match kind {
        PING_KIND => send_event(out_tx, pong_body(in_reply_to)).await,
        LOAD_KIND => handle_load(out_tx, body, in_reply_to, program, host).await,
        EVAL_KIND => handle_eval(out_tx, body, in_reply_to, program).await,
        EXECUTE_KIND => {
            handle_execute(out_tx, body, in_reply_to, program, host, active, bridge).await
        }
        APPLY_KIND => handle_apply(out_tx, body, in_reply_to, host, active, bridge).await,
        // A capability response correlated to a kernel-minted request id.
        // Unknown ids are dropped inside the kernel (no open correlation), so
        // forwarding every tool.result while a run is live is safe.
        TOOL_RESULT_KIND if active.is_some() => {
            handle_tool_result(out_tx, body, host, active, bridge).await
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
    program: &mut Option<LoadedProgram>,
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

    match nefor_mag::load(Path::new(source_dir), entry) {
        Ok(loaded) => {
            // The registry's factory names ride along so the control plane can
            // validate reasoner/factory types against the kernel's source of
            // truth instead of a hand-synced allowlist.
            let factories = host.registry_names().unwrap_or_default();
            let reply = match serde_json::to_value(&loaded.modification) {
                Ok(m) => loaded_body(in_reply_to, &loaded.hash, m, &factories),
                Err(e) => error_body(in_reply_to, &format!("modification serialize: {e}")),
            };
            *program = Some(loaded);
            send_event(out_tx, reply).await
        }
        Err(e) => send_event(out_tx, error_body(in_reply_to, &e.to_string())).await,
    }
}

/// Run a program through the kernel. The modification is taken inline from the
/// request (`modification` — the control plane reaches kernel ops directly,
/// ir.md) or, absent that, from the session's resident program (`mag.load`).
/// Registers the constellation, delivers the initial messages (each actor
/// constructs lazily at its first firing), streams lifecycle events, and — for
/// a synchronous program — replies `mag.run_result` with the sink's final
/// result + output path in the same turn. An async program (a provider
/// round-trip pending) defers the reply until `mag.run_complete`.
async fn handle_execute(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    in_reply_to: Option<&str>,
    program: &Option<LoadedProgram>,
    host: &LuaHost,
    active: &mut Option<ActiveExecute>,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    let mut modification: Value = match body.get("modification") {
        Some(m @ Value::Object(_)) => m.clone(),
        Some(_) => {
            return send_event(
                out_tx,
                error_body(in_reply_to, "mag.execute modification must be an object"),
            )
            .await
        }
        None => match program {
            Some(p) => match serde_json::to_value(&p.modification) {
                Ok(m) => m,
                Err(e) => {
                    return send_event(
                        out_tx,
                        error_body(
                            in_reply_to,
                            &format!("resident modification serialize: {e}"),
                        ),
                    )
                    .await
                }
            },
            None => {
                return send_event(
                    out_tx,
                    error_body(
                        in_reply_to,
                        "mag.execute before any mag.load and no inline modification",
                    ),
                )
                .await
            }
        },
    };

    // Apply the control plane's per-actor params overlay before spawn. Actor
    // params are kernel-opaque data owned by the factory (docs/ir.md), so an
    // overlay patched at apply time is legitimate control-plane input — it is
    // how the lead threads resolved profile params (provider/model/reasoning)
    // into the program without re-authoring the modification. Shallow per-actor
    // top-level merge; unknown ids are ignored (a race artifact, not an error).
    if let Some(overlay) = body.get("params_overlay").and_then(Value::as_object) {
        apply_params_overlay(&mut modification, overlay);
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
    let session_id = body.get("session_id").and_then(Value::as_str);

    host.begin_run(&run_id, run_name, session_id)?;
    let outcome = host.start(&modification)?;
    flush_emits(out_tx, host, bridge).await?;

    // A failed apply / rejected initial modification: nothing spawned.
    if !outcome.ok {
        let msg = outcome.error.unwrap_or_else(|| "start failed".into());
        return send_event(out_tx, run_result_failed(in_reply_to, &run_id, &msg)).await;
    }

    // Synchronous program: the sink fired inside `start`. Reply now.
    if let Some(rc) = host.take_run_complete()? {
        return send_event(out_tx, run_result_ok(in_reply_to, &run_id, &rc)).await;
    }
    // Synchronous failure inside `start`: an actor failed with no failure
    // route, or a factory's construct rejected at first firing.
    if let Some(error) = host.take_run_failed()? {
        return send_event(out_tx, run_result_failed(in_reply_to, &run_id, &error)).await;
    }

    // Async program: defer the terminal reply until `mag.run_complete` (or
    // `mag.run_failed`) arrives via capability responses.
    *active = Some(ActiveExecute {
        in_reply_to: in_reply_to.map(str::to_owned),
        run_id,
    });
    Ok(())
}

/// Merge a per-actor params overlay into a modification's actors before spawn.
/// The overlay is `{ actor_id: { param: value, ... }, ... }`. Each patch is a
/// shallow top-level merge into the matching actor's `params` (created if
/// absent, replaced if non-object). Actors not named in the overlay are
/// untouched; overlay keys with no matching actor are ignored.
fn apply_params_overlay(modification: &mut Value, overlay: &Map<String, Value>) {
    let actors = match modification.get_mut("actors").and_then(Value::as_array_mut) {
        Some(a) => a,
        None => return,
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
}

/// Named rejection for a `mag.apply` outside a live run. The control plane's
/// apply authority (docs/ir.md, "Kernel operations": the plane reaches
/// `actors`/`kills`/`messages` directly) is scoped to an active session with an
/// in-flight run — there is no constellation to modify otherwise.
const APPLY_NO_LIVE_RUN: &str =
    "mag.apply rejected: no live run (control-plane apply requires an active \
     session with an in-flight run — docs/ir.md, Kernel operations)";

/// Apply one graph modification directly to the resident kernel — the control
/// plane's mid-run kernel op (docs/ir.md, "Kernel operations"). The initial
/// modification runs via `mag.execute`; this is the *later* path: a
/// `{ kills = [...] }` modification kills an in-flight actor (the factory's
/// abort envelope reaches the bus, correlations drop, the late reply voids), a
/// `{ messages = [...] }` delivers a signal, a `{ actors = [...] }` registers
/// into the live constellation (constructing at first firing).
///
/// Guard policy: an apply is accepted only while a run is live (`active`), and
/// every accepted apply is logged with its `source`. Outside a live run the
/// apply is rejected with [`APPLY_NO_LIVE_RUN`] — the control-plane authority
/// ir.md grants the plane exists only *over a running constellation*, so a
/// session-less apply has nothing to act on. Lifecycle events the apply emits
/// stream on the wire; a completion it triggers settles the run.
async fn handle_apply(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    in_reply_to: Option<&str>,
    host: &LuaHost,
    active: &mut Option<ActiveExecute>,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    // Guard: reject applies with no live run before touching the kernel.
    let run_id = match active.as_ref() {
        Some(a) => a.run_id.clone(),
        None => {
            return send_event(
                out_tx,
                applied_body(in_reply_to, false, Some(APPLY_NO_LIVE_RUN)),
            )
            .await
        }
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

    let state = host.apply(&modification)?;
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
    // settles the in-flight execute reply.
    settle_if_complete(out_tx, host, active).await
}

/// Route a capability response into the kernel (unblocking a deferred
/// activation), forward whatever it produced, and settle the run if it
/// completed.
async fn handle_tool_result(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    host: &LuaHost,
    active: &mut Option<ActiveExecute>,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    let id = match body.get("id").and_then(Value::as_str) {
        Some(id) => id,
        None => return Ok(()),
    };
    // Providers/tools reply with `output`; some producers use `result`.
    let result = body.get("output").or_else(|| body.get("result"));
    let error = body.get("error").and_then(Value::as_str);
    host.bus_response(id, result, error)?;
    flush_emits(out_tx, host, bridge).await?;
    settle_if_complete(out_tx, host, active).await
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
    host: &LuaHost,
    active: &mut Option<ActiveExecute>,
    bridge: &mut CapabilityBridge,
) -> Result<(), MagError> {
    let reply = match bridge.take_reply(kind, body) {
        Some(r) => r,
        None => return Ok(()),
    };
    host.bus_response(
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
    settle_if_complete(out_tx, host, active).await
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
    program: &mut Option<LoadedProgram>,
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
        Ok(modification) => {
            let reply = match serde_json::to_value(&modification) {
                Ok(m) => modification_body(in_reply_to, m),
                Err(e) => error_body(in_reply_to, &format!("modification serialize: {e}")),
            };
            send_event(out_tx, reply).await
        }
        Err(e) => send_event(out_tx, error_body(in_reply_to, &e.to_string())).await,
    }
}

// ---- static body constructors ----------------------------------------------

fn hello_body(kernel: Option<&str>, factories: &[String]) -> Map<String, Value> {
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
    modification: Value,
    factories: &[String],
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(LOADED_KIND.into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    m.insert("hash".into(), Value::String(hash.to_owned()));
    m.insert("modification".into(), modification);
    // The kernel registry's factory names — the control plane's validation
    // source of truth for reasoner/factory types.
    m.insert(
        "factories".into(),
        Value::Array(factories.iter().cloned().map(Value::String).collect()),
    );
    m
}

/// Terminal run reply on success: status, the sink's final result INLINE
/// (`result` — text/kind/structured payload, exactly what the sink signalled
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

fn modification_body(in_reply_to: Option<&str>, modification: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(MODIFICATION_KIND.into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
    m.insert("modification".into(), modification);
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

    #[test]
    fn hello_body_advertises_version_and_kernel() {
        let b = hello_body(Some("mag-kernel"), &["sink".to_owned(), "llm".to_owned()]);
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
    }

    #[test]
    fn hello_body_omits_kernel_when_absent() {
        let b = hello_body(None, &[]);
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
    fn loaded_body_carries_hash_and_modification() {
        let b = loaded_body(
            Some("load-1"),
            "sha256:abc",
            serde_json::json!({"actors": []}),
            &["stub".to_owned(), "sink".to_owned()],
        );
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.loaded"));
        assert_eq!(b.get("in_reply_to").and_then(Value::as_str), Some("load-1"));
        assert_eq!(b.get("hash").and_then(Value::as_str), Some("sha256:abc"));
        assert!(b.get("modification").and_then(Value::as_object).is_some());
        let factories = b
            .get("factories")
            .and_then(Value::as_array)
            .expect("factories");
        assert!(factories.iter().any(|f| f.as_str() == Some("sink")));
    }

    #[test]
    fn modification_body_names_the_kind() {
        let b = modification_body(None, serde_json::json!({"kills": ["x"]}));
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("mag.modification")
        );
        assert!(b.get("in_reply_to").is_none());
        assert!(b.get("modification").is_some());
    }

    #[test]
    fn applied_body_acks_ok_and_error() {
        let ok = applied_body(Some("apply-1"), true, None);
        assert_eq!(ok.get("kind").and_then(Value::as_str), Some("mag.applied"));
        assert_eq!(
            ok.get("in_reply_to").and_then(Value::as_str),
            Some("apply-1")
        );
        assert_eq!(ok.get("ok").and_then(Value::as_bool), Some(true));
        assert!(ok.get("error").is_none());

        let bad = applied_body(None, false, Some("rejected"));
        assert_eq!(bad.get("ok").and_then(Value::as_bool), Some(false));
        assert_eq!(bad.get("error").and_then(Value::as_str), Some("rejected"));
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
        apply_params_overlay(&mut modification, overlay.as_object().unwrap());

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
        apply_params_overlay(&mut modification, overlay.as_object().unwrap());
        assert_eq!(
            modification["actors"][0]["params"]["model"].as_str(),
            Some("m")
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
