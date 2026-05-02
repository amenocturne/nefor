//! Lua VM that hosts the user's `tui.start { initial_state, view, update }`.
//!
//! Phase 1 surface:
//! - `tui.text   { content, key?, style?, wrap? }`     → tagged Lua table
//! - `tui.column { children, gap?, key? }`             → tagged Lua table
//! - `tui.padding { value | {top,right,bottom,left}, child, key? }`
//! - `tui.start  { initial_state, view, update }`      → registers handlers
//!
//! View / update / state are kept in the Lua registry. The Rust engine
//! pulls the registry-stored values for each render and dispatch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mlua::{Lua, LuaSerdeExt, RegistryKey, Table, Value};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::desc::{from_lua_table, WidgetDescription, KIND_FIELD};
use crate::error::TuiError;

/// One queued scroll command produced by a Lua call to `tui.scroll_to /
/// scroll_by / scroll_into_view`. The engine drains the queue after each
/// `dispatch` call and applies the commands to the reconciled tree.
///
/// Decoupling this through a queue keeps Lua's `update` pure — the side
/// effects of scroll changes are visible only after `dispatch` returns,
/// matching the `side_effects` model used elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScrollCommand {
    /// Set absolute offset.
    To { key: String, offset: u16 },
    /// Apply a relative delta (positive = down).
    By { key: String, delta: i32 },
    /// Move the named scrollable's content so the focused/cursor target
    /// is visible. v1 minimal scope: scroll to the bottom (matches the
    /// chat-transcript "show me the latest" intent).
    IntoView { key: String },
}

/// Snapshot of every scrollable's current geometry, keyed by user_key.
/// The engine refreshes this map after every render so `tui.scroll_position`
/// reads reflect the most recently painted frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrollPositionSnapshot {
    pub offset: u16,
    pub max: u16,
    pub viewport_size: u16,
}

pub type ScrollPositionMap = HashMap<String, ScrollPositionSnapshot>;

#[derive(Default)]
struct StartedState {
    state_key: Option<RegistryKey>,
    view_key: Option<RegistryKey>,
    update_key: Option<RegistryKey>,
}

/// Side-effect record returned from `update` (or queued via the
/// imperative `tui.emit` helper). Phase 6 adds NCP egress (`Emit`); the
/// engine drains the list after every `dispatch` and acts on each entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideEffect {
    /// Exit the run loop on the next `render_if_dirty` call.
    Exit,
    /// Emit an NCP `event`-shaped envelope from this plugin. The body
    /// MUST contain a `kind` string per the protocol; the engine wraps
    /// it as a `PluginOutgoing::event(body)` and writes it to stdout.
    /// `target_hint` is documentation-only at the plugin layer — actual
    /// per-peer delivery is the engine's broker / Lua-side starter
    /// transforms job (see `starter/ncp.lua`'s prefix-targeting).
    Emit {
        target_hint: Option<String>,
        body: JsonMap<String, JsonValue>,
    },
}

pub struct LuaHost {
    lua: Lua,
    started: Arc<Mutex<StartedState>>,
    /// Queue of scroll commands produced by the Lua API surface. Drained
    /// by the engine after every `dispatch` so the side effects show up
    /// on the next render.
    scroll_queue: Arc<Mutex<Vec<ScrollCommand>>>,
    /// Snapshot of every scrollable's current geometry. Engine writes;
    /// Lua reads via `tui.scroll_position(key)`.
    scroll_positions: Arc<Mutex<ScrollPositionMap>>,
    /// Queue of NCP `event`-shaped envelopes that Lua produced via the
    /// imperative `tui.emit` API. Drained by the engine after every
    /// `dispatch` and written to stdout. Side-effects returned from
    /// `update` are layered on top — the engine merges both before
    /// flushing to the writer.
    emit_queue: Arc<Mutex<Vec<SideEffect>>>,
}

impl LuaHost {
    /// Create the VM and install the `tui` global.
    pub fn new() -> Result<Self, TuiError> {
        let lua = Lua::new();
        let started = Arc::new(Mutex::new(StartedState::default()));
        let scroll_queue: Arc<Mutex<Vec<ScrollCommand>>> = Arc::new(Mutex::new(Vec::new()));
        let scroll_positions: Arc<Mutex<ScrollPositionMap>> =
            Arc::new(Mutex::new(ScrollPositionMap::new()));
        let emit_queue: Arc<Mutex<Vec<SideEffect>>> = Arc::new(Mutex::new(Vec::new()));
        install_tui(
            &lua,
            Arc::clone(&started),
            Arc::clone(&scroll_queue),
            Arc::clone(&scroll_positions),
            Arc::clone(&emit_queue),
        )?;
        Ok(LuaHost {
            lua,
            started,
            scroll_queue,
            scroll_positions,
            emit_queue,
        })
    }

    /// Drain queued NCP egress so the engine can flush them to stdout.
    /// Returns the list in submission order.
    pub fn take_emit_queue(&self) -> Vec<SideEffect> {
        let mut q = lock(&self.emit_queue);
        std::mem::take(&mut *q)
    }

    /// Drain any scroll commands that Lua queued during the most recent
    /// `dispatch`. The engine processes the returned list against its
    /// reconciled tree.
    pub fn take_scroll_commands(&self) -> Vec<ScrollCommand> {
        let mut q = lock(&self.scroll_queue);
        std::mem::take(&mut *q)
    }

    /// Snapshot the engine's most recently observed scroll positions
    /// into the shared map so `tui.scroll_position` returns up-to-date
    /// data on the next Lua call.
    pub fn write_scroll_positions(&self, snapshot: ScrollPositionMap) {
        let mut p = lock(&self.scroll_positions);
        *p = snapshot;
    }

    /// Borrow the underlying VM. Useful for integration tests that load
    /// scenarios directly.
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Whether `tui.start` has been called.
    pub fn started(&self) -> bool {
        let s = lock(&self.started);
        s.view_key.is_some() && s.update_key.is_some() && s.state_key.is_some()
    }

    /// Load a Lua source string. Use this to bootstrap a scenario in
    /// tests; the source is expected to call `tui.start { ... }` exactly
    /// once.
    pub fn load_source(&self, name: &str, source: &str) -> Result<(), TuiError> {
        self.lua.load(source).set_name(name).exec()?;
        if !self.started() {
            return Err(TuiError::NotStarted);
        }
        Ok(())
    }

    /// Run `view(state)` and convert the returned table to a
    /// [`WidgetDescription`].
    pub fn render_view(&self) -> Result<WidgetDescription, TuiError> {
        let (view, state) = {
            let started = lock(&self.started);
            let view_key = started.view_key.as_ref().ok_or(TuiError::NotStarted)?;
            let state_key = started.state_key.as_ref().ok_or(TuiError::NotStarted)?;
            let view: mlua::Function = self.lua.registry_value(view_key)?;
            let state: Value = self.lua.registry_value(state_key)?;
            (view, state)
        };
        let returned: Value = view.call(state)?;
        let table = match returned {
            Value::Table(t) => t,
            other => {
                return Err(TuiError::InvalidDesc(format!(
                    "view must return a widget table (got {})",
                    other.type_name()
                )));
            }
        };
        from_lua_table(&table)
    }

    /// Convert an NCP event body (`{ kind = "...", ... }`, JSON-shaped)
    /// to a Lua table suitable for [`Self::dispatch`]. The conversion
    /// goes through serde so nested objects/arrays / numbers / strings
    /// land as native Lua types.
    pub fn body_to_msg_table(&self, body: &JsonMap<String, JsonValue>) -> Result<Table, TuiError> {
        let value = JsonValue::Object(body.clone());
        let lua_val: Value = self.lua.to_value(&value)?;
        let Value::Table(t) = lua_val else {
            return Err(TuiError::InvalidDesc(
                "internal: body_to_msg_table: top-level NCP body must be a JSON object".into(),
            ));
        };
        Ok(t)
    }

    /// Dispatch a message through `update(msg, state)`. Returns the side-
    /// effect list (Exit / Emit). Unknown kinds are tracing-warned and
    /// dropped.
    pub fn dispatch(&self, msg: Table) -> Result<Vec<SideEffect>, TuiError> {
        let (update, state) = {
            let started = lock(&self.started);
            let update_key = started.update_key.as_ref().ok_or(TuiError::NotStarted)?;
            let state_key = started.state_key.as_ref().ok_or(TuiError::NotStarted)?;
            let update: mlua::Function = self.lua.registry_value(update_key)?;
            let state: Value = self.lua.registry_value(state_key)?;
            (update, state)
        };
        let result: mlua::MultiValue = update.call((msg, state))?;
        let mut iter = result.into_iter();
        let new_state = iter.next().unwrap_or(Value::Nil);
        let effects_val = iter.next().unwrap_or(Value::Nil);

        // Replace stored state.
        let new_state_key = self.lua.create_registry_value(new_state)?;
        {
            let mut started = lock(&self.started);
            if let Some(old) = started.state_key.take() {
                self.lua.remove_registry_value(old)?;
            }
            started.state_key = Some(new_state_key);
        }

        Ok(parse_side_effects(&self.lua, effects_val))
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| {
        tracing::error!("nefor-tui: mutex poisoned; recovering for best-effort progress");
        poisoned.into_inner()
    })
}

fn parse_side_effects(lua: &Lua, v: Value) -> Vec<SideEffect> {
    let table = match v {
        Value::Nil => return Vec::new(),
        Value::Table(t) => t,
        other => {
            tracing::warn!(
                got = other.type_name(),
                "update returned non-table side_effects; ignoring"
            );
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    let len = table.raw_len();
    for i in 1..=len {
        let entry: Value = match table.get(i) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, index = i, "failed to read side-effect entry");
                continue;
            }
        };
        let entry_t = match entry {
            Value::Table(t) => t,
            other => {
                tracing::warn!(
                    got = other.type_name(),
                    "side-effect must be a table; skipping"
                );
                continue;
            }
        };
        let kind: Option<String> = match entry_t.get::<Value>("kind") {
            Ok(Value::String(s)) => s.to_str().ok().map(|c| c.to_string()),
            _ => None,
        };
        match kind.as_deref() {
            Some("exit") => out.push(SideEffect::Exit),
            // `send_to` / `emit` — both shapes are accepted, both produce
            // a `SideEffect::Emit`. The body is converted from Lua to
            // serde_json::Value via mlua's serde bridge.
            Some("send_to") | Some("emit") => match parse_emit_entry(lua, &entry_t) {
                Ok(eff) => out.push(eff),
                Err(e) => tracing::warn!(error = %e, "failed to parse emit/send_to side-effect"),
            },
            Some(other) => {
                tracing::warn!(kind = other, "unknown side-effect kind; ignoring");
            }
            None => {
                tracing::warn!("side-effect missing `kind`; ignoring");
            }
        }
    }
    out
}

/// Convert a `{ kind = "send_to" | "emit", target?: string, body: table }`
/// Lua table into a [`SideEffect::Emit`]. Errors propagate so the caller
/// can log a precise reason.
fn parse_emit_entry(lua: &Lua, entry: &Table) -> mlua::Result<SideEffect> {
    let target_hint: Option<String> = match entry.get::<Value>("target") {
        Ok(Value::String(s)) => s.to_str().ok().map(|c| c.to_string()),
        // Backwards-compat: chat.lua may use `plugin = "..."`.
        _ => match entry.get::<Value>("plugin") {
            Ok(Value::String(s)) => s.to_str().ok().map(|c| c.to_string()),
            _ => None,
        },
    };
    let body_val: Value = entry.get("body")?;
    let body_json: JsonValue = lua.from_value(body_val)?;
    let JsonValue::Object(map) = body_json else {
        return Err(mlua::Error::runtime(
            "send_to/emit: `body` must be a JSON object (Lua table with string keys)",
        ));
    };
    Ok(SideEffect::Emit {
        target_hint,
        body: map,
    })
}

fn install_tui(
    lua: &Lua,
    started: Arc<Mutex<StartedState>>,
    scroll_queue: Arc<Mutex<Vec<ScrollCommand>>>,
    scroll_positions: Arc<Mutex<ScrollPositionMap>>,
    emit_queue: Arc<Mutex<Vec<SideEffect>>>,
) -> Result<(), TuiError> {
    let tui = lua.create_table()?;

    // tui.text { content, key?, style?, wrap? }
    let text_fn = lua.create_function(|lua, args: Table| {
        args.set(KIND_FIELD, "text")?;
        let _ = lua;
        Ok(args)
    })?;
    tui.set("text", text_fn)?;

    // tui.spans { spans = { { text=, fg=, bg=, bold=, italic=, ... } }, wrap?, key? }
    let spans_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "spans")?;
        Ok(args)
    })?;
    tui.set("spans", spans_fn)?;

    // tui.markdown { source, theme?, wrap?, key? }
    let markdown_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "markdown")?;
        Ok(args)
    })?;
    tui.set("markdown", markdown_fn)?;

    // tui.animation { frames, duration_ms, iterations?, direction?, key? }
    let animation_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "animation")?;
        Ok(args)
    })?;
    tui.set("animation", animation_fn)?;

    // tui.column { children, gap?, key? }
    let column_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "column")?;
        Ok(args)
    })?;
    tui.set("column", column_fn)?;

    // tui.row { children, gap?, key? }
    let row_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "row")?;
        Ok(args)
    })?;
    tui.set("row", row_fn)?;

    // tui.padding { value | {top,right,bottom,left}, child, key? }
    let padding_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "padding")?;
        Ok(args)
    })?;
    tui.set("padding", padding_fn)?;

    // tui.stack { children, key? }
    let stack_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "stack")?;
        Ok(args)
    })?;
    tui.set("stack", stack_fn)?;

    // tui.expanded { flex?, child, key? }
    let expanded_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "expanded")?;
        Ok(args)
    })?;
    tui.set("expanded", expanded_fn)?;

    // tui.spacer { flex?, key? }
    let spacer_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "spacer")?;
        Ok(args)
    })?;
    tui.set("spacer", spacer_fn)?;

    // tui.constrained { min_width?, max_width?, min_height?, max_height?, child, key? }
    let constrained_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "constrained")?;
        Ok(args)
    })?;
    tui.set("constrained", constrained_fn)?;

    // tui.align { alignment?, child, key? }
    let align_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "align")?;
        Ok(args)
    })?;
    tui.set("align", align_fn)?;

    // tui.anchored { anchor?, offset_x?, offset_y?, width?, height?, child, key? }
    let anchored_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "anchored")?;
        Ok(args)
    })?;
    tui.set("anchored", anchored_fn)?;

    // tui.text_input { key, value?, focused?, on_change?, on_submit?,
    //                  min_lines?, max_lines?, placeholder?, cursor_blink?,
    //                  style? }
    let text_input_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "text_input")?;
        Ok(args)
    })?;
    tui.set("text_input", text_input_fn)?;

    // tui.scrollable { key, child, stick_to?, on_scroll?, scrollbar?, style? }
    let scrollable_fn = lua.create_function(|_, args: Table| {
        args.set(KIND_FIELD, "scrollable")?;
        Ok(args)
    })?;
    tui.set("scrollable", scrollable_fn)?;

    // ── Scroll-control APIs ──────────────────────────────────────────
    //
    // All four helpers funnel through the shared `scroll_queue` /
    // `scroll_positions` channels — Lua never holds a widget instance.
    // Missing keys raise a Lua error so config bugs surface immediately
    // (per spec: "if the key doesn't resolve, error to Lua — surfaces
    // config bugs early").

    let queue_for_to = Arc::clone(&scroll_queue);
    let scroll_to_fn =
        lua.create_function(move |_, (key, offset): (String, i64)| -> mlua::Result<()> {
            if !(0..=u16::MAX as i64).contains(&offset) {
                return Err(mlua::Error::runtime(format!(
                    "tui.scroll_to: `offset` must be in 0..=65535 (got {offset})"
                )));
            }
            lock(&queue_for_to).push(ScrollCommand::To {
                key,
                offset: offset as u16,
            });
            Ok(())
        })?;
    tui.set("scroll_to", scroll_to_fn)?;

    let queue_for_by = Arc::clone(&scroll_queue);
    let scroll_by_fn =
        lua.create_function(move |_, (key, delta): (String, i64)| -> mlua::Result<()> {
            if !(i32::MIN as i64..=i32::MAX as i64).contains(&delta) {
                return Err(mlua::Error::runtime(format!(
                    "tui.scroll_by: `delta` must fit in i32 (got {delta})"
                )));
            }
            lock(&queue_for_by).push(ScrollCommand::By {
                key,
                delta: delta as i32,
            });
            Ok(())
        })?;
    tui.set("scroll_by", scroll_by_fn)?;

    let queue_for_into = Arc::clone(&scroll_queue);
    let scroll_into_view_fn = lua.create_function(move |_, key: String| -> mlua::Result<()> {
        lock(&queue_for_into).push(ScrollCommand::IntoView { key });
        Ok(())
    })?;
    tui.set("scroll_into_view", scroll_into_view_fn)?;

    let positions_for_read = Arc::clone(&scroll_positions);
    let scroll_position_fn =
        lua.create_function(move |lua, key: String| -> mlua::Result<Table> {
            let map = lock(&positions_for_read);
            let snap = map.get(&key).copied().ok_or_else(|| {
                mlua::Error::runtime(format!(
                    "tui.scroll_position: no scrollable with key `{key}` found in the current tree"
                ))
            })?;
            let t = lua.create_table()?;
            t.set("offset", snap.offset)?;
            t.set("max", snap.max)?;
            t.set("viewport_size", snap.viewport_size)?;
            Ok(t)
        })?;
    tui.set("scroll_position", scroll_position_fn)?;

    // ── NCP egress ───────────────────────────────────────────────────
    //
    // Two equivalent surfaces:
    //   tui.emit(body)
    //   tui.send_to(target, body)
    // Both push a `SideEffect::Emit` onto the shared queue; the engine
    // drains the queue after every dispatch and writes one
    // `PluginOutgoing::event(body)` per entry. `target` is a hint —
    // actual per-peer routing is the engine's broker / starter NCP
    // transforms job.

    let queue_for_emit = Arc::clone(&emit_queue);
    let emit_fn = lua.create_function(move |lua, body: Value| -> mlua::Result<()> {
        let Value::Table(t) = body else {
            return Err(mlua::Error::runtime(
                "tui.emit: `body` must be a table with a `kind` field",
            ));
        };
        let body_json: JsonValue = lua.from_value(Value::Table(t))?;
        let JsonValue::Object(map) = body_json else {
            return Err(mlua::Error::runtime(
                "tui.emit: `body` must encode to a JSON object",
            ));
        };
        if !map.get("kind").is_some_and(JsonValue::is_string) {
            return Err(mlua::Error::runtime(
                "tui.emit: `body` must contain a string `kind` field",
            ));
        }
        lock(&queue_for_emit).push(SideEffect::Emit {
            target_hint: None,
            body: map,
        });
        Ok(())
    })?;
    tui.set("emit", emit_fn)?;

    let queue_for_send_to = Arc::clone(&emit_queue);
    let send_to_fn = lua.create_function(
        move |lua, (target, body): (String, Value)| -> mlua::Result<()> {
            let Value::Table(t) = body else {
                return Err(mlua::Error::runtime(
                    "tui.send_to: `body` must be a table with a `kind` field",
                ));
            };
            let body_json: JsonValue = lua.from_value(Value::Table(t))?;
            let JsonValue::Object(map) = body_json else {
                return Err(mlua::Error::runtime(
                    "tui.send_to: `body` must encode to a JSON object",
                ));
            };
            if !map.get("kind").is_some_and(JsonValue::is_string) {
                return Err(mlua::Error::runtime(
                    "tui.send_to: `body` must contain a string `kind` field",
                ));
            }
            lock(&queue_for_send_to).push(SideEffect::Emit {
                target_hint: Some(target),
                body: map,
            });
            Ok(())
        },
    )?;
    tui.set("send_to", send_to_fn)?;

    // tui.start { initial_state, view, update }
    let started_for_start = Arc::clone(&started);
    let start_fn = lua.create_function(move |lua, args: Table| {
        if lock(&started_for_start).view_key.is_some() {
            return Err(mlua::Error::runtime(
                "tui.start: already called; only one root is supported",
            ));
        }
        let initial_state: Value = args.get("initial_state")?;
        let view: mlua::Function = args.get("view").map_err(|_| {
            mlua::Error::runtime("tui.start: `view` is required and must be a function")
        })?;
        let update: mlua::Function = args.get("update").map_err(|_| {
            mlua::Error::runtime("tui.start: `update` is required and must be a function")
        })?;

        let state_key = lua.create_registry_value(initial_state)?;
        let view_key = lua.create_registry_value(view)?;
        let update_key = lua.create_registry_value(update)?;

        let mut s = lock(&started_for_start);
        s.state_key = Some(state_key);
        s.view_key = Some(view_key);
        s.update_key = Some(update_key);
        Ok(())
    })?;
    tui.set("start", start_fn)?;

    lua.globals().set("tui", tui)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_with(src: &str) -> LuaHost {
        let host = LuaHost::new().expect("host");
        host.load_source("test", src).expect("load");
        host
    }

    #[test]
    fn tui_start_registers_handlers() {
        let host = host_with(
            r#"
            tui.start {
                initial_state = { count = 0 },
                view = function(s) return tui.text { content = "n=" .. tostring(s.count) } end,
                update = function(_, s) return s, {} end,
            }
        "#,
        );
        assert!(host.started());
    }

    #[test]
    fn render_view_returns_text_description() {
        let host = host_with(
            r#"
            tui.start {
                initial_state = { msg = "hello" },
                view = function(s) return tui.text { content = s.msg } end,
                update = function(_, s) return s, {} end,
            }
        "#,
        );
        let d = host.render_view().expect("render");
        match d {
            WidgetDescription::Text { content, .. } => assert_eq!(content, "hello"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_updates_state_and_returns_no_effects() {
        let host = host_with(
            r#"
            tui.start {
                initial_state = { n = 0 },
                view = function(s) return tui.text { content = tostring(s.n) } end,
                update = function(msg, s)
                    if msg.kind == "key.space" then return { n = s.n + 1 }, {} end
                    return s, {}
                end,
            }
        "#,
        );
        let msg = host.lua().create_table().expect("tbl");
        msg.set("kind", "key.space").expect("set");
        let effects = host.dispatch(msg).expect("dispatch");
        assert!(effects.is_empty());

        let d = host.render_view().expect("render");
        match d {
            WidgetDescription::Text { content, .. } => assert_eq!(content, "1"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_honors_exit_effect() {
        let host = host_with(
            r#"
            tui.start {
                initial_state = {},
                view = function(_) return tui.text { content = "x" } end,
                update = function(_, s) return s, { { kind = "exit" } } end,
            }
        "#,
        );
        let msg = host.lua().create_table().expect("tbl");
        msg.set("kind", "key.q").expect("set");
        let effects = host.dispatch(msg).expect("dispatch");
        assert_eq!(effects, vec![SideEffect::Exit]);
    }

    #[test]
    fn unknown_effect_kind_warns_and_drops() {
        let host = host_with(
            r#"
            tui.start {
                initial_state = {},
                view = function(_) return tui.text { content = "x" } end,
                update = function(_, s) return s, { { kind = "totally-made-up-effect" } } end,
            }
        "#,
        );
        let msg = host.lua().create_table().expect("tbl");
        msg.set("kind", "key.x").expect("set");
        let effects = host.dispatch(msg).expect("dispatch");
        assert!(effects.is_empty());
    }

    #[test]
    fn send_to_side_effect_is_parsed_into_emit() {
        let host = host_with(
            r#"
            tui.start {
                initial_state = {},
                view = function(_) return tui.text { content = "x" } end,
                update = function(_, s)
                    return s, {
                        { kind = "send_to", target = "ollama",
                          body  = { kind = "chat.input.submit", text = "hi" } },
                    }
                end,
            }
        "#,
        );
        let msg = host.lua().create_table().expect("tbl");
        msg.set("kind", "noop").expect("set");
        let effects = host.dispatch(msg).expect("dispatch");
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            SideEffect::Emit { target_hint, body } => {
                assert_eq!(target_hint.as_deref(), Some("ollama"));
                assert_eq!(
                    body.get("kind").and_then(|v| v.as_str()),
                    Some("chat.input.submit")
                );
                assert_eq!(body.get("text").and_then(|v| v.as_str()), Some("hi"));
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn imperative_tui_emit_queues_for_engine_drain() {
        let host = LuaHost::new().expect("host");
        host.lua()
            .load(r#"tui.emit { kind = "x.test", n = 7 }"#)
            .exec()
            .expect("emit");
        let drained = host.take_emit_queue();
        assert_eq!(drained.len(), 1);
        match &drained[0] {
            SideEffect::Emit { target_hint, body } => {
                assert_eq!(*target_hint, None);
                assert_eq!(body.get("kind").and_then(|v| v.as_str()), Some("x.test"));
                assert_eq!(body.get("n").and_then(|v| v.as_i64()), Some(7));
            }
            other => panic!("expected Emit, got {other:?}"),
        }
        // Drain leaves the queue empty.
        assert!(host.take_emit_queue().is_empty());
    }

    #[test]
    fn body_to_msg_table_round_trips_nested_object() {
        let host = LuaHost::new().expect("host");
        let mut body = JsonMap::new();
        body.insert("kind".into(), JsonValue::String("chat.stream.delta".into()));
        body.insert("text".into(), JsonValue::String("hi".into()));
        let mut nested = JsonMap::new();
        nested.insert("a".into(), JsonValue::from(1));
        body.insert("nested".into(), JsonValue::Object(nested));
        let t = host.body_to_msg_table(&body).expect("convert");
        let kind: String = t.get("kind").expect("kind");
        assert_eq!(kind, "chat.stream.delta");
        let nested_t: Table = t.get("nested").expect("nested");
        let a: i64 = nested_t.get("a").expect("a");
        assert_eq!(a, 1);
    }

    #[test]
    fn double_start_errors() {
        let host = LuaHost::new().expect("host");
        let src = r#"
            tui.start {
                initial_state = {},
                view = function(_) return tui.text { content = "x" } end,
                update = function(_, s) return s, {} end,
            }
            tui.start {
                initial_state = {},
                view = function(_) return tui.text { content = "y" } end,
                update = function(_, s) return s, {} end,
            }
        "#;
        let err = host.load_source("t", src).unwrap_err();
        assert!(format!("{err}").contains("already called"));
    }

    #[test]
    fn missing_start_errors_on_render() {
        let host = LuaHost::new().expect("host");
        let err = host.render_view().unwrap_err();
        assert!(matches!(err, TuiError::NotStarted));
    }

    #[test]
    fn scroll_to_queues_command() {
        let host = LuaHost::new().expect("host");
        host.lua()
            .load(r#"tui.scroll_to("transcript", 5)"#)
            .exec()
            .expect("eval");
        let cmds = host.take_scroll_commands();
        assert_eq!(
            cmds,
            vec![ScrollCommand::To {
                key: "transcript".into(),
                offset: 5,
            }]
        );
        // Drain leaves the queue empty.
        assert!(host.take_scroll_commands().is_empty());
    }

    #[test]
    fn scroll_by_accepts_negative_delta() {
        let host = LuaHost::new().expect("host");
        host.lua()
            .load(r#"tui.scroll_by("k", -10)"#)
            .exec()
            .expect("eval");
        let cmds = host.take_scroll_commands();
        assert_eq!(
            cmds,
            vec![ScrollCommand::By {
                key: "k".into(),
                delta: -10,
            }]
        );
    }

    #[test]
    fn scroll_into_view_queues_command() {
        let host = LuaHost::new().expect("host");
        host.lua()
            .load(r#"tui.scroll_into_view("transcript")"#)
            .exec()
            .expect("eval");
        assert_eq!(
            host.take_scroll_commands(),
            vec![ScrollCommand::IntoView {
                key: "transcript".into()
            }]
        );
    }

    #[test]
    fn scroll_position_returns_snapshot_when_present() {
        let host = LuaHost::new().expect("host");
        let mut map = ScrollPositionMap::new();
        map.insert(
            "transcript".into(),
            ScrollPositionSnapshot {
                offset: 7,
                max: 30,
                viewport_size: 5,
            },
        );
        host.write_scroll_positions(map);
        let table: Table = host
            .lua()
            .load(r#"return tui.scroll_position("transcript")"#)
            .eval()
            .expect("eval");
        assert_eq!(table.get::<u16>("offset").unwrap(), 7);
        assert_eq!(table.get::<u16>("max").unwrap(), 30);
        assert_eq!(table.get::<u16>("viewport_size").unwrap(), 5);
    }

    #[test]
    fn scroll_position_errors_on_unknown_key() {
        let host = LuaHost::new().expect("host");
        let err = host
            .lua()
            .load(r#"return tui.scroll_position("missing")"#)
            .eval::<Table>()
            .unwrap_err();
        assert!(format!("{err}").contains("no scrollable with key `missing`"));
    }

    #[test]
    fn scroll_to_rejects_out_of_range_offset() {
        let host = LuaHost::new().expect("host");
        let err = host
            .lua()
            .load(r#"tui.scroll_to("k", 100000)"#)
            .exec()
            .unwrap_err();
        assert!(format!("{err}").contains("must be in 0..=65535"));
    }
}
