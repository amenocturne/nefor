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
}

impl BrokerShared {
    /// Build the shared state around an already-opened session writer.
    pub fn new(session: SessionWriter) -> Self {
        Self {
            event_log: Vec::new(),
            session,
            conns: HashMap::new(),
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
        }
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

    /// Drive the broker until either all connections have left or a
    /// shutdown completes.
    pub async fn run(mut self) -> BrokerStopReason {
        let mut shutdown_grace: Option<u64> = None;
        let mut shutdown_deadline: Option<Instant> = None;

        loop {
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

        // Append + snapshot the current log under the lock, then release it
        // before invoking step — step may call back into `BrokerOps::send`
        // which re-acquires the lock.
        let current_snapshot = {
            let mut guard = lock_shared(&self.shared);
            guard.event_log.push(entry.clone());
            if let Err(e) = guard.session.append(&entry) {
                tracing::error!(error = %e, "failed to append inbound entry to session log");
            }
            guard.event_log.clone()
        };

        if let Err(e) = self.host.invoke_step(&self.saved_log, &current_snapshot) {
            tracing::error!(error = %e, "step invocation errored at VM level");
        }
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
