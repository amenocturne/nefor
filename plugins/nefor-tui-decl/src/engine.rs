//! Engine state machine — owns the Lua host, reconciler, and renderer.
//!
//! The binary's event loop drives this through `handle_key`,
//! `handle_resize`, and `render_if_dirty`. Integration tests drive the
//! same surface in-process (no spawned subprocess, no /dev/tty).

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::animation::sample as animation_sample;
use crate::desc::WidgetDescription;
use crate::error::TuiError;
use crate::input::KeyMessage;
use crate::input_router::{route_key, RouteDecision};
use crate::instance::{sync_text_inputs, InstanceKind, InstanceState, WidgetInstance};
use crate::lua_host::{LuaHost, SideEffect};
use crate::mouse::{hit_test, kind_string, MouseMessage};
use crate::reconciler::Reconciler;
use crate::render::Renderer;

thread_local! {
    /// Wall-clock value installed by the engine for the duration of a
    /// layout/paint pass. Used by the animation sampler to read "now"
    /// without threading the value through every layout call.
    static RENDER_TIME_MS: Cell<u64> = const { Cell::new(0) };
}

/// Read the current render-time-ms set by the engine. Returns `0`
/// outside of a layout/paint pass — animation primitives seen at that
/// point behave as if just mounted, which is harmless for unit tests
/// that bypass the engine.
pub fn current_render_time_ms() -> u64 {
    RENDER_TIME_MS.with(|c| c.get())
}

fn install_render_time_ms(now_ms: u64) {
    RENDER_TIME_MS.with(|c| c.set(now_ms));
}

pub struct Engine {
    lua: LuaHost,
    reconciler: Reconciler,
    renderer: Renderer,
    needs_render: bool,
    exit_requested: bool,
    /// Origin of the engine's monotonic clock. Each frame the engine
    /// computes `now_ms = (Instant::now() - clock_origin).as_millis()`
    /// and installs that into the thread-local before drawing. Tests
    /// can advance the clock with [`Engine::advance_time`] to step the
    /// animation sampler deterministically.
    clock_origin: Instant,
    /// Synthetic offset added to the wall-clock-derived `now_ms`. Tests
    /// use [`Engine::advance_time`] to bump this; production code never
    /// touches it.
    clock_offset_ms: u64,
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
            clock_origin: Instant::now(),
            clock_offset_ms: 0,
        })
    }

    /// Current frame-clock value in milliseconds. Combines the engine's
    /// monotonic origin with the synthetic offset that tests can bump.
    fn now_ms(&self) -> u64 {
        let real = self.clock_origin.elapsed().as_millis() as u64;
        real.saturating_add(self.clock_offset_ms)
    }

    /// Test-only API: advance the engine's frame clock by `delta`. The
    /// animation sampler reads from this clock, so test scenarios can
    /// step animations deterministically without sleeping.
    pub fn advance_time(&mut self, delta: Duration) {
        self.clock_offset_ms = self
            .clock_offset_ms
            .saturating_add(delta.as_millis() as u64);
        // Time advancing is a render trigger — animations may need a
        // fresh frame. The render loop is the actual cadence; here we
        // just mark dirty so the next `render_if_dirty` runs.
        self.needs_render = true;
    }

    /// `true` if any mounted animation has not yet completed. The main
    /// loop should keep ticking the renderer at frame rate while this
    /// returns `true` and stay idle otherwise.
    pub fn has_active_animations(&self) -> bool {
        let now = self.now_ms();
        match self.reconciler.root.as_ref() {
            Some(root) => any_active_animation(root, now),
            None => false,
        }
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
        let now = self.now_ms();
        install_render_time_ms(now);
        let desc = self.lua.render_view()?;
        self.reconciler.reconcile(desc);
        let root = self.reconciler.root.as_mut().ok_or(TuiError::NotStarted)?;
        sync_text_inputs(root);
        let bytes = self.renderer.render(root);
        self.needs_render = false;
        Ok(Some(bytes))
    }

    /// Force a render on the next call to `render_if_dirty`. Used by
    /// the main loop's animation tick to redraw at frame rate without
    /// any state change.
    pub fn mark_animation_tick(&mut self) {
        self.needs_render = true;
    }

    pub fn exit_requested(&self) -> bool {
        self.exit_requested
    }

    pub fn dimensions(&self) -> (u16, u16) {
        (self.renderer.width(), self.renderer.height())
    }
}

/// Walk the instance tree looking for at least one mounted animation
/// that has not yet completed. Used by [`Engine::has_active_animations`].
fn any_active_animation(inst: &WidgetInstance, now_ms: u64) -> bool {
    if matches!(inst.kind(), InstanceKind::Animation) {
        if let WidgetDescription::Animation {
            frames,
            duration_ms,
            iterations,
            direction,
            ..
        } = &inst.last_desc
        {
            if frames.is_empty() || *duration_ms == 0 {
                return false;
            }
            let mount = match &inst.state {
                InstanceState::Animation(s) => s.mount_time_ms.unwrap_or(now_ms),
                _ => return false,
            };
            let s = animation_sample(
                frames.len(),
                *duration_ms,
                *iterations,
                *direction,
                mount,
                now_ms,
            );
            if !s.completed {
                return true;
            }
        }
    }
    inst.children
        .iter()
        .any(|c| any_active_animation(c, now_ms))
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

    const ANIMATION_SCENARIO: &str = r#"
        tui.start {
          initial_state = {},
          view = function(_)
            return tui.animation {
              frames = { "a", "b", "c", "d" },
              duration_ms = 100,
            }
          end,
          update = function(_, s) return s, {} end,
        }
    "#;

    const STATIC_SCENARIO: &str = r#"
        tui.start {
          initial_state = {},
          view = function(_) return tui.text { content = "hello" } end,
          update = function(_, s) return s, {} end,
        }
    "#;

    #[test]
    fn has_active_animations_false_when_no_animation_in_tree() {
        let mut engine = Engine::new(20, 5).expect("engine");
        engine.load_scenario(STATIC_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("render");
        assert!(!engine.has_active_animations());
    }

    #[test]
    fn has_active_animations_true_when_infinite_animation_present() {
        let mut engine = Engine::new(20, 5).expect("engine");
        engine.load_scenario(ANIMATION_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("render");
        assert!(engine.has_active_animations());
    }

    #[test]
    fn advance_time_marks_dirty() {
        let mut engine = Engine::new(20, 5).expect("engine");
        engine.load_scenario(ANIMATION_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("first").expect("dirty");
        // Without state change, second render returns None.
        assert!(engine.render_if_dirty().expect("second").is_none());
        // After advancing time, the engine should mark itself dirty.
        engine.advance_time(Duration::from_millis(50));
        let r = engine.render_if_dirty().expect("third");
        assert!(r.is_some(), "advance_time should mark dirty");
    }

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
