//! `nefor.engine.send` / `nefor.engine.deliver` — outbound paths from Lua.
//!
//! Dispatch (`init.lua`'s `function dispatch(current_log) ... end`) is
//! the policy engine. Two structurally different operations live here:
//!
//! * [`send`](EngineOps::send) — **emission**. The Lua caller is publishing
//!   a new envelope onto the bus. The broker stamps it as `Origin::Step`,
//!   appends a [`LogEntry`](crate::session::LogEntry), and writes the line
//!   to the target peer (broadcast = every connected peer). One emission,
//!   one log entry. Lua exposes this as `nefor.engine.send(payload, target?)`.
//! * [`deliver`](EngineOps::deliver) — **delivery**. The Lua caller is
//!   forwarding an emission that's already been logged (e.g. ncp.lua's
//!   per-peer fan-out of a plugin's broadcast event). The broker writes
//!   to the named peer's stdin without appending a new log entry.
//!   Targeted only — broadcast belongs to `send`. Lua exposes this as
//!   `nefor.engine.deliver(peer, payload)`.
//!
//! This split keeps the bus log canonical: a "1 emission → 1 entry"
//! invariant. Without it the dispatch hook's per-peer fan-out for a
//! single broadcast emission would synthesize N step entries — meaning
//! late attachers couldn't replay step-origin entries (the filter would
//! re-deliver every fan-out copy).
//!
//! This module validates argument shape, wraps the target in a
//! [`SendTarget`] enum (no stringly-typed "target or empty" state), and
//! delegates the actual routing to an [`EngineOps`] trait. The production
//! implementation wires to the broker + event log; the test implementation
//! records calls for assertion.

use std::sync::Arc;

use mlua::{Lua, Table, Value};
use nefor_protocol::{PluginName, Timestamp};

/// Outbound routing decision produced by a dispatch call.
///
/// Enum rather than `Option<PluginName>` so the intent (broadcast vs
/// targeted) is self-documenting at use sites and new variants (e.g., a
/// future "reply to origin") can be added without touching callers that
/// already pattern-match exhaustively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendTarget {
    /// Broadcast to all connected plugins.
    Broadcast,
    /// Deliver only to the named plugin.
    Targeted(PluginName),
}

/// Engine-side routing surface the Lua `nefor.engine.send` binding calls into.
///
/// Production wiring lives in the broker (I3+); tests pass a recording
/// implementation. Kept `Send + Sync` because the Lua VM is shared across
/// tasks via mlua's `send` feature.
pub trait EngineOps: Send + Sync {
    /// Emit `payload` onto the bus and route to `target`.
    ///
    /// The implementation appends a `LogEntry` (origin = Step) so the
    /// emission is part of the canonical log, then writes the line to
    /// every (broadcast) or one (targeted) connected plugin's stdin.
    ///
    /// Infallible from the binding's perspective — the engine is responsible
    /// for surfacing any delivery failure asynchronously (e.g., through the
    /// event log). Making `send` fallible here would force the dispatch
    /// function to reason about transport-level errors, which is not its
    /// job.
    fn send(&self, target: SendTarget, payload: String);

    /// Forward `payload` to one peer's stdin **without** appending a log
    /// entry.
    ///
    /// Used by the dispatch hook's per-peer fan-out: when ncp.lua receives
    /// a broadcast emission from one plugin and routes copies to every
    /// other connected peer, each copy is a delivery, not a new emission.
    /// The original emission already produced its single canonical log
    /// entry at ingress (Origin::Plugin). Logging the fan-out copies as
    /// well would muddy the log — replaying step-origin entries to late
    /// attachers would double-deliver every event.
    ///
    /// Returns `Err` if `target` is not currently connected. Unlike
    /// `send`'s infallible signature, the caller of `deliver` is the
    /// in-VM Lua code that just enumerated `nefor.engine.plugins()` —
    /// it has full agency to decide what to do on a TOCTOU disconnect
    /// (typically: log + drop). Default impl returns `Ok(())` so test
    /// recorders that don't care about transport semantics stay terse.
    fn deliver(&self, _target: PluginName, _payload: String) -> Result<(), String> {
        Ok(())
    }

    /// Snapshot the names of plugins currently connected to the engine.
    ///
    /// Used by `starter/ncp.lua` to implement broadcast-minus-sender: NCP
    /// broadcast excludes the sender, while `nefor.engine.send` broadcast
    /// reaches every plugin. Lua enumerates the set and issues N-1 targeted
    /// sends instead.
    fn plugins(&self) -> Vec<PluginName>;

    /// Request engine shutdown with the given exit code. The implementation
    /// signals the broker to wind down (closing every plugin connection's
    /// outbound channel within the cooperative-shutdown grace) and stashes
    /// the requested exit code so the caller can read it back after the
    /// broker's run loop returns. Idempotent — first call wins.
    ///
    /// Default impl is a no-op so test recorders that don't care about
    /// shutdown signalling stay terse. Production wires this to the
    /// broker's shutdown handle + an `AtomicI32` exit-code slot.
    fn request_exit(&self, _code: i32) {}
}

/// Install `nefor.engine.send` onto `nefor_tbl`.
pub fn install_engine(lua: &Lua, nefor_tbl: &Table, ops: Arc<dyn EngineOps>) -> mlua::Result<()> {
    let engine = lua.create_table()?;

    let ops_for_send = Arc::clone(&ops);
    let send_fn = lua.create_function(move |_, args: mlua::Variadic<Value>| {
        let payload = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            Some(other) => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.engine.send: payload must be a string (got {})",
                    other.type_name(),
                )));
            }
            None => {
                return Err(mlua::Error::runtime(
                    "nefor.engine.send: payload required (first argument must be a string)",
                ));
            }
        };
        let target = match args.get(1) {
            None | Some(Value::Nil) => SendTarget::Broadcast,
            Some(Value::String(s)) => {
                let name = PluginName::new(s.to_str()?.to_owned())
                    .map_err(|e| mlua::Error::runtime(format!("nefor.engine.send: {e}")))?;
                SendTarget::Targeted(name)
            }
            Some(other) => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.engine.send: target must be a string or nil (got {})",
                    other.type_name(),
                )));
            }
        };
        ops_for_send.send(target, payload);
        Ok(())
    })?;
    engine.set("send", send_fn)?;

    // nefor.engine.deliver(peer, payload) — write `payload` to one peer's
    // stdin without appending a log entry. Used by the dispatch hook's
    // per-peer fan-out (the original emission already produced its
    // canonical Plugin entry at ingress). Targeted only — there is no
    // broadcast variant because a fan-out call site always names one
    // peer at a time. See module-level docstring for the send vs deliver
    // split.
    let ops_for_deliver = Arc::clone(&ops);
    let deliver_fn = lua.create_function(move |_, args: mlua::Variadic<Value>| {
        let peer = match args.first() {
            Some(Value::String(s)) => {
                let raw = s.to_str()?.to_owned();
                PluginName::new(raw)
                    .map_err(|e| mlua::Error::runtime(format!("nefor.engine.deliver: {e}")))?
            }
            Some(other) => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.engine.deliver: peer must be a string (got {})",
                    other.type_name(),
                )));
            }
            None => {
                return Err(mlua::Error::runtime(
                    "nefor.engine.deliver: peer required (first argument must be a string)",
                ));
            }
        };
        let payload = match args.get(1) {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            Some(other) => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.engine.deliver: payload must be a string (got {})",
                    other.type_name(),
                )));
            }
            None => {
                return Err(mlua::Error::runtime(
                    "nefor.engine.deliver: payload required (second argument must be a string)",
                ));
            }
        };
        if let Err(e) = ops_for_deliver.deliver(peer, payload) {
            return Err(mlua::Error::runtime(format!("nefor.engine.deliver: {e}")));
        }
        Ok(())
    })?;
    engine.set("deliver", deliver_fn)?;

    // nefor.engine.now() returns an ISO-8601 timestamp with millisecond
    // precision — the wire format spec §3 requires for the `ts` field.
    // Starter's NCP module uses this to stamp outbound envelopes with the
    // engine's authoritative clock. Exposed from here rather than leaving
    // Lua to build its own timestamp because Lua's stdlib has no millisecond
    // precision and no UTC helper.
    let now_fn = lua.create_function(|_, _: ()| Ok(Timestamp::now().to_iso8601()))?;
    engine.set("now", now_fn)?;

    let ops_for_plugins = Arc::clone(&ops);
    let plugins_fn = lua.create_function(move |lua, _: ()| {
        let names = ops_for_plugins.plugins();
        let arr = lua.create_table()?;
        for (i, name) in names.into_iter().enumerate() {
            // Lua arrays are 1-indexed; the Lua caller iterates with ipairs.
            arr.set(i + 1, lua.create_string(name.as_str())?)?;
        }
        Ok(arr)
    })?;
    engine.set("plugins", plugins_fn)?;

    // nefor.engine.exit(code?) — request a clean shutdown with the given
    // exit code (defaults to 0). Broadcasts the cascade-close to every
    // plugin's outbound queue, then the engine process terminates with
    // the requested code once the broker's run loop unwinds.
    let ops_for_exit = Arc::clone(&ops);
    let exit_fn = lua.create_function(move |_, args: mlua::Variadic<Value>| {
        let code: i32 = match args.first() {
            None | Some(Value::Nil) => 0,
            Some(Value::Integer(i)) => i32::try_from(*i).map_err(|_| {
                mlua::Error::runtime(format!("nefor.engine.exit: code {i} does not fit in i32"))
            })?,
            Some(Value::Number(n)) => {
                if n.fract() != 0.0 {
                    return Err(mlua::Error::runtime(format!(
                        "nefor.engine.exit: code must be an integer (got {n})"
                    )));
                }
                let i = *n as i64;
                i32::try_from(i).map_err(|_| {
                    mlua::Error::runtime(format!("nefor.engine.exit: code {i} does not fit in i32"))
                })?
            }
            Some(other) => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.engine.exit: code must be an integer or nil (got {})",
                    other.type_name(),
                )));
            }
        };
        ops_for_exit.request_exit(code);
        Ok(())
    })?;
    engine.set("exit", exit_fn)?;

    nefor_tbl.set("engine", engine)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordOps {
        calls: Mutex<Vec<(SendTarget, String)>>,
        deliveries: Mutex<Vec<(PluginName, String)>>,
        plugins: Mutex<Vec<PluginName>>,
        exit_code: Mutex<Option<i32>>,
        // When set, deliver() returns Err with this message — lets tests
        // exercise the error surface without needing a real broker.
        deliver_err: Mutex<Option<String>>,
    }

    impl RecordOps {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                deliveries: Mutex::new(Vec::new()),
                plugins: Mutex::new(Vec::new()),
                exit_code: Mutex::new(None),
                deliver_err: Mutex::new(None),
            })
        }

        fn snapshot(&self) -> Vec<(SendTarget, String)> {
            self.calls.lock().unwrap().clone()
        }

        fn delivered(&self) -> Vec<(PluginName, String)> {
            self.deliveries.lock().unwrap().clone()
        }

        fn set_plugins(&self, names: Vec<PluginName>) {
            *self.plugins.lock().unwrap() = names;
        }

        fn set_deliver_err(&self, msg: &str) {
            *self.deliver_err.lock().unwrap() = Some(msg.to_owned());
        }

        fn exit_code(&self) -> Option<i32> {
            *self.exit_code.lock().unwrap()
        }
    }

    impl EngineOps for RecordOps {
        fn send(&self, target: SendTarget, payload: String) {
            self.calls.lock().unwrap().push((target, payload));
        }
        fn deliver(&self, target: PluginName, payload: String) -> Result<(), String> {
            if let Some(e) = self.deliver_err.lock().unwrap().clone() {
                return Err(e);
            }
            self.deliveries.lock().unwrap().push((target, payload));
            Ok(())
        }
        fn plugins(&self) -> Vec<PluginName> {
            self.plugins.lock().unwrap().clone()
        }
        fn request_exit(&self, code: i32) {
            *self.exit_code.lock().unwrap() = Some(code);
        }
    }

    fn setup() -> (Lua, Arc<RecordOps>) {
        let lua = Lua::new();
        let ops = RecordOps::new();
        let nefor = lua.create_table().unwrap();
        install_engine(&lua, &nefor, Arc::clone(&ops) as Arc<dyn EngineOps>).unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        (lua, ops)
    }

    #[test]
    fn engine_send_broadcast_records_call() {
        let (lua, ops) = setup();
        lua.load(r#"nefor.engine.send("hello")"#).exec().unwrap();
        let calls = ops.snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, SendTarget::Broadcast);
        assert_eq!(calls[0].1, "hello");
    }

    #[test]
    fn engine_send_targeted_records_call() {
        let (lua, ops) = setup();
        lua.load(r#"nefor.engine.send("hi", "mock-plugin")"#)
            .exec()
            .unwrap();
        let calls = ops.snapshot();
        assert_eq!(calls.len(), 1);
        let expected = PluginName::new("mock-plugin").unwrap();
        assert_eq!(calls[0].0, SendTarget::Targeted(expected));
        assert_eq!(calls[0].1, "hi");
    }

    #[test]
    fn engine_send_rejects_non_string_payload() {
        let (lua, ops) = setup();
        let err = lua
            .load(r#"nefor.engine.send({})"#)
            .exec()
            .expect_err("table payload must be rejected");
        assert!(
            err.to_string().contains("payload must be a string"),
            "got: {err}"
        );
        assert!(ops.snapshot().is_empty());
    }

    #[test]
    fn engine_send_rejects_non_string_target() {
        let (lua, ops) = setup();
        let err = lua
            .load(r#"nefor.engine.send("x", 42)"#)
            .exec()
            .expect_err("integer target must be rejected");
        assert!(
            err.to_string().contains("target must be a string"),
            "got: {err}"
        );
        assert!(ops.snapshot().is_empty());
    }

    #[test]
    fn engine_send_rejects_empty_payload_missing() {
        let (lua, ops) = setup();
        let err = lua
            .load(r#"nefor.engine.send()"#)
            .exec()
            .expect_err("missing payload must be rejected");
        assert!(err.to_string().contains("payload required"), "got: {err}");
        assert!(ops.snapshot().is_empty());
    }

    #[test]
    fn engine_send_accepts_empty_string_payload() {
        let (lua, ops) = setup();
        lua.load(r#"nefor.engine.send("")"#).exec().unwrap();
        let calls = ops.snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, SendTarget::Broadcast);
        assert_eq!(calls[0].1, "");
    }

    #[test]
    fn engine_send_nil_target_is_broadcast() {
        let (lua, ops) = setup();
        lua.load(r#"nefor.engine.send("x", nil)"#).exec().unwrap();
        let calls = ops.snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, SendTarget::Broadcast);
    }

    #[test]
    fn engine_plugins_returns_empty_array_when_none() {
        let (lua, _ops) = setup();
        let n: i64 = lua
            .load(r#"return #nefor.engine.plugins()"#)
            .eval()
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn engine_plugins_returns_names_as_array() {
        let (lua, ops) = setup();
        ops.set_plugins(vec![
            PluginName::new("a").unwrap(),
            PluginName::new("b").unwrap(),
            PluginName::new("c").unwrap(),
        ]);
        let concat: String = lua
            .load(
                r#"
                local names = nefor.engine.plugins()
                table.sort(names)
                return table.concat(names, ",")
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(concat, "a,b,c");
    }

    #[test]
    fn engine_send_rejects_reserved_target_name() {
        let (lua, ops) = setup();
        // PluginName::new rejects the reserved name "engine"; the binding
        // should surface that as a Lua error rather than silently broadcasting.
        let err = lua
            .load(r#"nefor.engine.send("x", "engine")"#)
            .exec()
            .expect_err("reserved name must be rejected");
        assert!(err.to_string().contains("nefor.engine.send"), "got: {err}");
        assert!(ops.snapshot().is_empty());
    }

    #[test]
    fn engine_deliver_records_call_to_target() {
        let (lua, ops) = setup();
        lua.load(r#"nefor.engine.deliver("mock-plugin", "hello")"#)
            .exec()
            .unwrap();
        let delivered = ops.delivered();
        assert_eq!(delivered.len(), 1);
        let expected = PluginName::new("mock-plugin").unwrap();
        assert_eq!(delivered[0].0, expected);
        assert_eq!(delivered[0].1, "hello");
        // Critically, deliver must NOT trigger send (which logs as Step).
        assert!(
            ops.snapshot().is_empty(),
            "deliver must not invoke EngineOps::send (no LogEntry)"
        );
    }

    #[test]
    fn engine_deliver_rejects_missing_peer() {
        let (lua, ops) = setup();
        let err = lua
            .load(r#"nefor.engine.deliver()"#)
            .exec()
            .expect_err("missing peer must be rejected");
        assert!(err.to_string().contains("peer required"), "got: {err}");
        assert!(ops.delivered().is_empty());
    }

    #[test]
    fn engine_deliver_rejects_missing_payload() {
        let (lua, ops) = setup();
        let err = lua
            .load(r#"nefor.engine.deliver("p")"#)
            .exec()
            .expect_err("missing payload must be rejected");
        assert!(err.to_string().contains("payload required"), "got: {err}");
        assert!(ops.delivered().is_empty());
    }

    #[test]
    fn engine_deliver_rejects_non_string_peer() {
        let (lua, ops) = setup();
        let err = lua
            .load(r#"nefor.engine.deliver(42, "x")"#)
            .exec()
            .expect_err("integer peer must be rejected");
        assert!(
            err.to_string().contains("peer must be a string"),
            "got: {err}"
        );
        assert!(ops.delivered().is_empty());
    }

    #[test]
    fn engine_deliver_rejects_non_string_payload() {
        let (lua, ops) = setup();
        let err = lua
            .load(r#"nefor.engine.deliver("p", {})"#)
            .exec()
            .expect_err("table payload must be rejected");
        assert!(
            err.to_string().contains("payload must be a string"),
            "got: {err}"
        );
        assert!(ops.delivered().is_empty());
    }

    #[test]
    fn engine_deliver_rejects_reserved_target_name() {
        let (lua, ops) = setup();
        let err = lua
            .load(r#"nefor.engine.deliver("engine", "x")"#)
            .exec()
            .expect_err("reserved name must be rejected");
        assert!(
            err.to_string().contains("nefor.engine.deliver"),
            "got: {err}"
        );
        assert!(ops.delivered().is_empty());
    }

    #[test]
    fn engine_deliver_surfaces_unknown_peer_error() {
        let (lua, ops) = setup();
        ops.set_deliver_err("target plugin 'nope' is not connected");
        let err = lua
            .load(r#"nefor.engine.deliver("nope", "x")"#)
            .exec()
            .expect_err("unknown peer must surface as Lua error");
        assert!(err.to_string().contains("not connected"), "got: {err}");
    }

    #[test]
    fn engine_send_and_deliver_are_independent_paths() {
        // A `send` records a call (logs Step entry); a `deliver` records
        // a delivery (no log entry). Same VM, both surfaces, no
        // cross-contamination.
        let (lua, ops) = setup();
        lua.load(
            r#"
            nefor.engine.send("emitted", "a")
            nefor.engine.deliver("b", "delivered")
            "#,
        )
        .exec()
        .unwrap();
        assert_eq!(ops.snapshot().len(), 1, "one send recorded");
        assert_eq!(ops.delivered().len(), 1, "one delivery recorded");
        assert_eq!(ops.snapshot()[0].1, "emitted");
        assert_eq!(ops.delivered()[0].1, "delivered");
    }

    #[test]
    fn engine_exit_default_code_is_zero() {
        let (lua, ops) = setup();
        lua.load(r#"nefor.engine.exit()"#).exec().unwrap();
        assert_eq!(ops.exit_code(), Some(0));
    }

    #[test]
    fn engine_exit_explicit_code_is_recorded() {
        let (lua, ops) = setup();
        lua.load(r#"nefor.engine.exit(42)"#).exec().unwrap();
        assert_eq!(ops.exit_code(), Some(42));
    }

    #[test]
    fn engine_exit_nil_code_is_zero() {
        let (lua, ops) = setup();
        lua.load(r#"nefor.engine.exit(nil)"#).exec().unwrap();
        assert_eq!(ops.exit_code(), Some(0));
    }

    #[test]
    fn engine_exit_rejects_non_integer() {
        let (lua, ops) = setup();
        let err = lua
            .load(r#"nefor.engine.exit("oops")"#)
            .exec()
            .expect_err("string code must be rejected");
        assert!(err.to_string().contains("must be an integer"));
        assert_eq!(ops.exit_code(), None);
    }

    #[test]
    fn engine_exit_rejects_fractional_number() {
        let (lua, ops) = setup();
        let err = lua
            .load(r#"nefor.engine.exit(1.5)"#)
            .exec()
            .expect_err("fractional code must be rejected");
        assert!(err.to_string().contains("must be an integer"));
        assert_eq!(ops.exit_code(), None);
    }
}
