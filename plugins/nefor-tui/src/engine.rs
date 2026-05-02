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
use crate::lua_host::{
    LuaHost, ScrollCommand, ScrollPositionMap, ScrollPositionSnapshot, SideEffect,
};
use crate::mouse::{
    find_scrollable_path, hit_test, instance_at_path as mouse_instance_at_path, kind_string,
    MouseKind, MouseMessage,
};
use crate::reconciler::Reconciler;
use crate::render::Renderer;
use crate::scrollable::{scroll_by_signed, WHEEL_STEP_ROWS};

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
        // Apply any scroll commands the update issued via tui.scroll_*.
        // Errors here propagate so a missing-key reference surfaces
        // immediately (per spec).
        let cmds = self.lua.take_scroll_commands();
        for cmd in cmds {
            self.apply_scroll_command(cmd)?;
        }
        self.needs_render = true;
        Ok(())
    }

    /// Apply one scroll command from Lua's queue against the live tree.
    /// Errors when the key doesn't resolve — silent no-op would mask
    /// config bugs (per spec § Lua scroll-control APIs).
    ///
    /// Fires `on_scroll` (if configured) on offset change so programmatic
    /// scrolling and wheel-driven scrolling behave identically from
    /// Lua's perspective.
    fn apply_scroll_command(&mut self, cmd: ScrollCommand) -> Result<(), TuiError> {
        let key = match &cmd {
            ScrollCommand::To { key, .. }
            | ScrollCommand::By { key, .. }
            | ScrollCommand::IntoView { key } => key.clone(),
        };
        let Some(root) = self.reconciler.root.as_mut() else {
            return Err(TuiError::InvalidDesc(format!(
                "tui.scroll_*: no rendered tree yet (key `{key}`)"
            )));
        };
        let path = match find_scrollable_by_key(root, &key) {
            Some(p) => p,
            None => {
                return Err(TuiError::InvalidDesc(format!(
                    "tui.scroll_*: no scrollable with key `{key}` in the current tree"
                )));
            }
        };
        let Some(target) = mouse_instance_at_path(root, &path) else {
            return Err(TuiError::InvalidDesc(format!(
                "tui.scroll_*: failed to walk path to scrollable `{key}`"
            )));
        };
        let on_scroll = match &target.last_desc {
            WidgetDescription::Scrollable { on_scroll, .. } => on_scroll.clone(),
            _ => None,
        };
        let st = match &mut target.state {
            InstanceState::Scrollable(s) => s,
            _ => {
                return Err(TuiError::InvalidDesc(format!(
                    "tui.scroll_*: instance for key `{key}` is not a scrollable"
                )));
            }
        };
        let prev = st.scroll_y;
        match cmd {
            ScrollCommand::To { offset, .. } => {
                let max = st.scroll_y_max();
                st.scroll_y = offset.min(max);
                // Mirror was_at_* so stick_to handling stays consistent
                // — a programmatic jump to the bottom should re-engage
                // stick_to=end.
                st.was_at_end = st.scroll_y == max;
                st.was_at_start = st.scroll_y == 0;
            }
            ScrollCommand::By { delta, .. } => {
                scroll_by_signed(st, delta);
                let max = st.scroll_y_max();
                st.was_at_end = st.scroll_y == max;
                st.was_at_start = st.scroll_y == 0;
            }
            ScrollCommand::IntoView { .. } => {
                // v1 minimal scope per spec: scroll to the bottom. Future
                // iterations may resolve a focused-target's location
                // within the scrollable.
                st.scroll_y = st.scroll_y_max();
                st.was_at_end = true;
                st.was_at_start = st.scroll_y == 0;
            }
        }
        let new_offset = st.scroll_y;
        if new_offset != prev {
            if let Some(kind) = on_scroll {
                let msg = self.lua().create_table()?;
                msg.set("kind", kind)?;
                msg.set("target_key", key)?;
                msg.set("offset", new_offset)?;
                // Recursive dispatch — this could in turn queue more
                // scroll commands, which `dispatch_msg` will drain. We
                // limit recursion depth indirectly: if Lua keeps
                // scrolling on every notification we'll grow the stack;
                // a malicious config can't trip an infinite loop unless
                // it keeps changing scroll_y, which terminates at the
                // clamp boundary.
                self.dispatch_msg(msg)?;
            }
        }
        Ok(())
    }

    /// Walk the current tree and update the Lua-visible scroll-position
    /// snapshot map. Called after every render so `tui.scroll_position`
    /// returns the freshest geometry.
    fn refresh_scroll_positions(&self) {
        let mut snap = ScrollPositionMap::new();
        if let Some(root) = self.reconciler.root.as_ref() {
            collect_scroll_positions(root, &mut snap);
        }
        self.lua.write_scroll_positions(snap);
    }

    /// Apply a terminal resize. Forces a full redraw on the next render.
    pub fn handle_resize(&mut self, width: u16, height: u16) -> Result<(), TuiError> {
        self.renderer.resize(width, height);
        self.needs_render = true;
        Ok(())
    }

    /// Dispatch a [`MouseMessage`]. Browser-style routing:
    ///
    /// - **Wheel + scrollable under cursor** → engine auto-scrolls the
    ///   deepest enclosing `scrollable` and does NOT bubble the event.
    ///   The scroll is `WHEEL_STEP_ROWS` per notch, matching the legacy
    ///   chat plugin's convention.
    /// - **Wheel + no scrollable** → bubble `mouse.wheel` to Lua so the
    ///   user can wire whatever fallback they want (e.g. carousel).
    /// - **Clicks** → always bubble, with the deepest keyed instance's
    ///   user_key as `target_key` (hit-test result).
    pub fn handle_mouse(&mut self, evt: MouseMessage) -> Result<(), TuiError> {
        // Wheel auto-scroll: try to absorb the event by mutating a
        // scrollable's state. If none is under the cursor, fall through
        // to the bubble path so Lua sees the wheel event verbatim.
        if matches!(evt.kind, MouseKind::Wheel) {
            let absorbed = self.try_wheel_scroll(&evt)?;
            if let Some(notify) = absorbed {
                if let Some((kind, target_key, offset)) = notify {
                    // The scrollable absorbed the wheel and configured
                    // an `on_scroll` callback — emit it so Lua observes
                    // the new offset. Same `dispatch_msg` path as any
                    // other engine-originated message.
                    let msg = self.lua().create_table()?;
                    msg.set("kind", kind)?;
                    msg.set("target_key", target_key)?;
                    msg.set("offset", offset)?;
                    self.dispatch_msg(msg)?;
                }
                self.needs_render = true;
                return Ok(());
            }
        }

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

    /// Attempt to absorb a wheel event into a scrollable under the
    /// cursor. Returns:
    ///
    /// - `Ok(None)` — no scrollable under cursor; caller should bubble.
    /// - `Ok(Some(None))` — scrolled silently (no `on_scroll` configured
    ///   or scroll_y didn't change).
    /// - `Ok(Some(Some((kind, target_key, offset))))` — scrolled and
    ///   `on_scroll` should be dispatched.
    ///
    /// Decoupled from `handle_mouse` so the borrow split (mutable
    /// scrollable state vs. immutable Lua VM access) stays local — the
    /// outer dispatch path can then use `self.lua()` after the mutation
    /// scope closes.
    #[allow(clippy::type_complexity)]
    fn try_wheel_scroll(
        &mut self,
        evt: &MouseMessage,
    ) -> Result<Option<Option<(String, String, u16)>>, TuiError> {
        let delta_rows: i32 = match evt.button {
            Some("up") => -(WHEEL_STEP_ROWS as i32),
            Some("down") => WHEEL_STEP_ROWS as i32,
            _ => return Ok(None),
        };
        let Some(root) = self.reconciler.root.as_mut() else {
            return Ok(None);
        };
        let Some(path) = find_scrollable_path(root, evt.x, evt.y) else {
            return Ok(None);
        };
        let Some(target) = mouse_instance_at_path(root, &path) else {
            return Ok(None);
        };
        let target_key = target.last_desc.user_key().map(|s| s.to_string());
        let on_scroll = match &target.last_desc {
            WidgetDescription::Scrollable { on_scroll, .. } => on_scroll.clone(),
            _ => None,
        };
        let new_offset = match &mut target.state {
            InstanceState::Scrollable(s) => {
                let prev = s.scroll_y;
                scroll_by_signed(s, delta_rows);
                if s.scroll_y == prev {
                    None
                } else {
                    Some(s.scroll_y)
                }
            }
            _ => None,
        };
        let notify = match (on_scroll, target_key, new_offset) {
            (Some(kind), Some(key), Some(offset)) => Some((kind, key, offset)),
            _ => None,
        };
        Ok(Some(notify))
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
        // Snapshot geometry post-paint so `tui.scroll_position` is up
        // to date on the next Lua call.
        self.refresh_scroll_positions();
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

/// Locate a scrollable by user_key in the instance tree. Returns the
/// path (sequence of child indices from the root) or `None` if no
/// matching scrollable exists. Used by `apply_scroll_command` so a
/// Lua-side `tui.scroll_to(key, ...)` resolves the right instance.
fn find_scrollable_by_key(root: &WidgetInstance, key: &str) -> Option<Vec<usize>> {
    let mut path: Vec<usize> = Vec::new();
    let mut found: Option<Vec<usize>> = None;
    walk_scrollable_by_key(root, key, &mut path, &mut found);
    found
}

fn walk_scrollable_by_key(
    inst: &WidgetInstance,
    key: &str,
    path: &mut Vec<usize>,
    out: &mut Option<Vec<usize>>,
) {
    if matches!(inst.kind(), InstanceKind::Scrollable) {
        if let Some(k) = inst.last_desc.user_key() {
            if k == key {
                *out = Some(path.clone());
                return;
            }
        }
    }
    for (i, child) in inst.children.iter().enumerate() {
        if out.is_some() {
            return;
        }
        path.push(i);
        walk_scrollable_by_key(child, key, path, out);
        path.pop();
    }
}

/// Walk the tree and accumulate every scrollable's key + geometry into
/// the snapshot map. Skips scrollables without a user_key (the desc
/// parser already rejects keyless scrollables, but the walk stays defensive).
fn collect_scroll_positions(inst: &WidgetInstance, out: &mut ScrollPositionMap) {
    if matches!(inst.kind(), InstanceKind::Scrollable) {
        if let (Some(k), InstanceState::Scrollable(s)) = (inst.last_desc.user_key(), &inst.state) {
            out.insert(
                k.to_string(),
                ScrollPositionSnapshot {
                    offset: s.scroll_y,
                    max: s.scroll_y_max(),
                    viewport_size: s.viewport_height,
                },
            );
        }
    }
    for c in inst.children.iter() {
        collect_scroll_positions(c, out);
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

    const SCROLL_SCENARIO: &str = r#"
        tui.start {
          initial_state = { wheels = 0 },
          view = function(s)
            local kids = {}
            for i = 1, 30 do
              kids[#kids + 1] = tui.text { content = "row " .. i }
            end
            -- text first so the column gives it its natural 1-row height,
            -- then `expanded` hands the rest of the rows to the scrollable.
            return tui.column { gap = 0, children = {
              tui.text { content = "wheels: " .. tostring(s.wheels) },
              tui.expanded {
                child = tui.scrollable {
                  key       = "transcript",
                  child     = tui.column { gap = 0, children = kids },
                  scrollbar = "auto",
                },
              },
            }}
          end,
          update = function(msg, s)
            if msg.kind == "mouse.wheel" then
              return { wheels = s.wheels + 1 }, {}
            end
            return s, {}
          end,
        }
    "#;

    #[test]
    fn wheel_inside_scrollable_scrolls_and_does_not_bubble() {
        use crate::mouse::{MouseKind, MouseMessage};
        let mut engine = Engine::new(20, 6).expect("engine");
        engine.load_scenario(SCROLL_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("render");

        // Cursor at y=2 lands inside the scrollable (rows 1..6).
        // Wheel-down should scroll the scrollable, not bubble. So
        // `wheels` stays at 0.
        for _ in 0..3 {
            engine
                .handle_mouse(MouseMessage {
                    kind: MouseKind::Wheel,
                    x: 1,
                    y: 2,
                    button: Some("down"),
                    mods: vec![],
                })
                .expect("wheel");
        }
        let _ = engine.render_if_dirty().expect("render");
        // Verify the wheel events did NOT bubble: the Lua state's
        // `wheels` counter would have to advance for that to happen,
        // which would in turn change row 0's text. Re-render the tree
        // by querying it directly for the current frame state.
        let root = engine.reconciler.root.as_ref().expect("root");
        // Tree: column { text, expanded { scrollable } }.
        // Verify the leading text still says "wheels: 0".
        let leading_text_desc = &root.children[0].last_desc;
        match leading_text_desc {
            WidgetDescription::Text { content, .. } => {
                assert_eq!(
                    content, "wheels: 0",
                    "wheel inside scrollable must not bubble"
                );
            }
            _ => panic!("expected leading text in column children[0]"),
        }
        // And the scroll position advanced. WHEEL_STEP_ROWS = 3, three
        // notches → 9 rows.
        let scroll_inst = &root.children[1].children[0];
        match &scroll_inst.state {
            InstanceState::Scrollable(s) => assert_eq!(s.scroll_y, 9),
            _ => panic!("expected scrollable state"),
        }
    }

    #[test]
    fn wheel_outside_scrollable_bubbles_to_lua() {
        use crate::mouse::{MouseKind, MouseMessage};
        let mut engine = Engine::new(20, 6).expect("engine");
        engine.load_scenario(SCROLL_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("render");

        // The leading text sits at row 0; the scrollable occupies rows
        // 1..6. Wheel at y=0 lands on the text — outside the
        // scrollable's painted rect — so the event bubbles.
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Wheel,
                x: 1,
                y: 0,
                button: Some("up"),
                mods: vec![],
            })
            .expect("wheel");
        let _ = engine.render_if_dirty().expect("render");
        // The wheel bubbled — Lua's `update` advanced `wheels` to 1, so
        // a re-render of `view` produces a Text desc with the new content.
        let root = engine.reconciler.root.as_ref().expect("root");
        let leading_text_desc = &root.children[0].last_desc;
        match leading_text_desc {
            WidgetDescription::Text { content, .. } => {
                assert_eq!(
                    content, "wheels: 1",
                    "wheel outside scrollable should bubble"
                );
            }
            _ => panic!("expected leading text in column children[0]"),
        }
    }

    const SCROLL_API_SCENARIO: &str = r#"
        tui.start {
          initial_state = { last_offset = 0 },
          view = function(s)
            local kids = {}
            for i = 1, 30 do
              kids[#kids + 1] = tui.text { content = "row " .. i }
            end
            return tui.column { gap = 0, children = {
              tui.text { content = "offset: " .. tostring(s.last_offset) },
              tui.expanded {
                child = tui.scrollable {
                  key   = "log",
                  child = tui.column { gap = 0, children = kids },
                },
              },
            }}
          end,
          update = function(msg, s)
            if msg.kind == "scroll.to" then
              tui.scroll_to("log", msg.offset)
              return s, {}
            elseif msg.kind == "scroll.by" then
              tui.scroll_by("log", msg.delta)
              return s, {}
            elseif msg.kind == "scroll.into_view" then
              tui.scroll_into_view("log")
              return s, {}
            elseif msg.kind == "scroll.read" then
              local p = tui.scroll_position("log")
              return { last_offset = p.offset }, {}
            end
            return s, {}
          end,
        }
    "#;

    fn dispatch_kind(engine: &mut Engine, kind: &str) {
        let msg = engine.lua().create_table().unwrap();
        msg.set("kind", kind).unwrap();
        engine.dispatch_msg(msg).unwrap();
    }

    fn dispatch_kind_with(engine: &mut Engine, kind: &str, field: &str, val: i64) {
        let msg = engine.lua().create_table().unwrap();
        msg.set("kind", kind).unwrap();
        msg.set(field, val).unwrap();
        engine.dispatch_msg(msg).unwrap();
    }

    #[test]
    fn lua_scroll_to_updates_scroll_state() {
        let mut engine = Engine::new(20, 6).expect("engine");
        engine.load_scenario(SCROLL_API_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("render");

        dispatch_kind_with(&mut engine, "scroll.to", "offset", 12);
        // After dispatch_msg, the scroll command was processed.
        let root = engine.reconciler.root.as_ref().expect("root");
        let scroll_inst = &root.children[1].children[0];
        match &scroll_inst.state {
            InstanceState::Scrollable(s) => assert_eq!(s.scroll_y, 12),
            _ => panic!("expected scrollable state"),
        }
    }

    #[test]
    fn lua_scroll_by_applies_relative_delta() {
        let mut engine = Engine::new(20, 6).expect("engine");
        engine.load_scenario(SCROLL_API_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("render");

        dispatch_kind_with(&mut engine, "scroll.by", "delta", 7);
        let root = engine.reconciler.root.as_ref().expect("root");
        let scroll_inst = &root.children[1].children[0];
        match &scroll_inst.state {
            InstanceState::Scrollable(s) => assert_eq!(s.scroll_y, 7),
            _ => panic!("expected scrollable state"),
        }

        // Negative delta back toward top.
        dispatch_kind_with(&mut engine, "scroll.by", "delta", -3);
        let root = engine.reconciler.root.as_ref().expect("root");
        let scroll_inst = &root.children[1].children[0];
        match &scroll_inst.state {
            InstanceState::Scrollable(s) => assert_eq!(s.scroll_y, 4),
            _ => panic!("expected scrollable state"),
        }
    }

    #[test]
    fn lua_scroll_into_view_jumps_to_bottom() {
        let mut engine = Engine::new(20, 6).expect("engine");
        engine.load_scenario(SCROLL_API_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("render");

        dispatch_kind(&mut engine, "scroll.into_view");
        let root = engine.reconciler.root.as_ref().expect("root");
        let scroll_inst = &root.children[1].children[0];
        match &scroll_inst.state {
            InstanceState::Scrollable(s) => assert_eq!(s.scroll_y, s.scroll_y_max()),
            _ => panic!("expected scrollable state"),
        }
    }

    #[test]
    fn lua_scroll_position_round_trips_offset_to_state() {
        let mut engine = Engine::new(20, 6).expect("engine");
        engine.load_scenario(SCROLL_API_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("render");

        // Scroll to 8, render to settle, then ask Lua for the position.
        dispatch_kind_with(&mut engine, "scroll.to", "offset", 8);
        let _ = engine.render_if_dirty().expect("render");
        dispatch_kind(&mut engine, "scroll.read");
        let _ = engine.render_if_dirty().expect("render");
        // After scroll.read, Lua's update stored last_offset = 8.
        let root = engine.reconciler.root.as_ref().expect("root");
        // `column { text("offset: ..."), expanded { scrollable } }`
        let leading_text = &root.children[0].last_desc;
        match leading_text {
            WidgetDescription::Text { content, .. } => {
                assert_eq!(
                    content, "offset: 8",
                    "tui.scroll_position should return live offset"
                );
            }
            _ => panic!("expected text in children[0]"),
        }
    }

    const STICK_END_SCENARIO: &str = r#"
        tui.start {
          initial_state = { rows = 5, scroll_events = 0 },
          view = function(s)
            local kids = {}
            for i = 1, s.rows do
              kids[#kids + 1] = tui.text { content = "row " .. i }
            end
            return tui.column { gap = 0, children = {
              tui.text { content = "events: " .. tostring(s.scroll_events) },
              tui.expanded {
                child = tui.scrollable {
                  key       = "transcript",
                  child     = tui.column { gap = 0, children = kids },
                  stick_to  = "end",
                  on_scroll = "log.scrolled",
                },
              },
            }}
          end,
          update = function(msg, s)
            if msg.kind == "grow" then
              return { rows = s.rows + msg.delta, scroll_events = s.scroll_events }, {}
            elseif msg.kind == "log.scrolled" then
              return { rows = s.rows, scroll_events = s.scroll_events + 1 }, {}
            end
            return s, {}
          end,
        }
    "#;

    #[test]
    fn stick_to_end_engine_pins_to_bottom_through_growth() {
        let mut engine = Engine::new(20, 6).expect("engine");
        engine.load_scenario(STICK_END_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("render");
        // Initial: 5 rows in a 5-row viewport — fits, scroll_y_max == 0.
        // Grow content to 30 rows.
        dispatch_kind_with(&mut engine, "grow", "delta", 25);
        let _ = engine.render_if_dirty().expect("render");
        let root = engine.reconciler.root.as_ref().expect("root");
        let scroll_inst = &root.children[1].children[0];
        match &scroll_inst.state {
            InstanceState::Scrollable(s) => {
                assert_eq!(
                    s.scroll_y,
                    s.scroll_y_max(),
                    "stick_to = end pins after growth"
                );
                assert_eq!(s.scroll_y, 25);
            }
            _ => panic!("expected scrollable state"),
        }
    }

    #[test]
    fn wheel_fires_on_scroll_callback_when_configured() {
        let mut engine = Engine::new(20, 6).expect("engine");
        engine.load_scenario(STICK_END_SCENARIO).expect("load");
        // Grow content first so wheel can move scroll_y.
        dispatch_kind_with(&mut engine, "grow", "delta", 25);
        let _ = engine.render_if_dirty().expect("render");

        // Wheel up moves us off the bottom — on_scroll fires once.
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Wheel,
                x: 1,
                y: 2,
                button: Some("up"),
                mods: vec![],
            })
            .expect("wheel");
        let _ = engine.render_if_dirty().expect("render");
        let root = engine.reconciler.root.as_ref().expect("root");
        let leading_text_desc = &root.children[0].last_desc;
        match leading_text_desc {
            WidgetDescription::Text { content, .. } => {
                assert_eq!(
                    content, "events: 1",
                    "on_scroll should fire on wheel-induced scroll"
                );
            }
            _ => panic!("expected leading text"),
        }
    }

    #[test]
    fn lua_scroll_to_unknown_key_errors() {
        const BAD_KEY: &str = r#"
            tui.start {
              initial_state = {},
              view = function(_) return tui.text { content = "x" } end,
              update = function(msg, s)
                if msg.kind == "go" then
                  tui.scroll_to("nope", 1)
                end
                return s, {}
              end,
            }
        "#;
        let mut engine = Engine::new(20, 6).expect("engine");
        engine.load_scenario(BAD_KEY).expect("load");
        let _ = engine.render_if_dirty().expect("render");
        let msg = engine.lua().create_table().unwrap();
        msg.set("kind", "go").unwrap();
        let err = engine.dispatch_msg(msg).unwrap_err();
        assert!(
            format!("{err}").contains("no scrollable with key `nope`"),
            "expected missing-key error, got: {err}"
        );
    }

    #[test]
    fn wheel_at_top_clamps_at_zero() {
        use crate::mouse::{MouseKind, MouseMessage};
        let mut engine = Engine::new(20, 6).expect("engine");
        engine.load_scenario(SCROLL_SCENARIO).expect("load");
        let _ = engine.render_if_dirty().expect("render");

        // Already at top — wheel-up should clamp at 0 (no underflow).
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Wheel,
                x: 1,
                y: 2,
                button: Some("up"),
                mods: vec![],
            })
            .expect("wheel");
        let root = engine.reconciler.root.as_ref().expect("root");
        let scroll_inst = &root.children[1].children[0];
        match &scroll_inst.state {
            InstanceState::Scrollable(s) => assert_eq!(s.scroll_y, 0),
            _ => panic!("expected scrollable state"),
        }
    }
}
