//! Engine state machine — owns the Lua host, reconciler, and renderer.
//!
//! The binary's event loop drives this through `handle_key`,
//! `handle_resize`, and `render_if_dirty`. Integration tests drive the
//! same surface in-process (no spawned subprocess, no /dev/tty).

use crate::error::TuiError;
use crate::input::KeyMessage;
use crate::input_router::{route_key, RouteDecision};
use crate::instance::sync_text_inputs;
use crate::lua_host::{LuaHost, SideEffect};
use crate::mouse::{hit_test, kind_string, MouseMessage};
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

    /// Dispatch a [`KeyMessage`]. The router first inspects the current
    /// reconciled tree: if a focused `text_input` exists and the key is
    /// an editing key, the router mutates the input's internal state in
    /// place and dispatches the configured `on_change` / `on_submit`
    /// callbacks to Lua. Otherwise the key bubbles to Lua as
    /// `{ kind = "key.<name>", mods = [...] }`.
    pub fn handle_key(&mut self, key: KeyMessage) -> Result<(), TuiError> {
        // Ensure we have a current reconciled tree so the router can
        // inspect the latest description. The first key event would
        // otherwise see an empty reconciler.
        self.ensure_reconciled()?;

        if let Some(root) = self.reconciler.root.as_mut() {
            match route_key(root, &key) {
                RouteDecision::HandledByTextInput {
                    target_key,
                    on_change,
                    on_submit,
                    value,
                    value_changed,
                    submitted,
                } => {
                    if value_changed {
                        if let Some(kind) = on_change {
                            self.dispatch_named(&kind, &target_key, Some(&value))?;
                        }
                    }
                    if submitted {
                        if let Some(kind) = on_submit {
                            self.dispatch_named(&kind, &target_key, Some(&value))?;
                        }
                    }
                    // Even with no Lua-visible callbacks (e.g. cursor
                    // moves, copy with no clipboard), set dirty so the
                    // next render repaints the cursor / selection.
                    self.needs_render = true;
                    return Ok(());
                }
                RouteDecision::BubbleToLua => {}
            }
        }

        let msg = self.lua().create_table()?;
        msg.set("kind", key.kind())?;
        let mods = self.lua().create_table()?;
        for (i, m) in key.mods.iter().enumerate() {
            mods.set(i + 1, *m)?;
        }
        msg.set("mods", mods)?;
        self.dispatch_msg(msg)
    }

    /// Build `{ kind, target_key, value? }` and dispatch through Lua's
    /// `update`. Used to relay text_input `on_change` / `on_submit`
    /// callbacks.
    fn dispatch_named(
        &mut self,
        kind: &str,
        target_key: &str,
        value: Option<&str>,
    ) -> Result<(), TuiError> {
        let msg = self.lua().create_table()?;
        msg.set("kind", kind)?;
        msg.set("target_key", target_key)?;
        if let Some(v) = value {
            msg.set("value", v)?;
        }
        self.dispatch_msg(msg)
    }

    /// Run a reconcile pass + sync text_input states without painting.
    /// Used so the router observes the latest description tree before
    /// the first render.
    fn ensure_reconciled(&mut self) -> Result<(), TuiError> {
        if self.reconciler.root.is_none() && self.lua.started() {
            let desc = self.lua.render_view()?;
            self.reconciler.reconcile(desc);
            if let Some(root) = self.reconciler.root.as_mut() {
                sync_text_inputs(root);
            }
        }
        Ok(())
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

    /// Dispatch a [`MouseMessage`]. Hit-tests against the most recently
    /// painted tree (uses each instance's `painted_rect`); the result is
    /// always bubbled to Lua as `{ kind, x, y, target_key, button, mods }`.
    /// Wheel events bubble verbatim — auto-scroll on `scrollable` lands
    /// in phase 5a.
    pub fn handle_mouse(&mut self, evt: MouseMessage) -> Result<(), TuiError> {
        let target_key = self
            .reconciler
            .root
            .as_ref()
            .and_then(|root| hit_test(root, evt.x, evt.y));

        let msg = self.lua().create_table()?;
        msg.set("kind", kind_string(evt.kind))?;
        msg.set("x", evt.x)?;
        msg.set("y", evt.y)?;
        match target_key {
            Some(k) => msg.set("target_key", k)?,
            None => msg.set("target_key", mlua::Value::Nil)?,
        }
        if let Some(b) = evt.button {
            msg.set("button", b)?;
        }
        let mods = self.lua().create_table()?;
        for (i, m) in evt.mods.iter().enumerate() {
            mods.set(i + 1, *m)?;
        }
        msg.set("mods", mods)?;
        self.dispatch_msg(msg)
    }

    /// Render if dirty. Returns the ANSI bytes; `None` means "no work".
    pub fn render_if_dirty(&mut self) -> Result<Option<Vec<u8>>, TuiError> {
        if !self.needs_render {
            return Ok(None);
        }
        let desc = self.lua.render_view()?;
        self.reconciler.reconcile(desc);
        let root = self.reconciler.root.as_mut().ok_or(TuiError::NotStarted)?;
        sync_text_inputs(root);
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

    const MOUSE_SCENARIO: &str = r#"
        tui.start {
          initial_state = { last_click = nil },
          view = function(s)
            local label = s.last_click and ("clicked: " .. s.last_click) or "no click"
            return tui.column { gap = 0, children = {
              tui.padding {
                value = 1,
                key   = "wrapper",
                child = tui.text { content = label, key = "label" },
              },
            }}
          end,
          update = function(msg, s)
            if msg.kind == "mouse.click" then
              return { last_click = msg.target_key or "<nil>" }, {}
            end
            return s, {}
          end,
        }
    "#;

    #[test]
    fn mouse_click_dispatches_target_key_to_lua() {
        use crate::mouse::{MouseKind, MouseMessage};
        let mut engine = Engine::new(20, 5).expect("engine");
        engine.load_scenario(MOUSE_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("render");
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Click,
                x: 2,
                y: 1,
                button: Some("left"),
                mods: vec![],
            })
            .expect("mouse");
        let bytes = engine.render_if_dirty().expect("render").expect("dirty");
        let s = String::from_utf8(bytes).expect("utf-8");
        // Hit-test reaches the deepest keyed instance under (2, 1) — the
        // text labelled "label", inside the keyed padding.
        assert!(
            s.contains("clicked: label"),
            "expected label hit, got:\n{s}"
        );
    }
}
