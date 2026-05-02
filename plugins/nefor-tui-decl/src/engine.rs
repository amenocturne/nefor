//! Engine state machine — owns the Lua host, reconciler, and renderer.
//!
//! The binary's event loop drives this through `handle_key`,
//! `handle_resize`, and `render_if_dirty`. Integration tests drive the
//! same surface in-process (no spawned subprocess, no /dev/tty).

use crate::error::TuiError;
use crate::input::KeyMessage;
use crate::lua_host::{LuaHost, SideEffect};
use crate::reconciler::Reconciler;
use crate::render::Renderer;

pub struct Engine {
    lua: LuaHost,
    reconciler: Reconciler,
    renderer: Renderer,
    needs_render: bool,
    exit_requested: bool,
}

impl Engine {
    /// Build an engine sized to `(width, height)`. The Lua host is
    /// installed but no scenario is loaded yet — callers must invoke
    /// [`Engine::load_scenario`] before driving the loop.
    pub fn new(width: u16, height: u16) -> Result<Self, TuiError> {
        Ok(Engine {
            lua: LuaHost::new()?,
            reconciler: Reconciler::new(),
            renderer: Renderer::new(width, height),
            needs_render: true,
            exit_requested: false,
        })
    }

    /// Load and execute a Lua source that calls `tui.start { ... }`.
    pub fn load_scenario(&mut self, lua_source: &str) -> Result<(), TuiError> {
        self.lua.load_source("scenario", lua_source)
    }

    /// Direct access to the Lua VM, e.g. for tests that build a message
    /// table.
    pub fn lua(&self) -> &mlua::Lua {
        self.lua.lua()
    }

    /// Dispatch a [`KeyMessage`]. Always sets the dirty flag in phase 1
    /// (always-dirty after dispatch is acceptable per orchestrator
    /// override Q2).
    pub fn handle_key(&mut self, key: KeyMessage) -> Result<(), TuiError> {
        let msg = self.lua().create_table()?;
        msg.set("kind", key.kind())?;
        let mods = self.lua().create_table()?;
        for (i, m) in key.mods.iter().enumerate() {
            mods.set(i + 1, *m)?;
        }
        msg.set("mods", mods)?;
        self.dispatch_msg(msg)
    }

    /// Dispatch an arbitrary Lua message table — used by the binary to
    /// inject NCP-routed events alongside synthetic key messages.
    pub fn dispatch_msg(&mut self, msg: mlua::Table) -> Result<(), TuiError> {
        let effects = self.lua.dispatch(msg)?;
        for e in effects {
            match e {
                SideEffect::Exit => self.exit_requested = true,
            }
        }
        self.needs_render = true;
        Ok(())
    }

    /// Apply a terminal resize. Forces a full redraw on the next render.
    pub fn handle_resize(&mut self, width: u16, height: u16) -> Result<(), TuiError> {
        self.renderer.resize(width, height);
        self.needs_render = true;
        Ok(())
    }

    /// Render if dirty. Returns the ANSI bytes; `None` means "no work".
    pub fn render_if_dirty(&mut self) -> Result<Option<Vec<u8>>, TuiError> {
        if !self.needs_render {
            return Ok(None);
        }
        let desc = self.lua.render_view()?;
        self.reconciler.reconcile(desc);
        let root = self.reconciler.root.as_ref().ok_or(TuiError::NotStarted)?;
        let bytes = self.renderer.render(root);
        self.needs_render = false;
        Ok(Some(bytes))
    }

    pub fn exit_requested(&self) -> bool {
        self.exit_requested
    }

    pub fn dimensions(&self) -> (u16, u16) {
        (self.renderer.width(), self.renderer.height())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COUNTER_SCENARIO: &str = r#"
        tui.start {
          initial_state = { count = 0 },
          view = function(s)
            return tui.column { gap = 0, children = {
              tui.padding { value = 1, child = tui.text { content = "count: " .. tostring(s.count) } },
            }}
          end,
          update = function(msg, s)
            if msg.kind == "key.space" then return { count = s.count + 1 }, {} end
            if msg.kind == "key.q"     then return s, { { kind = "exit" } } end
            return s, {}
          end,
        }
    "#;

    #[test]
    fn first_render_emits_full_frame() {
        let mut engine = Engine::new(40, 5).expect("engine");
        engine.load_scenario(COUNTER_SCENARIO).expect("load");
        let bytes = engine.render_if_dirty().expect("render").expect("dirty");
        let s = String::from_utf8(bytes).expect("utf-8");
        assert!(s.contains("count: 0"), "first frame should show count: 0");
    }

    #[test]
    fn space_key_increments_counter() {
        let mut engine = Engine::new(40, 5).expect("engine");
        engine.load_scenario(COUNTER_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("render");
        engine
            .handle_key(KeyMessage {
                name: "space".into(),
                mods: vec![],
            })
            .expect("space");
        let bytes = engine.render_if_dirty().expect("render").expect("dirty");
        let s = String::from_utf8(bytes).expect("utf-8");
        assert!(s.contains("count: 1"));
    }

    #[test]
    fn q_key_requests_exit() {
        let mut engine = Engine::new(40, 5).expect("engine");
        engine.load_scenario(COUNTER_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("render");
        assert!(!engine.exit_requested());
        engine
            .handle_key(KeyMessage {
                name: "q".into(),
                mods: vec![],
            })
            .expect("q");
        assert!(engine.exit_requested());
    }

    #[test]
    fn render_idempotent_when_clean() {
        let mut engine = Engine::new(40, 5).expect("engine");
        engine.load_scenario(COUNTER_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("first").expect("dirty");
        let again = engine.render_if_dirty().expect("second");
        assert!(again.is_none(), "no work when nothing changed");
    }

    #[test]
    fn resize_forces_redraw() {
        let mut engine = Engine::new(40, 5).expect("engine");
        engine.load_scenario(COUNTER_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("first").expect("dirty");
        engine.handle_resize(60, 8).expect("resize");
        assert_eq!(engine.dimensions(), (60, 8));
        let bytes = engine.render_if_dirty().expect("second").expect("dirty");
        let s = String::from_utf8(bytes).expect("utf-8");
        assert!(s.contains("\x1b[2J"), "post-resize should full-redraw");
    }
}
