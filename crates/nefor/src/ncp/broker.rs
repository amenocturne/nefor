//! Broker — central state + event loop.
//!
//! The broker owns one [`ConnectionState`] per connected plugin and runs
//! the single event loop that fans out envelopes via per-connection
//! bounded send queues (§6).
//!
//! Per §9 clarification / D-09, `shutdown` is delivered as N point-to-
//! point sends, not through a bus abstraction. Event messages (§6) fan
//! out to every OTHER connected plugin.
//!
//! # Lifecycle
//!
//! 1. Runner spawns the plugin subprocess with cwd = `<plugin-dir>/<name>/`
//!    and stamps `from = spec.name` on every envelope this connection
//!    emits. No name negotiation on the wire.
//! 2. Broker waits for the plugin's first message: a `ready` system body
//!    declaring the NCP protocol version. Validates version, replies
//!    `ready_ok` or an `error` that closes the connection.
//! 3. Broker routes event messages (broadcast to every other peer) and
//!    rejects system messages that plugins are not allowed to send.
//! 4. On plugin stdout-close / exit the broker logs and cleans up. Peers
//!    that care about peer liveness implement their own heartbeat /
//!    farewell conventions.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use nefor_protocol::{
    Body, Envelope, ErrorCode, MessageKind, Offending, ParseError, PluginName, PluginOutgoing,
    SystemBody, Timestamp,
};
use tokio::sync::mpsc;

use crate::ncp::connection::{
    run_exit_watcher, run_reader, run_stderr_pump, run_writer, ConnectionId, ConnectionInbound,
    ConnectionOutbound, ReaderEnd, DEFAULT_QUEUE_CAPACITY,
};
use crate::ncp::transport::{ExitOutcome, Transport};

/// Protocol version this broker implements. Plugins declaring any other
/// `protocol_version` in the ready handshake are rejected with
/// `ProtocolVersionMismatch` (§9).
pub const SUPPORTED_PROTOCOL_VERSION: &str = "0.1";

/// How long a fresh connection has to send a valid `ready` before the
/// broker closes it (§2, 10 s recommended default).
pub const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Default shutdown grace — see §5.3. The broker still accepts an override
/// at `shutdown` time for operator flexibility.
pub const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 2000;

/// Per-connection state the broker tracks.
struct ConnectionState {
    /// Engine-local id.
    id: ConnectionId,
    /// Plugin name — assigned by the runner at connection creation, not
    /// negotiated on the wire. Present before the ready handshake.
    name: PluginName,
    /// Sender onto the writer's queue. Unbounded so the broker is never
    /// blocked; the writer task enforces the §6 bounded-queue overflow
    /// policy on the receive side.
    send: mpsc::UnboundedSender<ConnectionOutbound>,
    /// When the ready timeout fires (only meaningful while pre-ready).
    ready_deadline: Instant,
    /// True once the plugin has sent a valid `ready`. Pre-ready events /
    /// non-ready system messages are rejected.
    ready: bool,
    /// Set when the broker has scheduled this connection to close. While
    /// `closing`, further inbound messages are logged-and-dropped.
    closing: bool,
}

/// The broker's single event loop.
pub struct Broker {
    conns: HashMap<ConnectionId, ConnectionState>,
    /// Shared channel all per-connection readers drop messages onto.
    inbound_tx: mpsc::Sender<(ConnectionId, ConnectionInbound)>,
    inbound_rx: mpsc::Receiver<(ConnectionId, ConnectionInbound)>,
    /// Shared channel all per-connection exit watchers drop outcomes onto.
    exit_tx: mpsc::Sender<(ConnectionId, ExitOutcome)>,
    exit_rx: mpsc::Receiver<(ConnectionId, ExitOutcome)>,
    /// Triggered by [`Broker::shutdown`] or an external signal.
    shutdown_rx: mpsc::Receiver<u64>,
    shutdown_tx: mpsc::Sender<u64>,
    /// Engine version string included in `ready_ok`.
    engine_version: String,
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
    /// Construct a new broker with default capacities.
    pub fn new(engine_version: impl Into<String>) -> Self {
        // Shared inbound/exit channels sized to tolerate brief bursts from
        // many plugins. 1024 each matches §6's per-connection default.
        let (inbound_tx, inbound_rx) = mpsc::channel(1024);
        let (exit_tx, exit_rx) = mpsc::channel(64);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(4);
        Self {
            conns: HashMap::new(),
            inbound_tx,
            inbound_rx,
            exit_tx,
            exit_rx,
            shutdown_rx,
            shutdown_tx,
            engine_version: engine_version.into(),
        }
    }

    /// Clone a handle the caller can hold to request shutdown from outside
    /// the broker loop (e.g. a `ctrl_c` watcher).
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle(self.shutdown_tx.clone())
    }

    /// Attach an arbitrary transport to the broker under a pre-assigned
    /// plugin name. Returns the assigned [`ConnectionId`]. The broker
    /// treats this as a fresh connection that still owes a `ready`
    /// handshake before any events are accepted.
    pub fn attach_transport(&mut self, transport: Transport, name: PluginName) -> ConnectionId {
        let id = ConnectionId::next();
        let log_label = name.as_str().to_owned();
        let (send_tx, send_rx) = mpsc::unbounded_channel::<ConnectionOutbound>();
        tokio::spawn(run_writer(
            transport.writer,
            send_rx,
            DEFAULT_QUEUE_CAPACITY,
        ));
        tokio::spawn(run_reader(id, transport.reader, self.inbound_tx.clone()));
        if let Some(stderr) = transport.stderr {
            tokio::spawn(run_stderr_pump(log_label, stderr));
        }
        tokio::spawn(run_exit_watcher(id, transport.exit, self.exit_tx.clone()));

        self.conns.insert(
            id,
            ConnectionState {
                id,
                name,
                send: send_tx,
                ready_deadline: Instant::now() + READY_TIMEOUT,
                ready: false,
                closing: false,
            },
        );
        id
    }

    /// Drive the broker until either all connections have left or a
    /// shutdown completes.
    pub async fn run(mut self) -> BrokerStopReason {
        let mut ready_tick = tokio::time::interval(Duration::from_millis(500));
        ready_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut shutdown_grace: Option<u64> = None;
        let mut shutdown_deadline: Option<Instant> = None;

        loop {
            // Pending closes whose writer queue drained — remove them.
            self.reap_closed();

            // If we're past the shutdown deadline, force-close everything
            // and exit.
            if let Some(deadline) = shutdown_deadline {
                if Instant::now() >= deadline {
                    for conn in self.conns.values_mut() {
                        let _ = conn.send.send(ConnectionOutbound::Close);
                        conn.closing = true;
                    }
                    return BrokerStopReason::Shutdown;
                }
            }

            // If the engine said to shut down and there are no connections
            // left, exit immediately without waiting out the grace.
            if shutdown_deadline.is_some() && self.conns.is_empty() {
                return BrokerStopReason::Shutdown;
            }

            // If no shutdown in flight and all connections have quietly
            // left, return. This handles the "empty config" case (no
            // plugins spawned) and the "last plugin exited" case.
            if shutdown_deadline.is_none() && self.conns.is_empty() {
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
                    self.broadcast_shutdown(grace_ms).await;
                }
                _ = ready_tick.tick() => {
                    self.check_ready_timeouts().await;
                }
                _ = tokio::time::sleep(sleep_dur), if shutdown_deadline.is_some() => {
                    // Loop iteration to re-check the shutdown deadline above.
                }
            }
        }
    }

    // ---- inbound dispatch -------------------------------------------------

    async fn handle_inbound(&mut self, id: ConnectionId, msg: ConnectionInbound) {
        let Some(conn) = self.conns.get(&id) else {
            return;
        };
        if conn.closing {
            return;
        }
        match msg {
            ConnectionInbound::Message(out) => self.handle_message(id, out).await,
            ConnectionInbound::ParseError(e) => self.handle_parse_error(id, e).await,
            ConnectionInbound::Closed { reason } => {
                self.handle_reader_closed(id, reason).await;
            }
        }
    }

    async fn handle_message(&mut self, id: ConnectionId, out: PluginOutgoing) {
        let ready = self.conns.get(&id).map(|c| c.ready).unwrap_or(false);
        if !ready {
            // Pre-ready connections: the first message MUST be a valid
            // `ready` system body. Anything else (including event
            // messages) closes the connection with invalid_ready.
            if let (MessageKind::System, Body::System(SystemBody::Ready { .. })) =
                (out.kind, &out.body)
            {
                self.handle_ready(id, out).await;
            } else {
                self.send_error_and_close(
                    id,
                    ErrorCode::InvalidReady,
                    "first message must be a system ready".into(),
                    None,
                )
                .await;
            }
            return;
        }

        // Post-ready: route by type.
        match out.kind {
            MessageKind::Event => self.route_event(id, out).await,
            MessageKind::System => self.route_system(id, out).await,
        }
    }

    async fn handle_ready(&mut self, id: ConnectionId, out: PluginOutgoing) {
        let Body::System(SystemBody::Ready { protocol_version }) = out.body else {
            // Can't happen given the check in `handle_message`, but we
            // handle defensively.
            self.send_error_and_close(
                id,
                ErrorCode::InvalidReady,
                "internal: non-ready body in ready path".into(),
                None,
            )
            .await;
            return;
        };

        if protocol_version != SUPPORTED_PROTOCOL_VERSION {
            self.send_error_and_close(
                id,
                ErrorCode::ProtocolVersionMismatch,
                format!(
                    "engine supports protocol_version={SUPPORTED_PROTOCOL_VERSION:?}, \
                     plugin sent {protocol_version:?}"
                ),
                None,
            )
            .await;
            return;
        }

        let plugin_name = match self.conns.get_mut(&id) {
            Some(conn) => {
                conn.ready = true;
                conn.name.clone()
            }
            None => return,
        };

        tracing::info!(plugin = %plugin_name, %protocol_version, "plugin ready");

        self.send_system(
            id,
            SystemBody::ReadyOk {
                engine_version: self.engine_version.clone(),
            },
        )
        .await;
    }

    async fn route_event(&mut self, sender_id: ConnectionId, out: PluginOutgoing) {
        let Body::Event(event_body) = out.body else {
            return;
        };
        let sender_name = match self.conns.get(&sender_id).map(|c| c.name.clone()) {
            Some(n) => n,
            None => return,
        };
        let ts = Timestamp::now();
        let envelope = Envelope::event(sender_name, ts, event_body);

        let targets: Vec<ConnectionId> = self
            .conns
            .iter()
            .filter(|(peer_id, c)| **peer_id != sender_id && !c.closing && c.ready)
            .map(|(peer_id, _)| *peer_id)
            .collect();
        for target_id in targets {
            self.push_envelope(target_id, envelope.clone()).await;
        }
    }

    async fn route_system(&mut self, sender_id: ConnectionId, out: PluginOutgoing) {
        let Body::System(body) = out.body else {
            return;
        };
        match body {
            SystemBody::Ready { .. } => {
                // Re-ready on an already-ready connection is a protocol
                // error — the plugin has one shot at the handshake.
                self.send_error_and_close(
                    sender_id,
                    ErrorCode::InvalidReady,
                    "ready received on an already-ready connection".into(),
                    None,
                )
                .await;
            }
            // The remaining system kinds are engine→plugin only per §5.
            // A plugin sending them is a protocol violation — drop with
            // an unknown_kind error (the kind is structurally recognized
            // but not allowed in this direction).
            SystemBody::ReadyOk { .. } | SystemBody::Shutdown { .. } | SystemBody::Error { .. } => {
                self.send_system_error(
                    sender_id,
                    ErrorCode::UnknownKind,
                    "system kind not permitted from plugins".into(),
                    None,
                )
                .await;
            }
        }
    }

    async fn handle_parse_error(&mut self, id: ConnectionId, err: ParseError) {
        let (code, close, message) = classify_parse_error(&err);
        if close {
            self.send_error_and_close(id, code, message, None).await;
        } else {
            self.send_system_error(id, code, message, None).await;
        }
    }

    async fn handle_reader_closed(&mut self, id: ConnectionId, reason: ReaderEnd) {
        // Reader EOF / IO error — the plugin's outbound stream is done.
        // We don't immediately remove the connection from state; we wait
        // for the exit watcher to fire. Peers detecting plugin departure
        // is a plugin-level concern (heartbeat / farewell events); the
        // broker does not broadcast anything.

        tracing::debug!(conn = %id, ?reason, "reader loop ended");

        // For framing errors (LineTooLong), we additionally emit a
        // malformed_envelope error so §2's 16 MiB bound is enforced
        // visibly before close.
        if matches!(reason, ReaderEnd::LineTooLong) {
            self.send_system_error(
                id,
                ErrorCode::MalformedEnvelope,
                "line exceeded 16 MiB framing bound".into(),
                None,
            )
            .await;
        }

        // Pre-ready connections whose reader closes are torn down
        // immediately; post-ready connections wait on the exit watcher
        // to disambiguate clean exit vs crash. In-memory test transports
        // without an exit watcher fall through to the exit-less path —
        // they're reaped via the shared inbound channel when tests drop
        // the plugin side.
        let ready = self.conns.get(&id).map(|c| c.ready).unwrap_or(false);
        if !ready {
            self.force_close(id).await;
        }
    }

    async fn handle_exit(&mut self, id: ConnectionId, outcome: ExitOutcome) {
        let name = self
            .conns
            .get(&id)
            .map(|c| c.name.as_str().to_owned())
            .unwrap_or_default();
        tracing::info!(plugin = %name, ?outcome, "plugin exited");

        // Drop the connection state. The writer task will exit when its
        // channel closes.
        if let Some(conn) = self.conns.remove(&id) {
            drop(conn.send);
        }

        // Policy: the plugin set is a cooperating group. If one plugin
        // exits and others are still alive, propagate shutdown so the
        // session winds down as a whole instead of the remaining plugins
        // hanging on an engine with nothing to drive. The shutdown select
        // arm is already guarded against double-arming, and try_send
        // failing (channel full / closed) means a shutdown is already
        // in flight.
        if !self.conns.is_empty() {
            tracing::info!(
                trigger_plugin = %name,
                "peer exited; initiating engine shutdown"
            );
            let _ = self.shutdown_tx.try_send(DEFAULT_SHUTDOWN_GRACE_MS);
        }
    }

    // ---- helpers ----------------------------------------------------------

    async fn send_error_and_close(
        &mut self,
        id: ConnectionId,
        code: ErrorCode,
        message: String,
        offending: Option<Offending>,
    ) {
        self.send_system_error(id, code, message, offending).await;

        // Schedule close after the error drains.
        if let Some(conn) = self.conns.get_mut(&id) {
            let _ = conn.send.send(ConnectionOutbound::Close);
            conn.closing = true;
        }
    }

    async fn send_system_error(
        &self,
        id: ConnectionId,
        code: ErrorCode,
        message: String,
        offending: Option<Offending>,
    ) {
        let body = SystemBody::Error {
            code,
            message,
            offending,
        };
        self.send_system(id, body).await;
    }

    async fn send_system(&self, id: ConnectionId, body: SystemBody) {
        let env = Envelope::system(PluginName::engine(), Timestamp::now(), body);
        self.push_envelope(id, env).await;
    }

    async fn push_envelope(&self, id: ConnectionId, env: Envelope) {
        let Some(conn) = self.conns.get(&id) else {
            return;
        };
        // The send channel is unbounded so the broker never blocks. The
        // writer task enforces the §6 receive-queue cap with drop-oldest +
        // QueueOverflow emission to the receiver.
        if conn.send.send(ConnectionOutbound::Send(env)).is_err() {
            tracing::debug!(conn = %conn.id, "send queue closed (connection already torn down)");
        }
    }

    async fn broadcast_shutdown(&mut self, grace_ms: u64) {
        let targets: Vec<ConnectionId> = self
            .conns
            .iter()
            .filter(|(_, c)| !c.closing && c.ready)
            .map(|(peer_id, _)| *peer_id)
            .collect();
        for target_id in targets {
            self.send_system(
                target_id,
                SystemBody::Shutdown {
                    reason: Some("engine shutting down".into()),
                    grace_ms: Some(grace_ms),
                },
            )
            .await;
        }
    }

    async fn check_ready_timeouts(&mut self) {
        let now = Instant::now();
        let expired: Vec<ConnectionId> = self
            .conns
            .iter()
            .filter(|(_, c)| !c.ready && !c.closing && c.ready_deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            tracing::warn!(conn = %id, "ready timeout; closing connection");
            self.force_close(id).await;
        }
    }

    async fn force_close(&mut self, id: ConnectionId) {
        if let Some(conn) = self.conns.get_mut(&id) {
            let _ = conn.send.send(ConnectionOutbound::Close);
            conn.closing = true;
        }
    }

    /// Remove closed connections whose send queue has drained. Called at
    /// the top of every loop iteration so state stays in sync with the
    /// writer tasks.
    fn reap_closed(&mut self) {
        self.conns.retain(|_, c| !(c.closing && c.send.is_closed()));
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

// ---- parse-error → error-code mapping -----------------------------------

/// Map a [`ParseError`] to the §8 code, whether the connection should close,
/// and the human-readable message to embed in the error.
fn classify_parse_error(err: &ParseError) -> (ErrorCode, bool, String) {
    match err {
        // Framing-level errors that close per §8 footnote.
        ParseError::InvalidJson(_) => (ErrorCode::MalformedEnvelope, true, err.to_string()),
        // Ordinary envelope-level malformed_envelope: keep the connection.
        ParseError::ExtraFields(_)
        | ParseError::MissingField(_)
        | ParseError::NotAnObject
        | ParseError::WrongType { .. }
        | ParseError::InvalidType(_)
        | ParseError::InvalidTimestamp(_)
        | ParseError::EmptyFrom
        | ParseError::OutgoingHasStampedField(_)
        | ParseError::SystemBodyMissingKind
        | ParseError::SystemBodyKindNotString => {
            (ErrorCode::MalformedEnvelope, false, err.to_string())
        }
        ParseError::BodyNotObject => (ErrorCode::BodyNotObject, false, err.to_string()),
        ParseError::UnknownKind(_) => (ErrorCode::UnknownKind, false, err.to_string()),
        // Ready-body faults → invalid_ready, close. The variant itself
        // tells us the category, no string sniffing required.
        ParseError::InvalidReadyBody(_) => (ErrorCode::InvalidReady, true, err.to_string()),
        // Structural faults on non-ready system bodies → malformed_envelope,
        // keep the connection open. In practice this path triggers for
        // engine-sent-only kinds that a plugin sent with a bad shape —
        // we keep the library consumer well-covered even if no known
        // plugin flow reaches here.
        ParseError::InvalidSystemBody { .. } => {
            (ErrorCode::MalformedEnvelope, false, err.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ncp::transport::Transport;
    use nefor_protocol::{Body, MessageKind, PluginOutgoing, SystemBody};
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A mock transport backed by `tokio::io::duplex`. Returns the client
    /// half for the test to drive.
    struct MockPlugin {
        writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    }

    fn make_transport() -> (MockPlugin, Transport) {
        make_transport_buf(64 * 1024)
    }

    fn make_transport_buf(buf: usize) -> (MockPlugin, Transport) {
        // Broker side: reads from plugin's writer, writes to plugin's reader.
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

    async fn send_outgoing(p: &mut MockPlugin, out: PluginOutgoing) {
        let line = format!("{}\n", out.to_line());
        p.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn recv_envelope(p: &mut MockPlugin) -> Option<Envelope> {
        let mut line = String::new();
        let n = p.reader.read_line(&mut line).await.ok()?;
        if n == 0 {
            return None;
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        Envelope::parse_line(trimmed).ok()
    }

    fn ready_body(protocol_version: &str) -> PluginOutgoing {
        PluginOutgoing::system(SystemBody::Ready {
            protocol_version: protocol_version.into(),
        })
    }

    fn pn(s: &str) -> PluginName {
        PluginName::new(s).expect("valid plugin name")
    }

    // --- tests ---------------------------------------------------------

    #[tokio::test]
    async fn ready_accepts_valid_protocol_version() {
        let mut broker = Broker::new("0.1.0");
        let (mut p, t) = make_transport();
        broker.attach_transport(t, pn("test"));
        tokio::spawn(broker.run());

        send_outgoing(&mut p, ready_body("0.1")).await;

        let env = recv_envelope(&mut p).await.expect("ready_ok");
        assert!(matches!(env.body, Body::System(SystemBody::ReadyOk { .. })));
    }

    #[tokio::test]
    async fn ready_rejects_version_mismatch() {
        let mut broker = Broker::new("0.1.0");
        let (mut p, t) = make_transport();
        broker.attach_transport(t, pn("test"));
        tokio::spawn(broker.run());

        send_outgoing(&mut p, ready_body("0.2")).await;

        let env = recv_envelope(&mut p).await.expect("error");
        match env.body {
            Body::System(SystemBody::Error { code, .. }) => {
                assert_eq!(code, ErrorCode::ProtocolVersionMismatch);
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn broadcast_delivers_to_n_minus_1_peers() {
        let mut broker = Broker::new("0.1.0");
        let (mut pa, ta) = make_transport();
        let (mut pb, tb) = make_transport();
        let (mut pc, tc) = make_transport();
        broker.attach_transport(ta, pn("a"));
        broker.attach_transport(tb, pn("b"));
        broker.attach_transport(tc, pn("c"));
        tokio::spawn(broker.run());

        send_outgoing(&mut pa, ready_body("0.1")).await;
        recv_envelope(&mut pa).await.expect("ready_ok a");

        send_outgoing(&mut pb, ready_body("0.1")).await;
        recv_envelope(&mut pb).await.expect("ready_ok b");

        send_outgoing(&mut pc, ready_body("0.1")).await;
        recv_envelope(&mut pc).await.expect("ready_ok c");

        // Now a sends an event.
        let mut body = serde_json::Map::new();
        body.insert("k".into(), serde_json::Value::String("hello".into()));
        send_outgoing(&mut pa, PluginOutgoing::event(body)).await;

        // b and c each receive it; a does not.
        let env_b = recv_envelope(&mut pb).await.expect("b got event");
        let env_c = recv_envelope(&mut pc).await.expect("c got event");
        assert_eq!(env_b.kind, MessageKind::Event);
        assert_eq!(env_c.kind, MessageKind::Event);
        assert_eq!(env_b.from.as_str(), "a");
        assert_eq!(env_c.from.as_str(), "a");

        // Sender does not receive its own event.
        let pa_next =
            tokio::time::timeout(Duration::from_millis(100), recv_envelope(&mut pa)).await;
        assert!(
            pa_next.is_err() || pa_next.unwrap().is_none(),
            "sender must not receive its own event",
        );
    }

    #[tokio::test]
    async fn non_ready_first_message_is_invalid_ready() {
        let mut broker = Broker::new("0.1.0");
        let (mut p, t) = make_transport();
        broker.attach_transport(t, pn("p"));
        tokio::spawn(broker.run());

        let mut body = serde_json::Map::new();
        body.insert("k".into(), serde_json::Value::String("v".into()));
        send_outgoing(&mut p, PluginOutgoing::event(body)).await;

        let env = recv_envelope(&mut p).await.expect("error");
        match env.body {
            Body::System(SystemBody::Error { code, .. }) => {
                assert_eq!(code, ErrorCode::InvalidReady);
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parse_error_unknown_kind_does_not_close() {
        let mut broker = Broker::new("0.1.0");
        let (mut p, t) = make_transport();
        broker.attach_transport(t, pn("p"));
        tokio::spawn(broker.run());

        send_outgoing(&mut p, ready_body("0.1")).await;
        recv_envelope(&mut p).await; // ready_ok

        // Send a system message with an unknown kind — must be handcrafted
        // since the type system won't allow construction through
        // SystemBody.
        p.writer
            .write_all(b"{\"type\":\"system\",\"body\":{\"kind\":\"invented\"}}\n")
            .await
            .unwrap();

        let env = recv_envelope(&mut p).await.expect("error");
        match env.body {
            Body::System(SystemBody::Error { code, .. }) => {
                assert_eq!(code, ErrorCode::UnknownKind);
            }
            other => panic!("expected error, got {other:?}"),
        }

        // Connection should still be open — send a valid event and
        // observe no further error arrives.
        let mut body = serde_json::Map::new();
        body.insert("k".into(), serde_json::Value::String("v".into()));
        send_outgoing(&mut p, PluginOutgoing::event(body)).await;

        let next = tokio::time::timeout(Duration::from_millis(100), recv_envelope(&mut p)).await;
        assert!(next.is_err() || next.unwrap().is_none());
    }

    #[tokio::test]
    async fn broker_exits_when_no_plugins_configured() {
        let broker = Broker::new("0.1.0");
        let outcome = tokio::time::timeout(Duration::from_secs(2), broker.run())
            .await
            .expect("broker should exit quickly");
        assert_eq!(outcome, BrokerStopReason::AllPluginsGone);
    }

    #[tokio::test]
    async fn queue_overflow_sends_error_to_receiver() {
        // Tiny receiver-side duplex buffer + the 1024-deep send queue means
        // even a few thousand small events backs up enough to trigger the
        // bounded-queue overflow path.
        let mut broker = Broker::new("0.1.0");
        let (mut sender_plugin, sender_t) = make_transport();
        let (mut receiver_plugin, receiver_t) = make_transport_buf(256);
        broker.attach_transport(sender_t, pn("s"));
        broker.attach_transport(receiver_t, pn("r"));
        tokio::spawn(broker.run());

        send_outgoing(&mut sender_plugin, ready_body("0.1")).await;
        recv_envelope(&mut sender_plugin).await;
        send_outgoing(&mut receiver_plugin, ready_body("0.1")).await;
        recv_envelope(&mut receiver_plugin).await;

        for i in 0..3000u32 {
            let mut body = serde_json::Map::new();
            body.insert("i".into(), serde_json::json!(i));
            send_outgoing(&mut sender_plugin, PluginOutgoing::event(body)).await;
        }

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Drain receiver looking for at least one QueueOverflow error.
        let mut saw_overflow = false;
        for _ in 0..4000 {
            match tokio::time::timeout(
                Duration::from_millis(50),
                recv_envelope(&mut receiver_plugin),
            )
            .await
            {
                Ok(Some(env)) => {
                    if let Body::System(SystemBody::Error {
                        code: ErrorCode::QueueOverflow,
                        ..
                    }) = env.body
                    {
                        saw_overflow = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(
            saw_overflow,
            "expected at least one QueueOverflow error on the saturated receiver",
        );
    }

    #[tokio::test]
    async fn shutdown_sends_shutdown_to_plugins() {
        let mut broker = Broker::new("0.1.0");
        let (mut p, t) = make_transport();
        broker.attach_transport(t, pn("p"));
        let handle = broker.shutdown_handle();
        tokio::spawn(broker.run());

        send_outgoing(&mut p, ready_body("0.1")).await;
        recv_envelope(&mut p).await;

        handle.shutdown(50).await;
        // Plugin should observe shutdown system message.
        let env = recv_envelope(&mut p).await.expect("shutdown msg");
        assert!(matches!(
            env.body,
            Body::System(SystemBody::Shutdown { .. })
        ));
    }
}
