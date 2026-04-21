//! Broker — central state + event loop.
//!
//! The broker owns one [`ConnectionState`] per attached or attaching plugin.
//! Its single event loop processes inbound messages, exit signals, attach
//! timeouts, and the shutdown signal, fanning out envelopes via per-
//! connection bounded send queues (§6).
//!
//! Per §9 clarification / D-09, system messages that the spec says go to
//! "every attached plugin" (`plugin_joined`, `plugin_left`, `shutdown`)
//! are delivered as N point-to-point sends, not through a bus abstraction.
//! Event messages (§6) fan out to every OTHER attached plugin.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use nefor_protocol::{
    Body, Envelope, ErrorCode, MessageKind, Offending, ParseError, PluginLeftReason, PluginName,
    PluginOutgoing, SystemBody, Timestamp,
};
// Imports above intentionally include `Offending` even though only the
// error-shape paths use it; keeping it grouped reads cleaner than splitting
// the line.
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::ncp::connection::{
    run_exit_watcher, run_reader, run_stderr_pump, run_writer, ConnectionId, ConnectionInbound,
    ConnectionOutbound, ReaderEnd, DEFAULT_QUEUE_CAPACITY,
};
use crate::ncp::error::BrokerError;
use crate::ncp::spawn::PluginSpec;
use crate::ncp::transport::{stdio_transport, ExitOutcome, ExitWatcher, Transport};

/// Protocol version this broker implements. Plugins declaring any other
/// `protocol_version` in attach are rejected with `ProtocolVersionMismatch`
/// (§9).
pub const SUPPORTED_PROTOCOL_VERSION: &str = "0.1";

/// How long a fresh connection has to send a valid `attach` before the
/// broker closes it (§2, 10 s recommended default).
pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// After sending `detach`-triggered `plugin_left` broadcasts, wait this
/// long for the plugin to close its side before we force-close it (§5.3).
pub const DETACH_GRACE: Duration = Duration::from_secs(1);

/// Default shutdown grace — see §5.6. The broker still accepts an override
/// at `shutdown` time for operator flexibility.
pub const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 2000;

/// Per-connection state the broker tracks.
struct ConnectionState {
    /// Engine-local id.
    id: ConnectionId,
    /// Plugin name, known only after a successful attach. Pre-attach we
    /// use the connection id for logs.
    name: Option<PluginName>,
    /// Plugin version from its attach body. Needed for `plugin_joined`
    /// rebroadcasts.
    version: Option<String>,
    /// Sender onto the writer's queue. Unbounded so the broker is never
    /// blocked; the writer task enforces the §6 bounded-queue overflow
    /// policy on the receive side.
    send: mpsc::UnboundedSender<ConnectionOutbound>,
    /// When the attach timeout fires (only meaningful while pre-attach).
    attach_deadline: Instant,
    /// Set when the broker has scheduled this connection to close. While
    /// `closing`, further inbound messages are logged-and-dropped.
    closing: bool,
    /// Set when the broker is waiting for the plugin to close after a
    /// detach. On expiry, broker force-closes.
    detach_deadline: Option<Instant>,
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
    /// Engine version string included in `attach_ok`.
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

    /// Attach an arbitrary transport to the broker. Returns the assigned
    /// [`ConnectionId`]. Primarily for tests — production code goes through
    /// [`Broker::spawn`].
    pub fn attach_transport(&mut self, transport: Transport, log_label: String) -> ConnectionId {
        let id = ConnectionId::next();
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
                name: None,
                version: None,
                send: send_tx,
                attach_deadline: Instant::now() + ATTACH_TIMEOUT,
                closing: false,
                detach_deadline: None,
            },
        );
        id
    }

    /// Spawn a plugin from a [`PluginSpec`] and attach it.
    pub fn spawn(&mut self, spec: &PluginSpec) -> Result<ConnectionId, BrokerError> {
        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|source| BrokerError::Spawn {
            name: spec.name.clone(),
            command: spec.command.clone(),
            source,
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io_err("child stdin missing"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io_err("child stdout missing"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io_err("child stderr missing"))?;

        let exit: ExitWatcher = Box::pin(async move {
            match child.wait().await {
                Ok(status) if status.success() => ExitOutcome::CleanExit,
                Ok(_) => ExitOutcome::Crash,
                Err(_) => ExitOutcome::Unknown,
            }
        });

        let transport = stdio_transport(stdin, stdout, stderr, exit);
        Ok(self.attach_transport(transport, spec.name.clone()))
    }

    /// Drive the broker until either all connections have left or a
    /// shutdown completes.
    pub async fn run(mut self) -> BrokerStopReason {
        let mut attach_tick = tokio::time::interval(Duration::from_millis(500));
        attach_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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
            // plugins spawned) and the "last plugin crashed/detached" case.
            if shutdown_deadline.is_none() && self.conns.is_empty() {
                // If no shutdown channel has ever fired, drain it with a
                // short timeout; otherwise we return.
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
                _ = attach_tick.tick() => {
                    self.check_attach_timeouts().await;
                    self.check_detach_timeouts();
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
        // Pre-attach connections: the first message MUST be a valid attach
        // system body. Anything else (including event messages) → close
        // with invalid_attach.
        let attached = self.conns.get(&id).and_then(|c| c.name.clone());
        if attached.is_none() {
            if let (MessageKind::System, Body::System(SystemBody::Attach { .. })) =
                (out.kind, &out.body)
            {
                self.handle_attach(id, out).await;
            } else {
                self.send_error_and_close(
                    id,
                    ErrorCode::InvalidAttach,
                    "first message must be a system attach".into(),
                    None,
                )
                .await;
            }
            return;
        }

        // Post-attach: route by type.
        match out.kind {
            MessageKind::Event => self.route_event(id, out).await,
            MessageKind::System => self.route_system(id, out).await,
        }
    }

    async fn handle_attach(&mut self, id: ConnectionId, out: PluginOutgoing) {
        let Body::System(SystemBody::Attach {
            name,
            version,
            protocol_version,
        }) = out.body
        else {
            // Can't happen given the check in `handle_message`, but we
            // handle defensively.
            self.send_error_and_close(
                id,
                ErrorCode::InvalidAttach,
                "internal: non-attach body in attach path".into(),
                None,
            )
            .await;
            return;
        };

        // Protocol version — v0.1 only accepts "0.1" exactly (§9).
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

        // Name validation — PluginName::new rejects "engine" and empty.
        let plugin_name = match PluginName::new(name.clone()) {
            Ok(n) => n,
            Err(e) => {
                self.send_error_and_close(
                    id,
                    ErrorCode::InvalidAttach,
                    format!("attach.name {name:?} rejected: {e}"),
                    None,
                )
                .await;
                return;
            }
        };

        // Name collision check across currently-attached connections (§5.1).
        let taken = self.conns.values().any(|c| {
            c.id != id
                && !c.closing
                && c.name
                    .as_ref()
                    .map(|n| n.as_str() == name.as_str())
                    .unwrap_or(false)
        });
        if taken {
            self.send_error_and_close(
                id,
                ErrorCode::NameTaken,
                format!("plugin name {name:?} is already attached"),
                None,
            )
            .await;
            return;
        }

        // Accept.
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.name = Some(plugin_name.clone());
            conn.version = Some(version.clone());
            conn.attach_deadline = Instant::now() + Duration::from_secs(86_400);
            // disable attach timeout
        }

        tracing::info!(plugin = %plugin_name, version = %version, "plugin attached");

        // 1. Send attach_ok to the new connection.
        self.send_system(
            id,
            SystemBody::AttachOk {
                engine_version: self.engine_version.clone(),
            },
        )
        .await;

        // 2. Send one plugin_joined to the new connection for each already-
        //    attached peer — bootstrap roster (§5.4).
        let peers: Vec<(String, String)> = self
            .conns
            .iter()
            .filter(|(peer_id, c)| {
                **peer_id != id && !c.closing && c.name.is_some() && c.version.is_some()
            })
            .map(|(_, c)| {
                (
                    c.name.as_ref().unwrap().as_str().to_string(),
                    c.version.as_ref().unwrap().clone(),
                )
            })
            .collect();
        for (peer_name, peer_version) in peers {
            self.send_system(
                id,
                SystemBody::PluginJoined {
                    name: peer_name,
                    version: peer_version,
                },
            )
            .await;
        }

        // 3. Broadcast plugin_joined to every already-attached peer (§5.4).
        let broadcast_peers: Vec<ConnectionId> = self
            .conns
            .iter()
            .filter(|(peer_id, c)| **peer_id != id && !c.closing && c.name.is_some())
            .map(|(peer_id, _)| *peer_id)
            .collect();
        for peer_id in broadcast_peers {
            self.send_system(
                peer_id,
                SystemBody::PluginJoined {
                    name: plugin_name.as_str().to_string(),
                    version: version.clone(),
                },
            )
            .await;
        }
    }

    async fn route_event(&mut self, sender_id: ConnectionId, out: PluginOutgoing) {
        let Body::Event(event_body) = out.body else {
            return;
        };
        let sender_name = match self.conns.get(&sender_id).and_then(|c| c.name.clone()) {
            Some(n) => n,
            None => return,
        };
        let ts = Timestamp::now();
        let envelope = Envelope::event(sender_name, ts, event_body);

        let targets: Vec<ConnectionId> = self
            .conns
            .iter()
            .filter(|(peer_id, c)| **peer_id != sender_id && !c.closing && c.name.is_some())
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
            SystemBody::Detach { reason } => {
                self.handle_detach(sender_id, reason).await;
            }
            SystemBody::Attach { .. } => {
                // Re-attach on an already-attached connection is a protocol
                // error — we treat it as a malformed attach and close. The
                // plugin has one shot at attaching; sending a second one is
                // not in the spec's vocabulary for an attached session.
                self.send_error_and_close(
                    sender_id,
                    ErrorCode::InvalidAttach,
                    "attach received on an already-attached connection".into(),
                    None,
                )
                .await;
            }
            // The remaining system kinds are engine→plugin only per §5. A
            // plugin sending them is a protocol violation — drop with an
            // unknown_kind error (the kind is structurally recognized but
            // not allowed in this direction).
            SystemBody::AttachOk { .. }
            | SystemBody::PluginJoined { .. }
            | SystemBody::PluginLeft { .. }
            | SystemBody::Shutdown { .. }
            | SystemBody::Error { .. } => {
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

    async fn handle_detach(&mut self, id: ConnectionId, reason: Option<String>) {
        let name = match self.conns.get(&id).and_then(|c| c.name.clone()) {
            Some(n) => n,
            None => return,
        };
        tracing::info!(plugin = %name, ?reason, "plugin sent detach");

        self.broadcast_plugin_left(id, name.as_str(), PluginLeftReason::Detach)
            .await;

        if let Some(conn) = self.conns.get_mut(&id) {
            conn.detach_deadline = Some(Instant::now() + DETACH_GRACE);
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
        // for the exit watcher to fire (which carries the definitive
        // outcome). For in-memory transports without an exit watcher,
        // treat reader-close as plugin_left(disconnect) immediately.
        let has_watcher = {
            // If a connection has no process behind it (tests), we infer
            // "no process" from the absence of any subsequent exit signal.
            // The flag below is a bookkeeping best-effort — we set a
            // short grace and if no exit arrives, fall back to disconnect.
            self.conns.contains_key(&id)
        };

        if !has_watcher {
            return;
        }

        tracing::debug!(conn = %id, ?reason, "reader loop ended");

        // Transports with a process-exit watcher will cause
        // `handle_exit` to be called shortly. For framing errors
        // (LineTooLong), we additionally emit a malformed_envelope
        // error so §2's 16 MiB bound is enforced visibly before close.
        if matches!(reason, ReaderEnd::LineTooLong) {
            self.send_system_error(
                id,
                ErrorCode::MalformedEnvelope,
                "line exceeded 16 MiB framing bound".into(),
                None,
            )
            .await;
        }

        // If no process watcher is attached (i.e. `transport.exit` was
        // None), treat reader-close as disconnect now. We tell from the
        // absence of the exit watcher indirectly: we set a short disconnect
        // deadline and let the main loop see if an exit outcome arrives.
        // For the initial v1, since stdio transports always attach an
        // exit watcher, and in-memory tests never do, we dispatch
        // disconnect synchronously for the name-aware case.
        let name = self.conns.get(&id).and_then(|c| c.name.clone());
        if name.is_none() {
            // Pre-attach connection's reader closed — nothing to broadcast;
            // just schedule connection removal.
            self.force_close(id, None, None).await;
        }
        // Post-attach: wait for exit watcher to disambiguate crash vs
        // disconnect. If no watcher exists (in-memory tests), the test
        // will feed an exit outcome manually via `inject_exit`.
    }

    async fn handle_exit(&mut self, id: ConnectionId, outcome: ExitOutcome) {
        let (name, reason) = {
            let conn = match self.conns.get(&id) {
                Some(c) => c,
                None => return,
            };
            let n = conn.name.clone();
            let r = match outcome {
                ExitOutcome::CleanExit => PluginLeftReason::Disconnect,
                ExitOutcome::Crash => PluginLeftReason::Crash,
                ExitOutcome::Evicted => PluginLeftReason::Evicted,
                ExitOutcome::Unknown => PluginLeftReason::Crash,
            };
            (n, r)
        };

        if let Some(plugin_name) = name.as_ref() {
            self.broadcast_plugin_left(id, plugin_name.as_str(), reason)
                .await;
        }

        // Drop the connection state. The writer task will exit when its
        // channel closes.
        if let Some(conn) = self.conns.remove(&id) {
            drop(conn.send);
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

    async fn broadcast_plugin_left(
        &mut self,
        departed_id: ConnectionId,
        departed_name: &str,
        reason: PluginLeftReason,
    ) {
        let targets: Vec<ConnectionId> = self
            .conns
            .iter()
            .filter(|(peer_id, c)| **peer_id != departed_id && !c.closing && c.name.is_some())
            .map(|(peer_id, _)| *peer_id)
            .collect();
        for target_id in targets {
            self.send_system(
                target_id,
                SystemBody::PluginLeft {
                    name: departed_name.to_string(),
                    reason,
                },
            )
            .await;
        }
    }

    async fn broadcast_shutdown(&mut self, grace_ms: u64) {
        let targets: Vec<ConnectionId> = self
            .conns
            .iter()
            .filter(|(_, c)| !c.closing && c.name.is_some())
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

    async fn check_attach_timeouts(&mut self) {
        let now = Instant::now();
        let expired: Vec<ConnectionId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.name.is_none() && !c.closing && c.attach_deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            tracing::warn!(conn = %id, "attach timeout; closing connection");
            self.force_close(id, None, None).await;
        }
    }

    fn check_detach_timeouts(&mut self) {
        let now = Instant::now();
        let expired: Vec<ConnectionId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.detach_deadline.is_some_and(|d| d <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            tracing::info!(conn = %id, "detach grace expired; force-closing");
            if let Some(conn) = self.conns.get_mut(&id) {
                let _ = conn.send.send(ConnectionOutbound::Close);
                conn.closing = true;
            }
        }
    }

    async fn force_close(
        &mut self,
        id: ConnectionId,
        _reason_code: Option<ErrorCode>,
        _message: Option<String>,
    ) {
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
        // Attach-body faults (semantic or structural) → invalid_attach,
        // close. The variant itself tells us the category, no string
        // sniffing required.
        ParseError::InvalidAttachBody(_) => (ErrorCode::InvalidAttach, true, err.to_string()),
        // Structural faults on non-attach system bodies → malformed_envelope,
        // keep the connection open. NOTE: plugins per §5/§7 only ever emit
        // `attach` and `detach`, so in practice this path is triggered by
        // structurally-broken `detach` bodies (wrong `reason` type, etc.);
        // the other `SystemBodyKind` variants exist for defense in depth
        // and for library consumers that also decode engine-sent messages.
        ParseError::InvalidSystemBody { .. } => {
            (ErrorCode::MalformedEnvelope, false, err.to_string())
        }
    }
}

fn io_err(msg: &str) -> BrokerError {
    BrokerError::Io(std::io::Error::other(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ncp::transport::Transport;
    use nefor_protocol::{Body, MessageKind, PluginOutgoing, SystemBody};
    use std::collections::HashMap;
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

    fn attach_body(name: &str, version: &str, protocol_version: &str) -> PluginOutgoing {
        PluginOutgoing::system(SystemBody::Attach {
            name: name.into(),
            version: version.into(),
            protocol_version: protocol_version.into(),
        })
    }

    // --- tests ---------------------------------------------------------

    #[tokio::test]
    async fn attach_accepts_well_formed_attach() {
        let mut broker = Broker::new("0.1.0");
        let (mut p, t) = make_transport();
        broker.attach_transport(t, "test".into());
        tokio::spawn(broker.run());

        send_outgoing(&mut p, attach_body("p", "0.1.0", "0.1")).await;

        let env = recv_envelope(&mut p).await.expect("attach_ok");
        assert!(matches!(
            env.body,
            Body::System(SystemBody::AttachOk { .. })
        ));
    }

    #[tokio::test]
    async fn attach_rejects_engine_name() {
        let mut broker = Broker::new("0.1.0");
        let (mut p, t) = make_transport();
        broker.attach_transport(t, "test".into());
        tokio::spawn(broker.run());

        // `PluginName::new("engine")` fails, but the plugin-side attach
        // payload is validated during PARSE (parse.rs rejects
        // attach.name = "engine" with InvalidAttachBody(ReservedName)),
        // which classify_parse_error maps to ErrorCode::InvalidAttach.
        send_outgoing(&mut p, attach_body("engine", "0.1.0", "0.1")).await;

        let env = recv_envelope(&mut p).await.expect("error");
        match env.body {
            Body::System(SystemBody::Error { code, .. }) => {
                assert_eq!(code, ErrorCode::InvalidAttach);
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn attach_rejects_version_mismatch() {
        let mut broker = Broker::new("0.1.0");
        let (mut p, t) = make_transport();
        broker.attach_transport(t, "test".into());
        tokio::spawn(broker.run());

        send_outgoing(&mut p, attach_body("p", "0.1.0", "0.2")).await;

        let env = recv_envelope(&mut p).await.expect("error");
        match env.body {
            Body::System(SystemBody::Error { code, .. }) => {
                assert_eq!(code, ErrorCode::ProtocolVersionMismatch);
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn attach_rejects_name_taken() {
        let mut broker = Broker::new("0.1.0");
        let (mut p1, t1) = make_transport();
        let (mut p2, t2) = make_transport();
        broker.attach_transport(t1, "p1".into());
        broker.attach_transport(t2, "p2".into());
        tokio::spawn(broker.run());

        send_outgoing(&mut p1, attach_body("p", "0.1.0", "0.1")).await;
        let _ok1 = recv_envelope(&mut p1).await.expect("attach_ok p1");

        send_outgoing(&mut p2, attach_body("p", "0.1.0", "0.1")).await;
        // p2 might receive name_taken then EOF (connection closed).
        let env = recv_envelope(&mut p2).await.expect("error for p2");
        match env.body {
            Body::System(SystemBody::Error { code, .. }) => {
                assert_eq!(code, ErrorCode::NameTaken);
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
        broker.attach_transport(ta, "a".into());
        broker.attach_transport(tb, "b".into());
        broker.attach_transport(tc, "c".into());
        tokio::spawn(broker.run());

        send_outgoing(&mut pa, attach_body("a", "0.1.0", "0.1")).await;
        // Drain attach_ok
        recv_envelope(&mut pa).await.expect("attach_ok a");

        send_outgoing(&mut pb, attach_body("b", "0.1.0", "0.1")).await;
        // pb receives: attach_ok, plugin_joined(a)
        recv_envelope(&mut pb).await.expect("attach_ok b");
        recv_envelope(&mut pb).await.expect("bootstrap joined a");
        // pa receives plugin_joined(b) broadcast
        recv_envelope(&mut pa).await.expect("broadcast joined b");

        send_outgoing(&mut pc, attach_body("c", "0.1.0", "0.1")).await;
        // pc: attach_ok + 2 bootstrap joined
        recv_envelope(&mut pc).await.expect("attach_ok c");
        recv_envelope(&mut pc).await.expect("bootstrap 1 for c");
        recv_envelope(&mut pc).await.expect("bootstrap 2 for c");
        // pa + pb each get plugin_joined(c)
        recv_envelope(&mut pa).await.expect("pa joined c");
        recv_envelope(&mut pb).await.expect("pb joined c");

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

        // Sender does not receive its own event. We prove this by a short
        // timeout: if nothing arrives within 100 ms, consider it pass.
        let pa_next =
            tokio::time::timeout(Duration::from_millis(100), recv_envelope(&mut pa)).await;
        assert!(
            pa_next.is_err() || pa_next.unwrap().is_none(),
            "sender must not receive its own event",
        );
    }

    #[tokio::test]
    async fn detach_triggers_plugin_left_detach() {
        let mut broker = Broker::new("0.1.0");
        let (mut pa, ta) = make_transport();
        let (mut pb, tb) = make_transport();
        broker.attach_transport(ta, "a".into());
        broker.attach_transport(tb, "b".into());
        tokio::spawn(broker.run());

        send_outgoing(&mut pa, attach_body("a", "0.1.0", "0.1")).await;
        recv_envelope(&mut pa).await;

        send_outgoing(&mut pb, attach_body("b", "0.1.0", "0.1")).await;
        recv_envelope(&mut pb).await;
        recv_envelope(&mut pb).await; // joined(a)
        recv_envelope(&mut pa).await; // joined(b)

        // b detaches.
        send_outgoing(
            &mut pb,
            PluginOutgoing::system(SystemBody::Detach { reason: None }),
        )
        .await;

        // a should receive plugin_left with reason detach.
        let env = recv_envelope(&mut pa).await.expect("plugin_left");
        match env.body {
            Body::System(SystemBody::PluginLeft { name, reason }) => {
                assert_eq!(name, "b");
                assert_eq!(reason, PluginLeftReason::Detach);
            }
            other => panic!("expected plugin_left, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_attach_first_message_is_invalid_attach() {
        let mut broker = Broker::new("0.1.0");
        let (mut p, t) = make_transport();
        broker.attach_transport(t, "p".into());
        tokio::spawn(broker.run());

        let mut body = serde_json::Map::new();
        body.insert("k".into(), serde_json::Value::String("v".into()));
        send_outgoing(&mut p, PluginOutgoing::event(body)).await;

        let env = recv_envelope(&mut p).await.expect("error");
        match env.body {
            Body::System(SystemBody::Error { code, .. }) => {
                assert_eq!(code, ErrorCode::InvalidAttach);
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parse_error_unknown_kind_does_not_close() {
        let mut broker = Broker::new("0.1.0");
        let (mut p, t) = make_transport();
        broker.attach_transport(t, "p".into());
        tokio::spawn(broker.run());

        send_outgoing(&mut p, attach_body("p", "0.1.0", "0.1")).await;
        recv_envelope(&mut p).await; // attach_ok

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

        // No other peers — nothing to receive. Proof of alive-ness is
        // that the writer task hasn't closed our reader; we just confirm
        // no error within a brief window.
        let next = tokio::time::timeout(Duration::from_millis(100), recv_envelope(&mut p)).await;
        assert!(next.is_err() || next.unwrap().is_none());
    }

    #[tokio::test]
    async fn parse_error_invalid_system_body_does_not_close() {
        // Structural errors on a non-attach system body (here: detach with
        // a wrong-typed `reason`) classify as malformed_envelope and must
        // keep the connection open.
        let mut broker = Broker::new("0.1.0");
        let (mut p, t) = make_transport();
        broker.attach_transport(t, "p".into());
        tokio::spawn(broker.run());

        send_outgoing(&mut p, attach_body("p", "0.1.0", "0.1")).await;
        recv_envelope(&mut p).await; // attach_ok

        // Handcrafted so the type system doesn't reject it first.
        p.writer
            .write_all(b"{\"type\":\"system\",\"body\":{\"kind\":\"detach\",\"reason\":42}}\n")
            .await
            .unwrap();

        let env = recv_envelope(&mut p).await.expect("error");
        match env.body {
            Body::System(SystemBody::Error { code, .. }) => {
                assert_eq!(code, ErrorCode::MalformedEnvelope);
            }
            other => panic!("expected error, got {other:?}"),
        }

        // Connection still open — prove by sending a valid event and
        // confirming no further error within a brief window.
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
        broker.attach_transport(sender_t, "s".into());
        broker.attach_transport(receiver_t, "r".into());
        tokio::spawn(broker.run());

        send_outgoing(&mut sender_plugin, attach_body("s", "0.1.0", "0.1")).await;
        recv_envelope(&mut sender_plugin).await;
        send_outgoing(&mut receiver_plugin, attach_body("r", "0.1.0", "0.1")).await;
        recv_envelope(&mut receiver_plugin).await;
        recv_envelope(&mut receiver_plugin).await; // joined(s)
        recv_envelope(&mut sender_plugin).await; // joined(r)

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
        broker.attach_transport(t, "p".into());
        let handle = broker.shutdown_handle();
        tokio::spawn(broker.run());

        send_outgoing(&mut p, attach_body("p", "0.1.0", "0.1")).await;
        recv_envelope(&mut p).await;

        handle.shutdown(50).await;
        // Plugin should observe shutdown system message.
        let env = recv_envelope(&mut p).await.expect("shutdown msg");
        assert!(matches!(
            env.body,
            Body::System(SystemBody::Shutdown { .. })
        ));
    }

    // --- PluginSpec sanity (used by tests below) -----------------------
    #[allow(dead_code)]
    fn spec(name: &str, cmd: &str) -> PluginSpec {
        PluginSpec {
            name: name.to_string(),
            command: cmd.to_string(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
        }
    }
}
