//! Lua VM lifecycle — bootstrap, init.lua loading, API installation.
//!
//! See the module-level doc on [`crate::lua`] for the threading model and
//! rationale. This file is the concrete orchestrator: `LuaHost::new` builds
//! the VM and installs every `nefor.*` table; `LuaHost::load_init` reads and
//! runs the user's `init.lua` with typed error reporting.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use mlua::Lua;

use crate::events::EventBus;
use crate::lua::bindings;
use crate::lua::error::LuaError;
use crate::ui::SharedRegistry;

/// Owns a Lua 5.4 VM with the `nefor.*` API installed.
///
/// `LuaHost` is intentionally not `Clone` — a process has one VM. The
/// underlying [`mlua::Lua`] handle is `Send + Sync + Clone` (mlua's `send`
/// feature), so pieces that need to call *into* the VM from other tasks
/// (event handlers, process stdio dispatchers) capture a clone of `lua()`
/// and call through it. The VM state is internally serialized.
pub struct LuaHost {
    lua: Lua,
    // Kept so future bindings can install themselves against the same bus
    // `install_events` already wired up. Not consumed after construction.
    #[allow(dead_code)]
    bus: Arc<EventBus>,
    // Same story for the widget registry — held so we can expose it later
    // if a new API needs direct access. Widget registration already routes
    // through it via `bindings::install_ui`.
    #[allow(dead_code)]
    registry: SharedRegistry,
}

impl LuaHost {
    /// Construct a new VM and install the full `nefor.*` binding surface.
    ///
    /// Installs `nefor.events`, `nefor.log`, `nefor.concurrency`,
    /// `nefor.ui`, and `nefor.process`. The shared widget registry must be
    /// the same one handed to [`crate::ui::app::run`] — `nefor.ui.register_widget`
    /// and the renderer read from the same `Arc<Mutex<_>>`.
    pub fn new(bus: Arc<EventBus>, registry: SharedRegistry) -> Result<Self, LuaError> {
        let lua = Lua::new();
        install_nefor_surface(&lua, Arc::clone(&bus), Arc::clone(&registry))
            .map_err(LuaError::VmInit)?;
        Ok(Self { lua, bus, registry })
    }

    /// Borrow the inner Lua VM. Exposed for follow-up bindings and tests —
    /// callers inside the binary don't typically need raw access.
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
    ///
    /// Per spec §Error handling: Lua errors that escape `init.lua` don't
    /// crash the binary — the caller logs and continues with whatever partial
    /// state Lua managed to set up before the error.
    pub fn load_init(&self, path: &Path) -> Result<(), LuaError> {
        let src = fs::read_to_string(path).map_err(LuaError::InitLuaRead)?;
        let path_buf = path.to_path_buf();
        // `set_name` threads the path into mlua's error messages so our
        // location parser has something to match against.
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
    ///
    /// Useful for integration tests and future REPL-style callers. Errors
    /// propagate through [`LuaError::InitLuaExec`] so location extraction
    /// works the same way as for a real `init.lua`.
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
}

/// Install every `nefor.*` sub-table.
fn install_nefor_surface(
    lua: &Lua,
    bus: Arc<EventBus>,
    registry: SharedRegistry,
) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    bindings::install_events(lua, &nefor, Arc::clone(&bus))?;
    bindings::install_log(lua, &nefor)?;
    bindings::install_concurrency(lua, &nefor)?;
    bindings::install_ui(lua, &nefor, Arc::clone(&bus), registry)?;
    bindings::install_process(lua, &nefor)?;
    lua.globals().set("nefor", nefor)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::WidgetRegistry;
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn host() -> LuaHost {
        let bus = Arc::new(EventBus::new());
        let registry: SharedRegistry = Arc::new(Mutex::new(WidgetRegistry::new()));
        LuaHost::new(bus, registry).expect("host ok")
    }

    #[test]
    fn new_installs_nefor_global() {
        let h = host();
        let ok: bool = h
            .lua
            .load(
                "return type(nefor) == 'table' \
                 and type(nefor.events) == 'table' \
                 and type(nefor.log) == 'table' \
                 and type(nefor.concurrency) == 'table' \
                 and type(nefor.ui) == 'table' \
                 and type(nefor.process) == 'table'",
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
        // Dangling `=` on line 2 — a clear syntax error.
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
        // Call a nil value — a runtime error pointing at the offending line.
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
}
