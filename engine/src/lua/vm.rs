//! Lua VM lifecycle — bootstrap, init.lua loading, API installation.
//!
//! See the module-level doc on [`crate::lua`] for the threading model and
//! rationale. This file is the concrete orchestrator: `LuaHost::new` builds
//! the VM and installs every `nefor.*` table; `LuaHost::load_init` reads and
//! runs the user's `init.lua` with typed error reporting.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use mlua::{Lua, RegistryKey, Table};

use crate::events::EventBus;
use crate::lua::bindings::{
    self, EngineOps, EventSubscriptions, SharedStdinPump, SharedSubscriptions, StdinPump,
};
use crate::lua::error::LuaError;
use crate::lua::log::log_entry_to_lua_table;
use crate::lua::mode::EngineMode;
use crate::ncp::SharedPluginRegistry;
use crate::session::LogEntry;

/// Owns a Lua 5.4 VM with the `nefor.*` API installed.
///
/// `LuaHost` is intentionally not `Clone` — a process has one VM. The
/// underlying [`mlua::Lua`] handle is `Send + Sync + Clone` (mlua's `send`
/// feature), so callers that need to invoke into the VM from other tasks
/// clone the inner handle via [`LuaHost::lua`].
pub struct LuaHost {
    lua: Lua,
    bus: Arc<EventBus>,
    /// Held to keep the `Arc` alive for Lua closures that captured a clone.
    #[allow(dead_code)]
    plugins: SharedPluginRegistry,
    /// Registry key for the global `dispatch` function, populated by
    /// [`LuaHost::cache_dispatch`] after `init.lua` runs. `None` until
    /// then; [`LuaHost::invoke_dispatch`] errors with
    /// [`LuaError::DispatchNotCached`] if called before caching.
    dispatch: Option<RegistryKey>,
    /// Registry key for the global `invoke_from_plugin` function.
    /// Populated alongside `dispatch` by [`LuaHost::cache_dispatch`].
    /// Optional in the registry sense (not all test harnesses install it),
    /// but production `init.lua` always exposes it.
    invoke_from_plugin: Option<RegistryKey>,
    /// Registry key for the global `invoke_from_plugin_batch` function —
    /// the per-peer per-tick batched form of `invoke_from_plugin`. The
    /// broker prefers this when present so a multi-line stdin burst from
    /// one peer reaches the wrapper as one `from_plugin(envs)` call (the
    /// inbound mirror of Phase A's outbound `to_plugin(envs)` shape).
    /// Falls back to a per-payload loop on `invoke_from_plugin` when
    /// absent so older `init.lua` files keep working.
    invoke_from_plugin_batch: Option<RegistryKey>,
    /// Persistent Lua array mirroring the current session's log. Created
    /// lazily on the first [`LuaHost::invoke_dispatch`] call and reused —
    /// each subsequent call appends only the new entries since the last
    /// call, avoiding the O(n²) re-marshalling that an n-entry session
    /// would otherwise incur.
    current_log_table: Option<RegistryKey>,
    /// Number of entries already mirrored into `current_log_table`. Used
    /// to compute the slice to append on each invocation.
    current_log_mirrored: usize,
    /// `nefor.bus.on_event` registry. Populated by Lua at any time; read
    /// by [`LuaHost::dispatch_subscriptions`] right after each step
    /// invocation to fan out matching events.
    subscriptions: SharedSubscriptions,
    /// Stdin-pump receiver shared with `nefor.io.read_line`. Empty until
    /// CLI dispatch mode installs a pump via [`LuaHost::attach_stdin_pump`].
    stdin_pump: SharedStdinPump,
}

impl LuaHost {
    /// Construct a new VM and install the full `nefor.*` binding surface.
    ///
    /// Installs `nefor.engine`, `nefor.events`, `nefor.bus`, `nefor.io`,
    /// `nefor.log`, `nefor.process`, `nefor.plugins`. The shared plugin
    /// registry is written to by `nefor.plugins.spawn` calls during
    /// `init.lua` and drained by the engine after load. `engine_ops`
    /// provides the routing sink used by `nefor.engine.send`.
    ///
    /// The host starts in [`EngineMode::Serve`]; CLI-dispatch callers must
    /// invoke [`LuaHost::set_mode`] (and re-install bindings that depend
    /// on the mode, namely `nefor.io`). For the common-case serve path this
    /// is a no-op.
    pub fn new(
        bus: Arc<EventBus>,
        plugins: SharedPluginRegistry,
        engine_ops: Arc<dyn EngineOps>,
        data_dir: crate::paths::DataDir,
    ) -> Result<Self, LuaError> {
        let lua = Lua::new();
        let subscriptions: SharedSubscriptions = Arc::new(Mutex::new(EventSubscriptions::new()));
        let stdin_pump: SharedStdinPump = Arc::new(Mutex::new(StdinPump::empty()));
        install_nefor_surface(
            &lua,
            Arc::clone(&bus),
            Arc::clone(&plugins),
            Arc::clone(&engine_ops),
            Arc::clone(&subscriptions),
            EngineMode::Serve,
            Arc::clone(&stdin_pump),
            data_dir,
        )
        .map_err(LuaError::VmInit)?;
        Ok(Self {
            lua,
            bus,
            plugins,
            dispatch: None,
            invoke_from_plugin: None,
            invoke_from_plugin_batch: None,
            current_log_table: None,
            current_log_mirrored: 0,
            subscriptions,
            stdin_pump,
        })
    }

    /// Switch the host to the given [`EngineMode`] and re-install
    /// mode-dependent bindings (`nefor.io`). Idempotent. The bus, log,
    /// plugin, and engine bindings are mode-independent and are not
    /// reinstalled.
    pub fn set_mode(&mut self, mode: EngineMode) {
        let nefor: Table = match self.lua.globals().get("nefor") {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, "set_mode: nefor table missing");
                return;
            }
        };
        if let Err(e) = bindings::install_io(&self.lua, &nefor, mode, Arc::clone(&self.stdin_pump))
        {
            tracing::error!(error = %e, "set_mode: failed to reinstall nefor.io");
        }
    }

    /// Install the receiver end of the stdin pump. Used by the engine's
    /// dispatch path to bridge the binary's stdin to `nefor.io.read_line`.
    pub fn attach_stdin_pump(&self, rx: tokio::sync::mpsc::UnboundedReceiver<String>) {
        let mut guard = match self.stdin_pump.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.set_rx(rx);
    }

    /// Borrow the inner Lua VM. Exposed for follow-up bindings and tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Borrow the engine-internal lifecycle bus. The broker uses this to
    /// emit [`crate::events::SHUTDOWN`] inside its cooperative-shutdown
    /// grace window so Lua subscribers (`nefor.events.on("shutdown",
    /// ...)`) fire before plugin connections close.
    pub fn events_bus(&self) -> Arc<EventBus> {
        Arc::clone(&self.bus)
    }

    /// Load and execute `path` as the user's `init.lua`.
    ///
    /// Reports typed errors in three shapes:
    /// - [`LuaError::InitLuaRead`] if the file can't be read.
    /// - [`LuaError::InitLuaExec`] if Lua errored during execution; the
    ///   source location is attached when mlua's error carries one.
    /// - No error on a clean run.
    ///
    /// `async` only because callers slot this into a tokio task; the
    /// chunk itself runs via sync `exec`. `pm.install` ran on a real
    /// fresh-boot path uses the sync `nefor.process.run` / `nefor.fs.*`
    /// bindings so no async-Lua surface is exposed.
    pub async fn load_init(&self, path: &Path) -> Result<(), LuaError> {
        let src = fs::read_to_string(path).map_err(LuaError::InitLuaRead)?;
        let path_buf = path.to_path_buf();
        let chunk_name = path.display().to_string();

        // Expose the directory containing init.lua as `NEFOR_CONFIG_DIR` so
        // user code can resolve sibling files (e.g. `package.path` extension
        // for bundled Lua modules) without `debug.getinfo`. mlua's safe
        // stdlib subset omits `debug`.
        //
        // Canonicalize so user code can build absolute paths (e.g. plugin
        // commands that need to survive a cwd change in the spawned
        // process). Fall back to the literal display string if canonicalize
        // fails — better a relative path than no path.
        if let Some(parent) = path.parent() {
            let resolved = fs::canonicalize(parent)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| parent.display().to_string());
            if let Err(e) = self.lua.globals().set("NEFOR_CONFIG_DIR", resolved) {
                tracing::warn!("failed to set NEFOR_CONFIG_DIR: {e}");
            }
        }

        match self.lua.load(&src).set_name(chunk_name).exec() {
            Ok(()) => Ok(()),
            Err(source) => {
                let location = LuaError::location_from_mlua(&source, &path_buf);
                Err(LuaError::InitLuaExec { source, location })
            }
        }
    }

    /// Load and execute an in-memory Lua source string under a synthetic name.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn exec_str(&self, name: &str, source: &str) -> Result<(), LuaError> {
        let name_buf = std::path::PathBuf::from(name);
        match self.lua.load(source).set_name(name).exec() {
            Ok(()) => Ok(()),
            Err(source) => {
                let location = LuaError::location_from_mlua(&source, &name_buf);
                Err(LuaError::InitLuaExec { source, location })
            }
        }
    }

    /// Look up the global `dispatch` function defined by `init.lua` and
    /// stash it in the Lua registry for repeat invocation.
    ///
    /// Returns [`LuaError::DispatchMissing`] when no such global exists or
    /// the global is not a function — both are fatal because the engine
    /// runs dispatch on every inbound envelope and can't proceed without
    /// it.
    pub fn cache_dispatch(&mut self) -> Result<(), LuaError> {
        let globals = self.lua.globals();
        let val: mlua::Value = globals
            .get("dispatch")
            .map_err(|_| LuaError::DispatchMissing)?;
        let func = match val {
            mlua::Value::Function(f) => f,
            _ => return Err(LuaError::DispatchMissing),
        };
        self.dispatch = Some(self.lua.create_registry_value(func)?);

        // Optional: cache `invoke_from_plugin` if `init.lua` defines one.
        // Production starter wires this to `ncp.invoke_from_plugin`; test
        // harnesses that drive dispatch directly (not through inbound
        // lines) may omit it.
        if let Ok(mlua::Value::Function(f)) = globals.get::<mlua::Value>("invoke_from_plugin") {
            self.invoke_from_plugin = Some(self.lua.create_registry_value(f)?);
        }
        // Optional: cache `invoke_from_plugin_batch` for the broker's
        // batched dispatch path. Production `starter/ncp.lua` exposes it;
        // if missing, the broker falls back to N per-payload calls on
        // `invoke_from_plugin`.
        if let Ok(mlua::Value::Function(f)) = globals.get::<mlua::Value>("invoke_from_plugin_batch")
        {
            self.invoke_from_plugin_batch = Some(self.lua.create_registry_value(f)?);
        }
        Ok(())
    }

    /// Invoke `dispatch(current_log)`.
    ///
    /// The Lua table is *persistent*: created on the first call, then
    /// reused. `new_current_entries` carries only the entries appended
    /// since the previous call — converting the full log every time would
    /// be O(n²) per session, which dominated typing latency on the
    /// keystroke→render path. The caller (broker) clones just the small
    /// tail under its lock, avoiding an O(n) clone of the full event log
    /// on every inbound line.
    ///
    /// Errors raised *inside* the dispatch function are logged and
    /// swallowed — they must not take down the engine loop. VM-level
    /// errors (missing cache, registry corruption, conversion failure)
    /// bubble up as [`LuaError`].
    pub fn invoke_dispatch(&mut self, new_current_entries: &[LogEntry]) -> Result<(), LuaError> {
        let Some(key) = self.dispatch.as_ref() else {
            return Err(LuaError::DispatchNotCached);
        };
        let func: mlua::Function = self.lua.registry_value(key)?;

        // current_log: persistent table, append-only on each call.
        let current: Table = match self.current_log_table.as_ref() {
            Some(k) => self.lua.registry_value(k)?,
            None => {
                let t = self.lua.create_table()?;
                self.current_log_table = Some(self.lua.create_registry_value(t.clone())?);
                t
            }
        };
        if !new_current_entries.is_empty() {
            for (offset, entry) in new_current_entries.iter().enumerate() {
                let lua_idx = self.current_log_mirrored + offset + 1; // 1-indexed
                current.set(lua_idx, log_entry_to_lua_table(&self.lua, entry)?)?;
            }
            self.current_log_mirrored += new_current_entries.len();
        }

        match func.call::<()>(current) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::error!(error = %e, "dispatch invocation failed");
                Ok(())
            }
        }
    }

    /// Invoke the global `invoke_from_plugin(source, payload)` Lua hook.
    ///
    /// Called by the broker for each inbound plugin line (post-callback
    /// refactor). The Lua side is responsible for:
    /// - decoding the payload + handling NCP-level system messages (ready
    ///   handshake, etc.) — these don't go through wrapper callbacks;
    /// - dispatching the parsed envelope to the corresponding wrapper's
    ///   `from_plugin(env)` callback (or the framework default which
    ///   publishes it verbatim onto the bus via `nefor.engine.send`).
    ///
    /// Errors raised inside the hook are logged and swallowed — same
    /// policy as `invoke_dispatch`. A missing global is silently a no-op
    /// so test harnesses that don't install one keep working.
    pub fn invoke_from_plugin(&self, source: &str, payload: &str) -> Result<(), LuaError> {
        let Some(key) = self.invoke_from_plugin.as_ref() else {
            return Ok(());
        };
        let func: mlua::Function = self.lua.registry_value(key)?;
        match func.call::<()>((source.to_owned(), payload.to_owned())) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::error!(error = %e, "invoke_from_plugin handler raised");
                Ok(())
            }
        }
    }

    /// Invoke the global `invoke_from_plugin_batch(source, payloads)` Lua
    /// hook. `payloads` is a list of raw inbound lines from one peer
    /// already drained from the broker's inbound channel within a single
    /// dispatch tick; the Lua side decodes each, classifies system vs
    /// event, and fires the wrapper's `from_plugin(envs)` callback ONCE
    /// with all event envelopes.
    ///
    /// Falls back to a per-payload loop on `invoke_from_plugin` when the
    /// Lua side hasn't installed a batched entry point — preserves
    /// behaviour for older `init.lua` files.
    ///
    /// Errors raised inside the hook are logged and swallowed (same
    /// policy as `invoke_from_plugin`).
    pub fn invoke_from_plugin_batch(
        &self,
        source: &str,
        payloads: &[String],
    ) -> Result<(), LuaError> {
        if payloads.is_empty() {
            return Ok(());
        }
        if let Some(key) = self.invoke_from_plugin_batch.as_ref() {
            let func: mlua::Function = self.lua.registry_value(key)?;
            let table = self.lua.create_table()?;
            for (i, p) in payloads.iter().enumerate() {
                table.set(i + 1, p.as_str())?; // 1-indexed
            }
            match func.call::<()>((source.to_owned(), table)) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::error!(error = %e, "invoke_from_plugin_batch handler raised");
                    return Ok(());
                }
            }
        }
        // Fallback: per-payload loop on the unbatched hook.
        for p in payloads {
            self.invoke_from_plugin(source, p)?;
        }
        Ok(())
    }

    /// Dispatch `nefor.bus.on_event` subscribers for each entry in
    /// `entries`. Each entry's `payload` is parsed as JSON to extract
    /// `body.kind`; pattern-matching subscriptions are invoked with the
    /// envelope as a Lua table.
    ///
    /// Errors raised inside a handler are logged and swallowed (D-21a-style
    /// — same policy as `dispatch`); subsequent handlers still run. Entries
    /// whose payload is not parseable JSON, has no `body.kind`, etc., are
    /// silently skipped — bus subscribers explicitly speak the kind layer
    /// and don't see protocol-malformed traffic. (`dispatch` still saw it;
    /// the distinction is: `dispatch` is the protocol authority, on_event
    /// is a convenience over kind-based routing.)
    pub fn dispatch_subscriptions(&self, entries: &[LogEntry]) {
        let snap = {
            let guard = match self.subscriptions.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.snapshot()
        };
        if snap.is_empty() {
            return;
        }
        for entry in entries {
            let Some(kind) = extract_body_kind(&entry.payload) else {
                continue;
            };
            for (pattern, handler_key) in &snap {
                if !pattern.matches(&kind) {
                    continue;
                }
                let handler: mlua::Function = match self.lua.registry_value(handler_key.as_ref()) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(error = %e, "bus.on_event handler missing from registry");
                        continue;
                    }
                };
                let env = match log_entry_to_lua_table(&self.lua, entry) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to convert entry for handler");
                        continue;
                    }
                };
                if let Err(e) = handler.call::<()>(env) {
                    tracing::error!(error = %e, kind = %kind, "bus.on_event handler raised");
                }
            }
        }
    }

    /// Invoke the cli function registered under `name` (via
    /// `nefor.plugins.spawn { cli = ... }`) with `argv` as a 1-indexed
    /// Lua table. Returns the integer the cli function returned, or 0 if
    /// it returned nil / a non-integer. Errors raised inside the
    /// function propagate as a Lua error and are mapped to a non-zero
    /// exit (1) by the caller.
    pub fn invoke_cli(&self, name: &str, argv: &[String]) -> Result<i32, LuaError> {
        let registry: mlua::Table = self
            .lua
            .globals()
            .get(crate::lua::bindings::plugins::CLI_REGISTRY_GLOBAL)
            .map_err(LuaError::Other)?;
        let func: mlua::Function = registry.get(name).map_err(LuaError::Other)?;

        let args = self.lua.create_table().map_err(LuaError::Other)?;
        for (i, a) in argv.iter().enumerate() {
            args.set(i + 1, self.lua.create_string(a).map_err(LuaError::Other)?)
                .map_err(LuaError::Other)?;
        }

        match func.call::<mlua::Value>(args) {
            Ok(mlua::Value::Integer(n)) => Ok(i32::try_from(n).unwrap_or(0)),
            Ok(mlua::Value::Number(n)) if n.fract() == 0.0 => {
                Ok(i32::try_from(n as i64).unwrap_or(0))
            }
            Ok(_) => Ok(0),
            Err(e) => {
                tracing::error!(plugin = %name, error = %e, "cli function raised");
                Err(LuaError::Other(e))
            }
        }
    }
}

/// Extract `body.kind` from a JSON payload string. Returns `None` for
/// any failure to parse, missing fields, or non-string `kind` — the
/// dispatch path treats such entries as "no kind to match" and moves on.
fn extract_body_kind(payload: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let body = v.get("body")?;
    let kind = body.get("kind")?;
    kind.as_str().map(|s| s.to_owned())
}

/// Install every `nefor.*` sub-table.
#[allow(clippy::too_many_arguments)]
fn install_nefor_surface(
    lua: &Lua,
    bus: Arc<EventBus>,
    plugins: SharedPluginRegistry,
    engine_ops: Arc<dyn EngineOps>,
    subscriptions: SharedSubscriptions,
    mode: EngineMode,
    stdin_pump: SharedStdinPump,
    data_dir: crate::paths::DataDir,
) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    bindings::install_engine(lua, &nefor, engine_ops)?;
    bindings::install_events(lua, &nefor, Arc::clone(&bus))?;
    bindings::install_fs(lua, &nefor, data_dir)?;
    bindings::install_json(lua, &nefor)?;
    bindings::install_log(lua, &nefor)?;
    bindings::install_process(lua, &nefor)?;
    bindings::install_plugins(lua, &nefor, plugins)?;
    bindings::install_bus(lua, &nefor, subscriptions)?;
    bindings::install_io(lua, &nefor, mode, stdin_pump)?;
    lua.globals().set("nefor", nefor)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::bindings::SendTarget;
    use crate::ncp::PluginRegistry;
    use crate::session::Origin;
    use nefor_protocol::{PluginName, Timestamp};
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn ts() -> Timestamp {
        Timestamp::parse("2026-04-23T00:00:00.000Z").expect("valid ts")
    }

    fn plugin(name: &str) -> PluginName {
        PluginName::new(name).expect("valid plugin name")
    }

    struct NullOps;
    impl EngineOps for NullOps {
        fn send(&self, _target: SendTarget, _payload: String) {}
        fn plugins(&self) -> Vec<PluginName> {
            Vec::new()
        }
    }

    struct RecordOps {
        calls: Mutex<Vec<(SendTarget, String)>>,
    }
    impl RecordOps {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }
        fn snapshot(&self) -> Vec<(SendTarget, String)> {
            self.calls.lock().unwrap().clone()
        }
    }
    impl EngineOps for RecordOps {
        fn send(&self, target: SendTarget, payload: String) {
            self.calls.lock().unwrap().push((target, payload));
        }
        fn plugins(&self) -> Vec<PluginName> {
            Vec::new()
        }
    }

    fn host() -> LuaHost {
        host_with_ops(Arc::new(NullOps) as Arc<dyn EngineOps>)
    }

    fn host_with_ops(ops: Arc<dyn EngineOps>) -> LuaHost {
        let bus = Arc::new(EventBus::new());
        let plugins: SharedPluginRegistry = Arc::new(Mutex::new(PluginRegistry::new()));
        let data_dir = crate::paths::DataDir::new(PathBuf::from("/var/empty/nefor-test"));
        LuaHost::new(bus, plugins, ops, data_dir).expect("host ok")
    }

    #[test]
    fn new_installs_nefor_global() {
        let h = host();
        let ok: bool = h
            .lua
            .load(
                "return type(nefor) == 'table' \
                 and type(nefor.engine) == 'table' \
                 and type(nefor.engine.send) == 'function' \
                 and type(nefor.events) == 'table' \
                 and type(nefor.json) == 'table' \
                 and type(nefor.json.encode) == 'function' \
                 and type(nefor.json.decode) == 'function' \
                 and type(nefor.log) == 'table' \
                 and type(nefor.process) == 'table' \
                 and type(nefor.process.run) == 'function' \
                 and type(nefor.fs) == 'table' \
                 and type(nefor.fs.mkdir_p) == 'function' \
                 and type(nefor.fs.read_file) == 'function' \
                 and type(nefor.plugins) == 'table'",
            )
            .eval()
            .unwrap();
        assert!(ok);
    }

    #[test]
    fn exec_simple_string_succeeds() {
        let h = host();
        h.exec_str("test.lua", "nefor.log.info('hi from lua')")
            .expect("exec ok");
    }

    #[test]
    fn syntax_error_returns_init_lua_exec_with_location() {
        let h = host();
        let src = "local x = 1\nx =\n";
        let err = h.exec_str("bad.lua", src).expect_err("must error");
        match err {
            LuaError::InitLuaExec { location, .. } => {
                let loc = location.expect("syntax errors should carry a location");
                assert_eq!(loc.file, PathBuf::from("bad.lua"));
                assert!(loc.line >= 1);
            }
            other => panic!("expected InitLuaExec, got {other:?}"),
        }
    }

    #[test]
    fn runtime_error_returns_init_lua_exec_with_location() {
        let h = host();
        let src = "local x = 1\nundefined_func()\n";
        let err = h.exec_str("bad.lua", src).expect_err("must error");
        match err {
            LuaError::InitLuaExec { location, .. } => {
                let loc = location.expect("runtime errors with chunk names carry locations");
                assert_eq!(loc.file, PathBuf::from("bad.lua"));
                assert_eq!(loc.line, 2);
            }
            other => panic!("expected InitLuaExec, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_init_reads_file() {
        let h = host();
        let tmp = std::env::temp_dir().join(format!(
            "nefor-test-init-{}-{}.lua",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::write(&tmp, "nefor.log.info('loaded from disk')").expect("write ok");
        let res = h.load_init(&tmp).await;
        let _ = std::fs::remove_file(&tmp);
        res.expect("load_init should succeed");
    }

    #[tokio::test]
    async fn load_init_missing_file_returns_read_error() {
        let h = host();
        let missing = std::env::temp_dir().join("nefor-definitely-not-here-xyz.lua");
        let _ = std::fs::remove_file(&missing);
        let err = h
            .load_init(&missing)
            .await
            .expect_err("missing file should error");
        assert!(matches!(err, LuaError::InitLuaRead(_)));
    }

    #[test]
    fn dispatch_missing_returns_error() {
        let mut h = host();
        // init.lua defines other globals but no `dispatch`.
        h.exec_str("init.lua", "other_global = 1").unwrap();
        let err = h.cache_dispatch().expect_err("dispatch missing must error");
        assert!(matches!(err, LuaError::DispatchMissing));
    }

    #[test]
    fn dispatch_global_is_not_a_function() {
        let mut h = host();
        h.exec_str("init.lua", "dispatch = 42").unwrap();
        let err = h
            .cache_dispatch()
            .expect_err("non-function dispatch must error");
        assert!(matches!(err, LuaError::DispatchMissing));
    }

    #[test]
    fn invoke_dispatch_before_cache_errors() {
        let mut h = host();
        let err = h
            .invoke_dispatch(&[])
            .expect_err("uncached dispatch must error");
        assert!(matches!(err, LuaError::DispatchNotCached));
    }

    #[test]
    fn dispatch_invocation_passes_log_table() {
        let mut h = host();
        h.exec_str("init.lua", "function dispatch(c) global_c_len = #c end")
            .unwrap();
        h.cache_dispatch().expect("cache ok");
        let current: Vec<LogEntry> = (0..3)
            .map(|i| LogEntry {
                ts: ts(),
                origin: Origin::Plugin(plugin("mock-plugin")),
                target: None,
                payload: format!("cp{i}"),
            })
            .collect();
        h.invoke_dispatch(&current).expect("invoke ok");
        let c_len: i64 = h.lua.globals().get("global_c_len").unwrap();
        assert_eq!(c_len, 3);
    }

    #[test]
    fn dispatch_can_call_engine_send() {
        let ops = RecordOps::new();
        let mut h = host_with_ops(Arc::clone(&ops) as Arc<dyn EngineOps>);
        h.exec_str(
            "init.lua",
            "function dispatch(c) nefor.engine.send(\"from-dispatch\") end",
        )
        .unwrap();
        h.cache_dispatch().expect("cache ok");
        h.invoke_dispatch(&[]).expect("invoke ok");
        let calls = ops.snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, SendTarget::Broadcast);
        assert_eq!(calls[0].1, "from-dispatch");
    }

    #[test]
    fn dispatch_error_is_logged_not_fatal() {
        let mut h = host();
        h.exec_str("init.lua", "function dispatch() error(\"boom\") end")
            .unwrap();
        h.cache_dispatch().expect("cache ok");
        // Should return Ok despite the Lua error — the engine loop must
        // continue running on handler failure.
        let res = h.invoke_dispatch(&[]);
        assert!(res.is_ok(), "dispatch error must not be fatal; got {res:?}");
    }

    #[test]
    fn dispatch_reads_log_entry_fields() {
        let mut h = host();
        h.exec_str(
            "init.lua",
            r#"
            function dispatch(current)
                captured_ts = current[1].ts
                captured_origin = current[1].origin
                captured_target = current[1].target
                captured_payload = current[1].payload
            end
            "#,
        )
        .unwrap();
        h.cache_dispatch().expect("cache ok");
        let entry = LogEntry {
            ts: ts(),
            origin: Origin::Step,
            target: Some(plugin("mock-plugin")),
            payload: "pl".into(),
        };
        h.invoke_dispatch(std::slice::from_ref(&entry)).unwrap();
        let got_ts: String = h.lua.globals().get("captured_ts").unwrap();
        let origin: String = h.lua.globals().get("captured_origin").unwrap();
        let target: String = h.lua.globals().get("captured_target").unwrap();
        let payload: String = h.lua.globals().get("captured_payload").unwrap();
        assert_eq!(got_ts, "2026-04-23T00:00:00.000Z");
        assert_eq!(origin, "step");
        assert_eq!(target, "mock-plugin");
        assert_eq!(payload, "pl");
    }

    fn entry(payload: &str) -> LogEntry {
        LogEntry {
            ts: ts(),
            origin: Origin::Plugin(plugin("p")),
            target: None,
            payload: payload.into(),
        }
    }

    #[test]
    fn extract_body_kind_returns_string() {
        let p = r#"{"type":"event","from":"a","ts":"x","body":{"kind":"chat.input"}}"#;
        assert_eq!(extract_body_kind(p), Some("chat.input".to_string()));
    }

    #[test]
    fn extract_body_kind_missing_returns_none() {
        let p = r#"{"type":"event","from":"a","ts":"x","body":{}}"#;
        assert_eq!(extract_body_kind(p), None);
    }

    #[test]
    fn extract_body_kind_garbage_returns_none() {
        assert_eq!(extract_body_kind("not json"), None);
    }

    #[test]
    fn on_event_exact_dispatch_fires_handler() {
        let h = host();
        h.exec_str(
            "init.lua",
            r#"
            saw = {}
            nefor.bus.on_event("chat.input", function(env)
                saw[#saw + 1] = env.payload
            end)
            "#,
        )
        .unwrap();
        let e1 = entry(r#"{"type":"event","from":"p","ts":"x","body":{"kind":"chat.input"}}"#);
        let e2 = entry(r#"{"type":"event","from":"p","ts":"x","body":{"kind":"chat.other"}}"#);
        h.dispatch_subscriptions(&[e1.clone(), e2]);
        let saw: mlua::Table = h.lua.globals().get("saw").unwrap();
        let len = saw.len().unwrap();
        assert_eq!(len, 1);
        let payload: String = saw.get(1).unwrap();
        assert_eq!(payload, e1.payload);
    }

    #[test]
    fn on_event_prefix_dispatch_fires_handler() {
        let h = host();
        h.exec_str(
            "init.lua",
            r#"
            count = 0
            nefor.bus.on_event("chat.*", function(env) count = count + 1 end)
            "#,
        )
        .unwrap();
        let entries = vec![
            entry(r#"{"body":{"kind":"chat.input"}}"#),
            entry(r#"{"body":{"kind":"chat.message.append"}}"#),
            entry(r#"{"body":{"kind":"unrelated.kind"}}"#),
        ];
        h.dispatch_subscriptions(&entries);
        let count: i64 = h.lua.globals().get("count").unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn on_event_handler_error_is_swallowed_and_subsequent_run() {
        let h = host();
        h.exec_str(
            "init.lua",
            r#"
            tally = 0
            -- First handler raises every time.
            nefor.bus.on_event("k", function() error("boom") end)
            -- Second handler must still fire.
            nefor.bus.on_event("k", function() tally = tally + 1 end)
            "#,
        )
        .unwrap();
        let e = entry(r#"{"body":{"kind":"k"}}"#);
        h.dispatch_subscriptions(&[e]);
        let tally: i64 = h.lua.globals().get("tally").unwrap();
        assert_eq!(tally, 1, "second handler must still run after first errors");
    }
}
