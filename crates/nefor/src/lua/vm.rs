//! Lua VM lifecycle — bootstrap, init.lua loading, API installation.
//!
//! See the module-level doc on [`crate::lua`] for the threading model and
//! rationale. This file is the concrete orchestrator: `LuaHost::new` builds
//! the VM and installs every `nefor.*` table; `LuaHost::load_init` reads and
//! runs the user's `init.lua` with typed error reporting.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use mlua::{Lua, RegistryKey};

use crate::events::EventBus;
use crate::lua::bindings::{self, EngineOps};
use crate::lua::error::LuaError;
use crate::lua::log::log_to_lua_table;
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
    #[allow(dead_code)]
    bus: Arc<EventBus>,
    #[allow(dead_code)]
    plugins: SharedPluginRegistry,
    /// Registry key for the global `step` function, populated by
    /// [`LuaHost::cache_step`] after `init.lua` runs. `None` until then;
    /// [`LuaHost::invoke_step`] errors with [`LuaError::StepNotCached`] if
    /// called before caching.
    step: Option<RegistryKey>,
}

impl LuaHost {
    /// Construct a new VM and install the full `nefor.*` binding surface.
    ///
    /// Installs `nefor.engine`, `nefor.events`, `nefor.log`, `nefor.process`,
    /// `nefor.plugins`. The shared plugin registry is written to by
    /// `nefor.plugins.spawn` calls during `init.lua` and drained by the
    /// engine after load. `engine_ops` provides the routing sink used by
    /// `nefor.engine.send`.
    pub fn new(
        bus: Arc<EventBus>,
        plugins: SharedPluginRegistry,
        engine_ops: Arc<dyn EngineOps>,
    ) -> Result<Self, LuaError> {
        let lua = Lua::new();
        install_nefor_surface(
            &lua,
            Arc::clone(&bus),
            Arc::clone(&plugins),
            Arc::clone(&engine_ops),
        )
        .map_err(LuaError::VmInit)?;
        Ok(Self {
            lua,
            bus,
            plugins,
            step: None,
        })
    }

    /// Borrow the inner Lua VM. Exposed for follow-up bindings and tests.
    #[allow(dead_code)]
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Load and execute `path` as the user's `init.lua`.
    ///
    /// Reports typed errors in three shapes:
    /// - [`LuaError::InitLuaRead`] if the file can't be read.
    /// - [`LuaError::InitLuaExec`] if Lua errored during execution; the
    ///   source location is attached when mlua's error carries one.
    /// - No error on a clean run.
    pub fn load_init(&self, path: &Path) -> Result<(), LuaError> {
        let src = fs::read_to_string(path).map_err(LuaError::InitLuaRead)?;
        let path_buf = path.to_path_buf();
        let chunk_name = path.display().to_string();
        match self.lua.load(&src).set_name(chunk_name).exec() {
            Ok(()) => Ok(()),
            Err(source) => {
                let location = LuaError::location_from_mlua(&source, &path_buf);
                Err(LuaError::InitLuaExec { source, location })
            }
        }
    }

    /// Load and execute an in-memory Lua source string under a synthetic name.
    #[allow(dead_code)]
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

    /// Look up the global `step` function defined by `init.lua` and stash
    /// it in the Lua registry for repeat invocation.
    ///
    /// Returns [`LuaError::StepMissing`] when no such global exists or the
    /// global is not a function — both are fatal because the new engine
    /// model runs step on every inbound envelope and can't proceed without
    /// it.
    pub fn cache_step(&mut self) -> Result<(), LuaError> {
        let globals = self.lua.globals();
        let val: mlua::Value = globals.get("step").map_err(|_| LuaError::StepMissing)?;
        let func = match val {
            mlua::Value::Function(f) => f,
            _ => return Err(LuaError::StepMissing),
        };
        self.step = Some(self.lua.create_registry_value(func)?);
        Ok(())
    }

    /// Invoke `step(saved_log, current_log)`.
    ///
    /// Each log slice is converted to a Lua array of entry tables (see
    /// [`crate::lua::log::log_to_lua_table`]). Errors raised *inside* the
    /// step function are logged and swallowed — they must not take down the
    /// engine loop. VM-level errors (missing cache, registry corruption,
    /// conversion failure) bubble up as [`LuaError`].
    pub fn invoke_step(
        &self,
        saved_log: &[LogEntry],
        current_log: &[LogEntry],
    ) -> Result<(), LuaError> {
        let Some(key) = self.step.as_ref() else {
            return Err(LuaError::StepNotCached);
        };
        let func: mlua::Function = self.lua.registry_value(key)?;
        let saved = log_to_lua_table(&self.lua, saved_log)?;
        let current = log_to_lua_table(&self.lua, current_log)?;
        match func.call::<()>((saved, current)) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::error!(error = %e, "step invocation failed");
                Ok(())
            }
        }
    }
}

/// Install every `nefor.*` sub-table.
fn install_nefor_surface(
    lua: &Lua,
    bus: Arc<EventBus>,
    plugins: SharedPluginRegistry,
    engine_ops: Arc<dyn EngineOps>,
) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    bindings::install_engine(lua, &nefor, engine_ops)?;
    bindings::install_events(lua, &nefor, Arc::clone(&bus))?;
    bindings::install_log(lua, &nefor)?;
    bindings::install_process(lua, &nefor)?;
    bindings::install_plugins(lua, &nefor, plugins)?;
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
        LuaHost::new(bus, plugins, ops).expect("host ok")
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
                 and type(nefor.log) == 'table' \
                 and type(nefor.process) == 'table' \
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

    #[test]
    fn load_init_reads_file() {
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
        let res = h.load_init(&tmp);
        let _ = std::fs::remove_file(&tmp);
        res.expect("load_init should succeed");
    }

    #[test]
    fn load_init_missing_file_returns_read_error() {
        let h = host();
        let missing = std::env::temp_dir().join("nefor-definitely-not-here-xyz.lua");
        let _ = std::fs::remove_file(&missing);
        let err = h
            .load_init(&missing)
            .expect_err("missing file should error");
        assert!(matches!(err, LuaError::InitLuaRead(_)));
    }

    #[test]
    fn step_missing_returns_error() {
        let mut h = host();
        // init.lua defines other globals but no `step`.
        h.exec_str("init.lua", "other_global = 1").unwrap();
        let err = h.cache_step().expect_err("step missing must error");
        assert!(matches!(err, LuaError::StepMissing));
    }

    #[test]
    fn step_global_is_not_a_function() {
        let mut h = host();
        h.exec_str("init.lua", "step = 42").unwrap();
        let err = h.cache_step().expect_err("non-function step must error");
        assert!(matches!(err, LuaError::StepMissing));
    }

    #[test]
    fn invoke_step_before_cache_errors() {
        let h = host();
        let err = h
            .invoke_step(&[], &[])
            .expect_err("uncached step must error");
        assert!(matches!(err, LuaError::StepNotCached));
    }

    #[test]
    fn step_invocation_passes_log_tables() {
        let mut h = host();
        h.exec_str(
            "init.lua",
            "function step(s, c) global_s_len = #s; global_c_len = #c end",
        )
        .unwrap();
        h.cache_step().expect("cache ok");
        let saved: Vec<LogEntry> = (0..2)
            .map(|i| LogEntry {
                ts: ts(),
                origin: Origin::Plugin(plugin("mock-plugin")),
                target: None,
                payload: format!("sp{i}"),
            })
            .collect();
        let current: Vec<LogEntry> = (0..3)
            .map(|i| LogEntry {
                ts: ts(),
                origin: Origin::Plugin(plugin("mock-plugin")),
                target: None,
                payload: format!("cp{i}"),
            })
            .collect();
        h.invoke_step(&saved, &current).expect("invoke ok");
        let s_len: i64 = h.lua.globals().get("global_s_len").unwrap();
        let c_len: i64 = h.lua.globals().get("global_c_len").unwrap();
        assert_eq!(s_len, 2);
        assert_eq!(c_len, 3);
    }

    #[test]
    fn step_can_call_engine_send() {
        let ops = RecordOps::new();
        let mut h = host_with_ops(Arc::clone(&ops) as Arc<dyn EngineOps>);
        h.exec_str(
            "init.lua",
            "function step(s, c) nefor.engine.send(\"from-step\") end",
        )
        .unwrap();
        h.cache_step().expect("cache ok");
        h.invoke_step(&[], &[]).expect("invoke ok");
        let calls = ops.snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, SendTarget::Broadcast);
        assert_eq!(calls[0].1, "from-step");
    }

    #[test]
    fn step_error_is_logged_not_fatal() {
        let mut h = host();
        h.exec_str("init.lua", "function step() error(\"boom\") end")
            .unwrap();
        h.cache_step().expect("cache ok");
        // Should return Ok despite the Lua error — the engine loop must
        // continue running on handler failure.
        let res = h.invoke_step(&[], &[]);
        assert!(res.is_ok(), "step error must not be fatal; got {res:?}");
    }

    #[test]
    fn step_reads_log_entry_fields() {
        let mut h = host();
        h.exec_str(
            "init.lua",
            r#"
            function step(saved, current)
                captured_ts = current[1].ts
                captured_origin = current[1].origin
                captured_target = current[1].target
                captured_payload = current[1].payload
            end
            "#,
        )
        .unwrap();
        h.cache_step().expect("cache ok");
        let entry = LogEntry {
            ts: ts(),
            origin: Origin::Step,
            target: Some(plugin("mock-plugin")),
            payload: "pl".into(),
        };
        h.invoke_step(&[], std::slice::from_ref(&entry)).unwrap();
        let got_ts: String = h.lua.globals().get("captured_ts").unwrap();
        let origin: String = h.lua.globals().get("captured_origin").unwrap();
        let target: String = h.lua.globals().get("captured_target").unwrap();
        let payload: String = h.lua.globals().get("captured_payload").unwrap();
        assert_eq!(got_ts, "2026-04-23T00:00:00.000Z");
        assert_eq!(origin, "step");
        assert_eq!(target, "mock-plugin");
        assert_eq!(payload, "pl");
    }
}
