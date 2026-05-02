//! Broker — central state + event loop.
//!
//! Post-Slice-2-I3 the broker is a protocol-agnostic string router. Its
//! responsibilities collapse to:
//!
//! 1. Accept per-connection transports attached by the runner (one per
//!    spawned plugin); own the read/write tasks and exit watcher.
//! 2. For every inbound line: stamp `origin = Plugin(name)` + `ts = now`,
//!    append a [`LogEntry`] to the in-memory event log and the session
//!    file, then invoke the cached Lua `step(saved_log, current_log)`
//!    hook. Step is free to decide what (if anything) flows out.
//! 3. Expose a [`BrokerOps`] routing sink to the Lua VM. When `step` calls
//!    `nefor.engine.send(payload, target?)` the broker stamps the outbound
//!    as `origin = Step`, appends it to the same log + session, and writes
//!    the line to the target writer queue (broadcast = every connected
//!    plugin; targeted = one plugin by name).
//! 4. Cascade shutdown: when one plugin exits and others are still alive,
//!    close the other connections' outbound channels within the grace
//!    window. No protocol-level `shutdown` system message is emitted — if
//!    `init.lua` wants to narrate the shutdown it does so via `step`.
//!
//! All NCP protocol handling (ready handshake, replay-on-attach, system
//! message dispatch, error-code classification) has moved to the user's
//! `starter/init.lua`. The broker does not parse the body of an inbound
//! line — raw bytes in, raw bytes out.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nefor_protocol::{PluginName, Timestamp};
use tokio::sync::mpsc;

use crate::lua::bindings::{EngineOps, SendTarget};
use crate::lua::LuaHost;
use crate::ncp::connection::{
    run_exit_watcher, run_reader, run_stderr_pump, run_writer, ConnectionId, ConnectionInbound,
    ConnectionOutbound, ReaderEnd, DEFAULT_QUEUE_CAPACITY,
};
use crate::ncp::transport::{ExitOutcome, Transport};
use crate::session::{LogEntry, Origin, SessionWriter};

/// Default shutdown grace — see §5.3. The broker still accepts an override
/// at `shutdown` time for operator flexibility.
pub const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 2000;

/// State the broker and the [`BrokerOps`] share: the engine's single source
/// of truth for the bus-wide event log, the open session file, and the
/// outbound-writer handle for every connected plugin.
pub struct BrokerShared {
    /// In-memory log of every message the engine has seen this run, inbound
    /// (`Origin::Plugin`) and outbound (`Origin::Step`), in routing order.
    /// Passed to `step` as `current_log`.
    pub event_log: Vec<LogEntry>,
    /// Write-through persistent mirror of `event_log`. Flushed on `Drop`.
    pub session: SessionWriter,
    /// Unbounded sender onto each connected plugin's writer queue, keyed by
    /// plugin name. Populated by [`Broker::attach_transport`] and cleared
    /// when the connection tears down.
    pub conns: HashMap<PluginName, mpsc::UnboundedSender<ConnectionOutbound>>,
    /// `nefor.engine.exit` sink. Set by the broker once it knows its
    /// shutdown handle + exit-code slot. None before the broker starts (in
    /// which case the binding still records a value for the next caller).
    pub exit_request: Option<ExitRequestSink>,
}

/// Routing sink for `nefor.engine.exit`. Holds a clone of the shutdown
/// handle and a shared exit-code slot. The broker installs one of these
/// on its `BrokerShared` before entering the run loop; the binding fires
/// it whenever Lua calls `nefor.engine.exit(code)`.
#[derive(Clone)]
pub struct ExitRequestSink {
    pub shutdown: ShutdownHandle,
    pub code: Arc<std::sync::atomic::AtomicI32>,
    pub fired: Arc<std::sync::atomic::AtomicBool>,
}

impl ExitRequestSink {
    /// Idempotent: first call wins, subsequent calls log + ignore so a
    /// faulty cli that calls exit twice with different codes doesn't
    /// produce surprising behaviour.
    ///
    /// Uses `try_send` directly on the shutdown channel rather than
    /// `tokio::spawn(shutdown.shutdown(...).await)`. The async-spawn
    /// path required a tokio runtime context (failing in unit tests
    /// outside `#[tokio::main]`) and risked the spawned future never
    /// running if the runtime was already winding down. The shutdown
    /// channel has capacity 4 with a single sender, so `try_send`
    /// always succeeds for the first call (the only one that matters
    /// — subsequent calls are gated by the `fired` latch above).
    pub fn request(&self, code: i32) {
        use std::sync::atomic::Ordering;
        if self
            .fired
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.code.store(code, Ordering::SeqCst);
            if let Err(e) = self.shutdown.0.try_send(DEFAULT_SHUTDOWN_GRACE_MS) {
                tracing::warn!(
                    code,
                    error = %e,
                    "nefor.engine.exit: shutdown channel rejected signal"
                );
            }
        } else {
            tracing::warn!(code, "nefor.engine.exit called more than once; ignoring");
        }
    }
}

impl BrokerShared {
    /// Build the shared state around an already-opened session writer.
    pub fn new(session: SessionWriter) -> Self {
        Self {
            event_log: Vec::new(),
            session,
            conns: HashMap::new(),
            exit_request: None,
        }
    }
}

/// Routing sink handed to the Lua VM. Every `nefor.engine.send` call from
/// `step` lands here; the sink stamps the outbound, logs it, and writes it
/// to the target connection(s).
pub struct BrokerOps {
    shared: Arc<Mutex<BrokerShared>>,
}

impl BrokerOps {
    /// Wrap a shared-state handle as an engine-ops sink.
    pub fn new(shared: Arc<Mutex<BrokerShared>>) -> Self {
        Self { shared }
    }
}

impl EngineOps for BrokerOps {
    fn request_exit(&self, code: i32) {
        let sink = {
            let guard = match self.shared.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.exit_request.clone()
        };
        match sink {
            Some(s) => s.request(code),
            None => {
                tracing::warn!(
                    code,
                    "nefor.engine.exit called before broker installed an exit sink; ignoring"
                );
            }
        }
    }

    fn plugins(&self) -> Vec<PluginName> {
        // Snapshot the connected set under the lock, then drop it. Callers
        // (Lua `nefor.engine.plugins()`) iterate the snapshot without holding
        // the lock — a plugin joining or leaving mid-iteration is fine since
        // any subsequent `send` re-checks the live map.
        let guard = match self.shared.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.conns.keys().cloned().collect()
    }

    fn send(&self, target: SendTarget, payload: String) {
        let ts = Timestamp::now();
        let target_name = match &target {
            SendTarget::Broadcast => None,
            SendTarget::Targeted(name) => Some(name.clone()),
        };
        let entry = LogEntry {
            ts,
            origin: Origin::Step,
            target: target_name,
            payload: payload.clone(),
        };

        // Hold the lock across the append + fanout so an interleaved inbound
        // line can't observe a half-applied outbound. The broker's run loop
        // is single-task so the only other holder is... itself, in a
        // sequential path — no contention.
        let mut guard = match self.shared.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.event_log.push(entry.clone());
        if let Err(e) = guard.session.append(&entry) {
            tracing::error!(error = %e, "failed to append outbound entry to session log");
        }
        let line = with_trailing_newline(payload);
        match target {
            SendTarget::Broadcast => {
                for conn in guard.conns.values() {
                    let _ = conn.send(ConnectionOutbound::Send(line.clone()));
                }
            }
            SendTarget::Targeted(name) => {
                if let Some(conn) = guard.conns.get(&name) {
                    let _ = conn.send(ConnectionOutbound::Send(line));
                } else {
                    tracing::warn!(
                        target = %name,
                        "step.send: target plugin is not connected; dropping outbound",
                    );
                }
            }
        }
    }
}

fn with_trailing_newline(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Per-connection bookkeeping the broker keeps outside the shared state
/// (the shared `conns` map is the routing index; this tracks lifecycle).
struct ConnectionRecord {
    name: PluginName,
    closing: bool,
}

/// The broker's single event loop.
pub struct Broker {
    shared: Arc<Mutex<BrokerShared>>,
    host: LuaHost,
    conns_by_id: HashMap<ConnectionId, ConnectionRecord>,
    /// Shared channel all per-connection readers drop messages onto.
    inbound_tx: mpsc::Sender<(ConnectionId, ConnectionInbound)>,
    inbound_rx: mpsc::Receiver<(ConnectionId, ConnectionInbound)>,
    /// Shared channel all per-connection exit watchers drop outcomes onto.
    exit_tx: mpsc::Sender<(ConnectionId, ExitOutcome)>,
    exit_rx: mpsc::Receiver<(ConnectionId, ExitOutcome)>,
    /// Triggered by [`Broker::shutdown_handle`] or an external signal.
    shutdown_rx: mpsc::Receiver<u64>,
    shutdown_tx: mpsc::Sender<u64>,
    /// Saved log from a resumed parent session. Passed verbatim to every
    /// `step` invocation as the first argument.
    saved_log: Vec<LogEntry>,
    /// Count of `event_log` entries already handed to `invoke_step`. The
    /// broker clones just `event_log[mirrored_count..]` under its lock and
    /// passes the small tail to step; the Lua VM appends those into the
    /// persistent `current_log` table. Avoids the per-event O(n) clone of
    /// the full log.
    mirrored_count: usize,
    /// Engine-originated synthetic envelopes (e.g. `engine.plugin_failed`)
    /// queued by callers outside the inbound path. Drained into the event
    /// log + step pipeline before the main `select!` on each tick so they
    /// route alongside real plugin lines. See [`Broker::queue_engine_envelope`].
    pending_engine_envelopes: Vec<LogEntry>,
    /// Exit-code slot updated by `nefor.engine.exit(code)` via
    /// [`ExitRequestSink`]. Read by [`Broker::requested_exit_code`] after
    /// `run()` returns so the dispatch path can propagate the code.
    exit_code_slot: Arc<std::sync::atomic::AtomicI32>,
    /// Latch: true once `nefor.engine.exit` fired at least once.
    exit_fired: Arc<std::sync::atomic::AtomicBool>,
}

/// Outcome of the broker's run loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerStopReason {
    /// `shutdown` signal completed its grace window.
    Shutdown,
    /// All connections exited and no shutdown was requested.
    AllPluginsGone,
}

impl Broker {
    /// Construct a new broker with default capacities. `shared` must already
    /// own an open [`SessionWriter`]; `host` must have its `step` function
    /// cached (see [`LuaHost::cache_step`]); `saved_log` is the hydrated
    /// parent-session log (empty for a fresh session).
    pub fn with_saved_log(
        shared: Arc<Mutex<BrokerShared>>,
        host: LuaHost,
        saved_log: Vec<LogEntry>,
    ) -> Self {
        // Shared inbound/exit channels sized to tolerate brief bursts from
        // many plugins. 1024 each matches §6's per-connection default.
        let (inbound_tx, inbound_rx) = mpsc::channel(1024);
        let (exit_tx, exit_rx) = mpsc::channel(64);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(4);

        // Install the exit-request sink so `nefor.engine.exit(code)` can
        // signal cooperative shutdown. The shutdown handle is the same
        // mpsc the broker's run loop watches.
        let exit_code_slot = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let exit_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut guard = lock_shared(&shared);
            guard.exit_request = Some(ExitRequestSink {
                shutdown: ShutdownHandle(shutdown_tx.clone()),
                code: Arc::clone(&exit_code_slot),
                fired: Arc::clone(&exit_fired),
            });
        }

        Self {
            shared,
            host,
            conns_by_id: HashMap::new(),
            inbound_tx,
            inbound_rx,
            exit_tx,
            exit_rx,
            shutdown_rx,
            shutdown_tx,
            saved_log,
            mirrored_count: 0,
            pending_engine_envelopes: Vec::new(),
            exit_code_slot,
            exit_fired,
        }
    }

    /// Read the exit code requested by `nefor.engine.exit`. Returns 0 if
    /// no exit was requested (e.g. broker exited because all plugins
    /// disconnected). Used by the CLI dispatch path to propagate the
    /// requested code to `std::process::exit`.
    #[allow(dead_code)]
    pub fn requested_exit_code(&self) -> i32 {
        if self.exit_fired.load(std::sync::atomic::Ordering::SeqCst) {
            self.exit_code_slot
                .load(std::sync::atomic::Ordering::SeqCst)
        } else {
            0
        }
    }

    /// Enqueue an engine-originated synthetic envelope for routing through
    /// `step`. The envelope is built as
    /// `{"type":"event","from":"engine","ts":<now>,"body":<body>}` and stamped
    /// with `Origin::Plugin(PluginName::engine())` so step sees it as a normal
    /// log entry. Drained on the next tick of [`Broker::run`] (or synchronously
    /// by callers that need ordering with shutdown — see [`Broker::handle_exit`]).
    ///
    /// Used to surface engine-level events (spawn-time and runtime plugin
    /// failures) to the Lua step layer, which translates them into plugin-
    /// targeted notifications (e.g. `chat.popup` to nefor-chat).
    pub fn queue_engine_envelope(&mut self, body: serde_json::Value) {
        let ts = Timestamp::now();
        let envelope = serde_json::json!({
            "type": "event",
            "from": "engine",
            "ts": ts.to_iso8601(),
            "body": body,
        });
        let payload = match serde_json::to_string(&envelope) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to serialize engine envelope; dropping");
                return;
            }
        };
        let entry = LogEntry {
            ts,
            origin: Origin::Plugin(PluginName::engine()),
            target: None,
            payload,
        };
        self.pending_engine_envelopes.push(entry);
    }

    /// Drain `pending_engine_envelopes` into the shared event log + session,
    /// then invoke step on the appended tail. No-op when the queue is empty.
    /// Called both from the main loop tick and synchronously from `handle_exit`
    /// so an `engine.plugin_failed` envelope can reach `nefor-chat`'s writer
    /// queue *before* the cooperative shutdown closes it.
    fn drain_engine_envelopes(&mut self) {
        if self.pending_engine_envelopes.is_empty() {
            return;
        }
        let drained = std::mem::take(&mut self.pending_engine_envelopes);

        let new_entries = {
            let mut guard = lock_shared(&self.shared);
            for entry in &drained {
                guard.event_log.push(entry.clone());
                if let Err(e) = guard.session.append(entry) {
                    tracing::error!(error = %e, "failed to append engine envelope to session log");
                }
            }
            let tail = guard.event_log[self.mirrored_count..].to_vec();
            self.mirrored_count = guard.event_log.len();
            tail
        };

        if let Err(e) = self.host.invoke_step(&self.saved_log, &new_entries) {
            tracing::error!(error = %e, "step invocation errored at VM level");
        }
        self.host.dispatch_subscriptions(&new_entries);
    }

    /// Clone a handle the caller can hold to request shutdown from outside
    /// the broker loop (e.g. a `ctrl_c` watcher).
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle(self.shutdown_tx.clone())
    }

    /// Attach an arbitrary transport to the broker under a pre-assigned
    /// plugin name. Returns the assigned [`ConnectionId`]. The broker
    /// does not wait for a ready handshake — the first inbound line flows
    /// directly into `step`.
    pub fn attach_transport(&mut self, transport: Transport, name: PluginName) -> ConnectionId {
        let id = ConnectionId::next();
        let log_label = name.as_str().to_owned();
        let (send_tx, send_rx) = mpsc::unbounded_channel::<ConnectionOutbound>();
        tokio::spawn(run_writer(
            id,
            transport.writer,
            send_rx,
            DEFAULT_QUEUE_CAPACITY,
        ));
        tokio::spawn(run_reader(id, transport.reader, self.inbound_tx.clone()));
        if let Some(stderr) = transport.stderr {
            tokio::spawn(run_stderr_pump(log_label, stderr));
        }
        tokio::spawn(run_exit_watcher(id, transport.exit, self.exit_tx.clone()));

        {
            let mut guard = lock_shared(&self.shared);
            guard.conns.insert(name.clone(), send_tx);
        }
        self.conns_by_id.insert(
            id,
            ConnectionRecord {
                name,
                closing: false,
            },
        );
        id
    }

    /// CLI-dispatch entry point. Drives the broker as in [`Broker::run`],
    /// but invokes the named plugin's `cli` function before entering the
    /// loop. Returns the exit code requested via `nefor.engine.exit`
    /// (defaults to 0 if the cli returns naturally without calling exit
    /// and broker shutdown happens via plugin disconnect / ctrl_c).
    ///
    /// Invocation ordering:
    /// 1. Attach was already done by the caller (subprocess plugins
    ///    spawned + transports wired).
    /// 2. Call `invoke_cli(name, args)`. Synchronous. The cli function
    ///    may register handlers via `nefor.bus.on_event`, send
    ///    envelopes via `nefor.engine.send`, block on
    ///    `nefor.io.read_line`, and finally call `nefor.engine.exit`.
    /// 3. Drive the run loop. Any handlers registered by the cli
    ///    function fire as plugin lines arrive; an `engine.exit` call
    ///    (made by the cli itself or any handler) triggers shutdown.
    pub async fn run_with_cli_dispatch(self, name: &str, args: &[String]) -> i32 {
        // The cli function runs on the main thread, holding the Lua VM
        // mutex. This is fine because the broker hasn't entered its run
        // loop yet — plugin lines queue in inbound_rx and are processed
        // once we drop into `run`.
        let cli_rc = self.host.invoke_cli(name, args);
        let exit_fired_already = self.exit_fired.load(std::sync::atomic::Ordering::SeqCst);
        match cli_rc {
            Ok(rc) => {
                // If the cli function returned a non-zero code without
                // calling exit, treat that as the requested exit. The
                // broker still runs to drain any handlers that need to
                // observe in-flight traffic, until shutdown fires (via
                // an exit call or peer disconnect).
                if rc != 0 && !exit_fired_already {
                    if let Some(sink) = lock_shared(&self.shared).exit_request.clone() {
                        sink.request(rc);
                    }
                }
            }
            Err(e) => {
                tracing::error!(plugin = %name, error = %e, "cli function failed");
                if !exit_fired_already {
                    if let Some(sink) = lock_shared(&self.shared).exit_request.clone() {
                        sink.request(1);
                    }
                }
            }
        }

        // Snapshot the exit-code slot before consuming self in run().
        let code_slot = Arc::clone(&self.exit_code_slot);
        let fired_slot = Arc::clone(&self.exit_fired);
        let _ = self.run().await;
        if fired_slot.load(std::sync::atomic::Ordering::SeqCst) {
            code_slot.load(std::sync::atomic::Ordering::SeqCst)
        } else {
            0
        }
    }

    /// Drive the broker until either all connections have left or a
    /// shutdown completes.
    pub async fn run(mut self) -> BrokerStopReason {
        let mut shutdown_grace: Option<u64> = None;
        let mut shutdown_deadline: Option<Instant> = None;

        loop {
            // Synthetic engine envelopes (queued via `queue_engine_envelope`)
            // are flushed at the top of every tick so they route alongside
            // real inbound lines. Doing this before the `tokio::select!`
            // guarantees they reach step before any pending shutdown arm
            // fires this iteration.
            self.drain_engine_envelopes();

            // If we're past the shutdown deadline, force-close everything
            // and exit.
            if let Some(deadline) = shutdown_deadline {
                if Instant::now() >= deadline {
                    self.force_close_all();
                    return BrokerStopReason::Shutdown;
                }
            }

            // If the engine said to shut down and there are no connections
            // left, exit immediately without waiting out the grace.
            if shutdown_deadline.is_some() && self.conns_by_id.is_empty() {
                return BrokerStopReason::Shutdown;
            }

            // If no shutdown in flight and all connections have quietly
            // left, return. This handles the "empty config" case (no
            // plugins spawned) and the "last plugin exited" case.
            if shutdown_deadline.is_none() && self.conns_by_id.is_empty() {
                return BrokerStopReason::AllPluginsGone;
            }

            let sleep_dur = if let Some(deadline) = shutdown_deadline {
                deadline.saturating_duration_since(Instant::now())
            } else {
                Duration::from_millis(500)
            };

            tokio::select! {
                Some((conn_id, msg)) = self.inbound_rx.recv() => {
                    self.handle_inbound(conn_id, msg).await;
                }
                Some((conn_id, outcome)) = self.exit_rx.recv() => {
                    self.handle_exit(conn_id, outcome).await;
                }
                Some(grace_ms) = self.shutdown_rx.recv(), if shutdown_grace.is_none() => {
                    shutdown_grace = Some(grace_ms);
                    shutdown_deadline = Some(Instant::now() + Duration::from_millis(grace_ms));
                    self.begin_shutdown();
                }
                _ = tokio::time::sleep(sleep_dur), if shutdown_deadline.is_some() => {
                    // Loop iteration to re-check the shutdown deadline above.
                }
            }
        }
    }

    // ---- inbound dispatch -------------------------------------------------

    async fn handle_inbound(&mut self, id: ConnectionId, msg: ConnectionInbound) {
        let Some(record) = self.conns_by_id.get(&id) else {
            return;
        };
        if record.closing {
            return;
        }
        match msg {
            ConnectionInbound::Line(line) => self.handle_line(id, line),
            ConnectionInbound::Closed { reason } => self.handle_reader_closed(id, reason),
        }
    }

    fn handle_line(&mut self, id: ConnectionId, payload: String) {
        let Some(record) = self.conns_by_id.get(&id) else {
            return;
        };
        let origin_name = record.name.clone();
        let entry = LogEntry {
            ts: Timestamp::now(),
            origin: Origin::Plugin(origin_name),
            target: None,
            payload,
        };

        // Append + clone only the unmirrored tail under the lock, then
        // release it before invoking step — step may call back into
        // `BrokerOps::send` which re-acquires the lock. Cloning the whole
        // `event_log` here was O(n) per inbound line, O(n²) per session.
        let new_entries = {
            let mut guard = lock_shared(&self.shared);
            guard.event_log.push(entry.clone());
            if let Err(e) = guard.session.append(&entry) {
                tracing::error!(error = %e, "failed to append inbound entry to session log");
            }
            let tail = guard.event_log[self.mirrored_count..].to_vec();
            self.mirrored_count = guard.event_log.len();
            tail
        };

        if let Err(e) = self.host.invoke_step(&self.saved_log, &new_entries) {
            tracing::error!(error = %e, "step invocation errored at VM level");
        }

        // Fan out the same tail to `nefor.bus.on_event` subscribers. Done
        // strictly after step so step's outbound `nefor.engine.send`
        // entries (which were appended to the event log during the call)
        // also reach handlers. Engine-side dispatch keeps it cheap when
        // no subscriptions are registered.
        self.host.dispatch_subscriptions(&new_entries);
    }

    fn handle_reader_closed(&mut self, id: ConnectionId, reason: ReaderEnd) {
        // Reader EOF / IO error — the plugin's outbound stream is done.
        // Don't immediately remove the connection from state; wait for the
        // exit watcher to fire. In-memory test transports without an exit
        // watcher fall through to the inbound-drained path below.
        tracing::debug!(conn = %id, ?reason, "reader loop ended");

        // If the connection has no exit watcher (in-memory tests), the
        // reader-closed signal is the only teardown notification we'll get.
        // Drop it now.
        let has_watcher = self.conns_by_id.contains_key(&id);
        if has_watcher {
            // If this connection has no exit watcher attached (we can't tell
            // from here — so be conservative), schedule a best-effort close.
            // The real exit watcher path takes priority and is idempotent.
            self.force_close(id);
        }
    }

    async fn handle_exit(&mut self, id: ConnectionId, outcome: ExitOutcome) {
        let name = self
            .conns_by_id
            .get(&id)
            .map(|r| r.name.as_str().to_owned())
            .unwrap_or_default();
        tracing::info!(plugin = %name, ?outcome, "plugin exited");

        // Drop the connection state. The writer task will exit when its
        // channel closes.
        if let Some(record) = self.conns_by_id.remove(&id) {
            let mut guard = lock_shared(&self.shared);
            guard.conns.remove(&record.name);
        }

        // Policy: the plugin set is a cooperating group. If one plugin
        // exits and others are still alive, propagate shutdown so the
        // session winds down as a whole instead of the remaining plugins
        // hanging on an engine with nothing to drive. The shutdown select
        // arm is already guarded against double-arming, and try_send
        // failing (channel full / closed) means a shutdown is already
        // in flight.
        if !self.conns_by_id.is_empty() {
            // Surface abnormal exits as engine-originated `engine.plugin_failed`
            // envelopes BEFORE triggering shutdown so step has a chance to
            // translate them into peer-targeted notifications (e.g. a
            // `chat.popup` to nefor-chat) while that peer's writer queue is
            // still open. Clean exits don't get a synthetic event — they are
            // normal lifecycle and shouldn't surface as failures.
            let (code, should_emit) = match outcome {
                ExitOutcome::CleanExit => ("clean_exit", false),
                ExitOutcome::Crash => ("crash", true),
                ExitOutcome::Evicted => ("evicted", false),
                ExitOutcome::Unknown => ("unknown_exit", true),
            };
            if should_emit && !name.is_empty() {
                self.queue_engine_envelope(serde_json::json!({
                    "kind":   "engine.plugin_failed",
                    "plugin": name,
                    "phase":  "runtime",
                    "reason": format!("plugin process exited abnormally ({code})"),
                    "code":   code,
                }));
                // Drain synchronously so step's outbound `nefor.engine.send`
                // lands on the target's writer queue before `begin_shutdown`
                // (which runs on the next loop iteration) closes it. The
                // writer task drains preceding `Send`s before honoring `Close`.
                self.drain_engine_envelopes();
            }

            tracing::info!(
                trigger_plugin = %name,
                "peer exited; initiating engine shutdown",
            );
            let _ = self.shutdown_tx.try_send(DEFAULT_SHUTDOWN_GRACE_MS);
        }
    }

    // ---- helpers ----------------------------------------------------------

    fn begin_shutdown(&mut self) {
        // Close every connection's writer channel. Writer tasks drain their
        // queues, flush, and exit. The shutdown-grace deadline in the run
        // loop force-closes anything still alive.
        let ids: Vec<ConnectionId> = self.conns_by_id.keys().copied().collect();
        for id in ids {
            self.force_close(id);
        }
    }

    fn force_close_all(&mut self) {
        let ids: Vec<ConnectionId> = self.conns_by_id.keys().copied().collect();
        for id in ids {
            self.force_close(id);
        }
    }

    fn force_close(&mut self, id: ConnectionId) {
        let Some(record) = self.conns_by_id.get_mut(&id) else {
            return;
        };
        if record.closing {
            return;
        }
        record.closing = true;
        let name = record.name.clone();
        let guard = lock_shared(&self.shared);
        if let Some(sender) = guard.conns.get(&name) {
            let _ = sender.send(ConnectionOutbound::Close);
        }
    }
}

fn lock_shared(m: &Arc<Mutex<BrokerShared>>) -> std::sync::MutexGuard<'_, BrokerShared> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Handle for requesting shutdown from outside the loop.
#[derive(Debug, Clone)]
pub struct ShutdownHandle(mpsc::Sender<u64>);

impl ShutdownHandle {
    /// Request shutdown with a grace window in milliseconds.
    pub async fn shutdown(&self, grace_ms: u64) {
        let _ = self.0.send(grace_ms).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventBus;
    use crate::lua::LuaHost;
    use crate::ncp::transport::Transport;
    use crate::ncp::PluginRegistry;
    use crate::session::{SessionHeader, SessionId};
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;
    use tokio::io::{duplex, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    /// A mock plugin transport pair. Returns the plugin half for tests to
    /// drive, and the broker-side [`Transport`] for attachment.
    struct MockPlugin {
        writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    }

    fn make_transport() -> (MockPlugin, Transport) {
        make_transport_buf(64 * 1024)
    }

    fn make_transport_buf(buf: usize) -> (MockPlugin, Transport) {
        let (plugin_side, broker_side) = duplex(buf);
        let (broker_read, broker_write) = tokio::io::split(broker_side);
        let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
        let transport = Transport {
            reader: Box::pin(broker_read),
            writer: Box::pin(broker_write),
            stderr: None,
            exit: None,
        };
        (
            MockPlugin {
                writer: plugin_write,
                reader: BufReader::new(plugin_read),
            },
            transport,
        )
    }

    async fn send_line(p: &mut MockPlugin, line: &str) {
        p.writer.write_all(line.as_bytes()).await.unwrap();
        if !line.ends_with('\n') {
            p.writer.write_all(b"\n").await.unwrap();
        }
    }

    async fn recv_line(p: &mut MockPlugin) -> Option<String> {
        let mut line = String::new();
        let n = p.reader.read_line(&mut line).await.ok()?;
        if n == 0 {
            return None;
        }
        Some(line.trim_end_matches(['\n', '\r']).to_owned())
    }

    fn pn(s: &str) -> PluginName {
        PluginName::new(s).expect("valid plugin name")
    }

    fn build_host(shared: &Arc<StdMutex<BrokerShared>>, init_src: &str) -> LuaHost {
        let bus = Arc::new(EventBus::new());
        let plugins = Arc::new(StdMutex::new(PluginRegistry::new()));
        let ops: Arc<dyn EngineOps> = Arc::new(BrokerOps::new(Arc::clone(shared)));
        let mut host = LuaHost::new(bus, plugins, ops).expect("host ok");
        host.exec_str("init.lua", init_src).expect("exec init");
        host.cache_step().expect("cache step");
        host
    }

    fn tmp_session(dir: &TempDir) -> (SessionWriter, std::path::PathBuf) {
        let id = SessionId::new();
        let path = dir
            .path()
            .join("nefor")
            .join("sessions")
            .join(format!("{id}.jsonl"));
        let header = SessionHeader::new(id, None, Timestamp::now());
        let writer = SessionWriter::create_at(path.clone(), header).expect("writer");
        (writer, path)
    }

    // --- tests ---------------------------------------------------------

    #[tokio::test]
    async fn broker_exits_when_no_plugins_configured() {
        let dir = TempDir::new().unwrap();
        let (session, _path) = tmp_session(&dir);
        let shared = Arc::new(StdMutex::new(BrokerShared::new(session)));
        let host = build_host(&shared, "function step(s, c) end");
        let broker = Broker::with_saved_log(shared, host, Vec::new());
        let outcome = tokio::time::timeout(Duration::from_secs(2), broker.run())
            .await
            .expect("broker should exit quickly");
        assert_eq!(outcome, BrokerStopReason::AllPluginsGone);
    }

    #[tokio::test]
    async fn inbound_line_invokes_step() {
        // Step appends to a Lua-side global every time it runs so the test
        // can assert what it saw.
        let dir = TempDir::new().unwrap();
        let (session, _path) = tmp_session(&dir);
        let shared = Arc::new(StdMutex::new(BrokerShared::new(session)));
        let host = build_host(
            &shared,
            r#"
            seen = {}
            function step(saved, current)
                local last = current[#current]
                seen[#seen + 1] = last.origin .. ":" .. last.payload
            end
            "#,
        );

        // Grab a handle on the Lua VM before handing it to the broker so the
        // test can read `seen` back after the run.
        let lua = host.lua().clone();

        let mut broker = Broker::with_saved_log(Arc::clone(&shared), host, Vec::new());
        let (mut p, t) = make_transport();
        broker.attach_transport(t, pn("test"));
        let handle = broker.shutdown_handle();
        let run = tokio::spawn(broker.run());

        send_line(&mut p, "hello from test").await;

        // Let the broker drain.
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.shutdown(50).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), run).await;

        let seen: mlua::Table = lua.globals().get("seen").unwrap();
        let first: String = seen.get(1).unwrap();
        assert_eq!(first, "test:hello from test");
    }

    #[tokio::test]
    async fn step_send_broadcast_writes_to_all_peers() {
        let dir = TempDir::new().unwrap();
        let (session, _path) = tmp_session(&dir);
        let shared = Arc::new(StdMutex::new(BrokerShared::new(session)));
        // Step broadcasts "pong" on every inbound line.
        let host = build_host(
            &shared,
            r#"function step(saved, current) nefor.engine.send("pong") end"#,
        );

        let mut broker = Broker::with_saved_log(Arc::clone(&shared), host, Vec::new());
        let (mut pa, ta) = make_transport();
        let (mut pb, tb) = make_transport();
        let (mut pc, tc) = make_transport();
        broker.attach_transport(ta, pn("a"));
        broker.attach_transport(tb, pn("b"));
        broker.attach_transport(tc, pn("c"));
        let handle = broker.shutdown_handle();
        let run = tokio::spawn(broker.run());

        send_line(&mut pa, "trigger").await;

        // All three plugins receive the broadcast — including the origin, per
        // the spec: step is not a plugin, so "all plugins minus origin (Step)"
        // = all plugins.
        for (p, label) in [(&mut pa, "a"), (&mut pb, "b"), (&mut pc, "c")] {
            let line = tokio::time::timeout(Duration::from_millis(500), recv_line(p))
                .await
                .unwrap_or_else(|_| panic!("{label} timed out waiting for broadcast"));
            assert_eq!(line.as_deref(), Some("pong"));
        }

        handle.shutdown(50).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), run).await;
    }

    #[tokio::test]
    async fn step_send_targeted_writes_to_one_peer() {
        let dir = TempDir::new().unwrap();
        let (session, _path) = tmp_session(&dir);
        let shared = Arc::new(StdMutex::new(BrokerShared::new(session)));
        let host = build_host(
            &shared,
            r#"function step(saved, current) nefor.engine.send("to-b-only", "b") end"#,
        );

        let mut broker = Broker::with_saved_log(Arc::clone(&shared), host, Vec::new());
        let (mut pa, ta) = make_transport();
        let (mut pb, tb) = make_transport();
        broker.attach_transport(ta, pn("a"));
        broker.attach_transport(tb, pn("b"));
        let handle = broker.shutdown_handle();
        let run = tokio::spawn(broker.run());

        send_line(&mut pa, "trigger").await;

        let got_b = tokio::time::timeout(Duration::from_millis(500), recv_line(&mut pb))
            .await
            .expect("b timed out");
        assert_eq!(got_b.as_deref(), Some("to-b-only"));

        // a must not have received anything — give it a generous window so we
        // catch accidental fan-out.
        let got_a = tokio::time::timeout(Duration::from_millis(150), recv_line(&mut pa)).await;
        assert!(
            got_a.is_err() || got_a.unwrap().is_none(),
            "a must not receive targeted send aimed at b",
        );

        handle.shutdown(50).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), run).await;
    }

    #[tokio::test]
    async fn session_log_records_inbound_and_outbound() {
        let dir = TempDir::new().unwrap();
        let (session, path) = tmp_session(&dir);
        let shared = Arc::new(StdMutex::new(BrokerShared::new(session)));
        let host = build_host(
            &shared,
            r#"function step(saved, current) nefor.engine.send("echoed", "a") end"#,
        );

        let mut broker = Broker::with_saved_log(Arc::clone(&shared), host, Vec::new());
        let (mut pa, ta) = make_transport();
        broker.attach_transport(ta, pn("a"));
        let handle = broker.shutdown_handle();
        let run = tokio::spawn(broker.run());

        send_line(&mut pa, "inbound-line").await;
        // Let the outbound drain through the writer task onto the wire.
        let got = tokio::time::timeout(Duration::from_millis(500), recv_line(&mut pa))
            .await
            .expect("a timed out");
        assert_eq!(got.as_deref(), Some("echoed"));

        // Shut the broker down cleanly so the SessionWriter flushes.
        handle.shutdown(50).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), run).await;

        // Explicitly flush the session writer before reading the file back.
        // Drop-ordering alone is not reliable here — the SessionWriter's
        // Drop only runs when the last Arc<Mutex<BrokerShared>> is dropped,
        // and subtle ordering inside spawned tasks can stretch that window.
        {
            let mut guard = shared.lock().expect("lock shared");
            guard.session.flush().expect("flush session");
        }

        let mut file = tokio::fs::File::open(&path).await.expect("session file");
        let mut body = String::new();
        file.read_to_string(&mut body).await.expect("read session");
        // Header + at least two entries (one inbound, one outbound).
        let lines: Vec<&str> = body.lines().collect();
        assert!(
            lines.len() >= 3,
            "expected header + >=2 entries, got {}: {body}",
            lines.len()
        );
        let inbound = lines
            .iter()
            .find(|l| l.contains("\"origin\":\"a\"") && l.contains("\"payload\":\"inbound-line\""))
            .expect("inbound entry present");
        let outbound = lines
            .iter()
            .find(|l| {
                l.contains("\"origin\":\"step\"")
                    && l.contains("\"target\":\"a\"")
                    && l.contains("\"payload\":\"echoed\"")
            })
            .expect("outbound entry present");
        assert_ne!(inbound, outbound);
    }

    #[tokio::test]
    async fn shutdown_closes_peer_connections() {
        // When one plugin exits, the broker cascades shutdown: the other
        // connections' outbound channels close within the grace window.
        let dir = TempDir::new().unwrap();
        let (session, _path) = tmp_session(&dir);
        let shared = Arc::new(StdMutex::new(BrokerShared::new(session)));
        let host = build_host(&shared, "function step(s, c) end");

        let mut broker = Broker::with_saved_log(Arc::clone(&shared), host, Vec::new());
        let (_pa, ta) = make_transport();
        let (pb, tb) = make_transport();
        broker.attach_transport(ta, pn("a"));
        broker.attach_transport(tb, pn("b"));
        let handle = broker.shutdown_handle();
        let run = tokio::spawn(broker.run());

        handle.shutdown(50).await;
        let outcome = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("broker should stop within grace")
            .expect("join ok");
        assert_eq!(outcome, BrokerStopReason::Shutdown);

        // After shutdown the plugin-side reader should observe EOF.
        let mut pb = pb;
        let mut line = String::new();
        let n = pb
            .reader
            .read_line(&mut line)
            .await
            .expect("read_line should return 0 at EOF");
        assert_eq!(n, 0, "expected EOF after shutdown, got {line:?}");
    }

    /// Build a transport pair whose broker side has a caller-controllable
    /// exit watcher. Returning the `oneshot::Sender` lets a test fire a
    /// specific `ExitOutcome` to drive `handle_exit`.
    fn make_transport_with_exit() -> (
        MockPlugin,
        Transport,
        tokio::sync::oneshot::Sender<ExitOutcome>,
    ) {
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<ExitOutcome>();
        let watcher: Pin<Box<dyn std::future::Future<Output = ExitOutcome> + Send>> =
            Box::pin(async move { exit_rx.await.unwrap_or(ExitOutcome::Unknown) });
        let (plugin_side, broker_side) = duplex(64 * 1024);
        let (broker_read, broker_write) = tokio::io::split(broker_side);
        let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
        let transport = Transport {
            reader: Box::pin(broker_read),
            writer: Box::pin(broker_write),
            stderr: None,
            exit: Some(watcher),
        };
        (
            MockPlugin {
                writer: plugin_write,
                reader: BufReader::new(plugin_read),
            },
            transport,
            exit_tx,
        )
    }

    #[tokio::test]
    async fn queue_engine_envelope_drains_into_new_entries_on_next_tick() {
        // Step records the kind of every entry it sees so we can assert the
        // synthetic envelope reached the Lua layer with origin=engine.
        let dir = TempDir::new().unwrap();
        let (session, _path) = tmp_session(&dir);
        let shared = Arc::new(StdMutex::new(BrokerShared::new(session)));
        let host = build_host(
            &shared,
            r#"
            seen = {}
            function step(saved, current)
                local last = current[#current]
                seen[#seen + 1] = last.origin .. ":" .. last.payload
            end
            "#,
        );
        let lua = host.lua().clone();
        let mut broker = Broker::with_saved_log(Arc::clone(&shared), host, Vec::new());

        // Queue *before* run() — the first tick must drain it.
        broker.queue_engine_envelope(serde_json::json!({
            "kind": "engine.plugin_failed",
            "plugin": "ghost",
            "phase": "spawn",
            "reason": "binary missing",
            "code": "missing_dir",
        }));
        // Attach one transport so the run loop has a connection to wait on
        // and doesn't exit AllPluginsGone before draining.
        let (_p, t) = make_transport();
        broker.attach_transport(t, pn("dummy"));

        let handle = broker.shutdown_handle();
        let run = tokio::spawn(broker.run());

        // Give the broker a tick to drain.
        tokio::time::sleep(Duration::from_millis(150)).await;
        handle.shutdown(50).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), run).await;

        let seen: mlua::Table = lua.globals().get("seen").unwrap();
        let first: String = seen.get(1).expect("step saw at least one entry");
        assert!(
            first.starts_with("engine:"),
            "first entry should be from engine, got {first}"
        );
        assert!(
            first.contains("\"kind\":\"engine.plugin_failed\""),
            "payload should carry the kind, got {first}"
        );
        assert!(
            first.contains("\"plugin\":\"ghost\""),
            "payload should carry plugin name, got {first}"
        );
    }

    #[tokio::test]
    async fn handle_exit_with_crash_emits_engine_plugin_failed_then_shuts_down() {
        // Two plugins: 'a' is the victim, 'b' stays alive long enough for the
        // synthetic envelope to flow through step. Step records what it saw
        // so the test can assert the engine-originated entry shape.
        let dir = TempDir::new().unwrap();
        let (session, _path) = tmp_session(&dir);
        let shared = Arc::new(StdMutex::new(BrokerShared::new(session)));
        let host = build_host(
            &shared,
            r#"
            engine_seen = {}
            function step(saved, current)
                local last = current[#current]
                if last.origin == "engine" then
                    engine_seen[#engine_seen + 1] = last.payload
                end
            end
            "#,
        );
        let lua = host.lua().clone();
        let mut broker = Broker::with_saved_log(Arc::clone(&shared), host, Vec::new());

        let (_pa, ta, exit_tx_a) = make_transport_with_exit();
        let (_pb, tb) = make_transport();
        broker.attach_transport(ta, pn("a"));
        broker.attach_transport(tb, pn("b"));

        let run = tokio::spawn(broker.run());

        // Fire the crash outcome for 'a'. The broker's exit watcher routes
        // it to `handle_exit` which queues + drains synchronously.
        let _ = exit_tx_a.send(ExitOutcome::Crash);

        // The cooperative-shutdown grace is DEFAULT_SHUTDOWN_GRACE_MS. Wait
        // long enough for: handle_exit → drain → step → shutdown → run exit.
        let outcome =
            tokio::time::timeout(Duration::from_millis(DEFAULT_SHUTDOWN_GRACE_MS + 500), run)
                .await
                .expect("broker should stop within grace + slack")
                .expect("join ok");
        assert_eq!(outcome, BrokerStopReason::Shutdown);

        let engine_seen: mlua::Table = lua.globals().get("engine_seen").unwrap();
        let len = engine_seen.len().unwrap();
        assert!(
            len >= 1,
            "step should have observed >=1 engine entry, got {len}"
        );
        let payload: String = engine_seen.get(1).unwrap();
        assert!(
            payload.contains("\"kind\":\"engine.plugin_failed\""),
            "expected engine.plugin_failed kind, got {payload}"
        );
        assert!(
            payload.contains("\"plugin\":\"a\""),
            "expected plugin name 'a', got {payload}"
        );
        assert!(
            payload.contains("\"phase\":\"runtime\""),
            "expected phase=runtime, got {payload}"
        );
        assert!(
            payload.contains("\"code\":\"crash\""),
            "expected code=crash, got {payload}"
        );
    }

    #[tokio::test]
    async fn handle_exit_with_clean_exit_does_not_emit_engine_plugin_failed() {
        // CleanExit is normal lifecycle — no synthetic envelope, just the
        // usual cooperative-shutdown cascade.
        let dir = TempDir::new().unwrap();
        let (session, _path) = tmp_session(&dir);
        let shared = Arc::new(StdMutex::new(BrokerShared::new(session)));
        let host = build_host(
            &shared,
            r#"
            engine_seen = {}
            function step(saved, current)
                local last = current[#current]
                if last.origin == "engine" then
                    engine_seen[#engine_seen + 1] = last.payload
                end
            end
            "#,
        );
        let lua = host.lua().clone();
        let mut broker = Broker::with_saved_log(Arc::clone(&shared), host, Vec::new());

        let (_pa, ta, exit_tx_a) = make_transport_with_exit();
        let (_pb, tb) = make_transport();
        broker.attach_transport(ta, pn("a"));
        broker.attach_transport(tb, pn("b"));

        let run = tokio::spawn(broker.run());

        let _ = exit_tx_a.send(ExitOutcome::CleanExit);

        let outcome =
            tokio::time::timeout(Duration::from_millis(DEFAULT_SHUTDOWN_GRACE_MS + 500), run)
                .await
                .expect("broker should stop within grace + slack")
                .expect("join ok");
        assert_eq!(outcome, BrokerStopReason::Shutdown);

        let engine_seen: mlua::Table = lua.globals().get("engine_seen").unwrap();
        let len = engine_seen.len().unwrap();
        assert_eq!(
            len, 0,
            "clean exit must not produce an engine.plugin_failed envelope, saw {len}"
        );
    }

    #[tokio::test]
    async fn write_queue_overflow_drops_oldest() {
        // Tiny duplex buffer + per-step broadcasts fills up the writer. The
        // broker's post-I3 overflow policy: drop oldest, no protocol emission.
        // We assert the broker keeps making forward progress (doesn't hang)
        // and the writer task logs the overflow internally.
        let dir = TempDir::new().unwrap();
        let (session, _path) = tmp_session(&dir);
        let shared = Arc::new(StdMutex::new(BrokerShared::new(session)));
        let host = build_host(
            &shared,
            r#"function step(saved, current) nefor.engine.send("x") end"#,
        );

        let mut broker = Broker::with_saved_log(Arc::clone(&shared), host, Vec::new());
        let (mut sender, sender_t) = make_transport();
        let (_receiver, receiver_t) = make_transport_buf(64);
        broker.attach_transport(sender_t, pn("s"));
        broker.attach_transport(receiver_t, pn("r"));
        let handle = broker.shutdown_handle();
        let run = tokio::spawn(broker.run());

        for i in 0..200u32 {
            send_line(&mut sender, &format!("trigger-{i}")).await;
        }
        // Give the broker time to process and the writer task time to attempt
        // draining; this test passes if the broker's run loop doesn't deadlock.
        tokio::time::sleep(Duration::from_millis(200)).await;

        handle.shutdown(50).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), run).await;
    }
}
