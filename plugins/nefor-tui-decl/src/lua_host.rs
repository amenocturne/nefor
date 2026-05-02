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

use std::sync::{Arc, Mutex};

use mlua::{Lua, RegistryKey, Table, Value};

use crate::desc::{from_lua_table, WidgetDescription, KIND_FIELD};
use crate::error::TuiError;

#[derive(Default)]
struct StartedState {
    state_key: Option<RegistryKey>,
    view_key: Option<RegistryKey>,
    update_key: Option<RegistryKey>,
}

/// Side-effect record returned from `update`. Phase 1 honors only the
/// `Exit` variant; unknown kinds are dropped at the boundary with a
/// warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideEffect {
    Exit,
}

pub struct LuaHost {
    lua: Lua,
    started: Arc<Mutex<StartedState>>,
}

impl LuaHost {
    /// Create the VM and install the `tui` global.
    pub fn new() -> Result<Self, TuiError> {
        let lua = Lua::new();
        let started = Arc::new(Mutex::new(StartedState::default()));
        install_tui(&lua, Arc::clone(&started))?;
        Ok(LuaHost { lua, started })
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

    /// Dispatch a message through `update(msg, state)`. Returns the side-
    /// effect list as honored side-effects (phase 1: only `Exit`).
    /// Other kinds are tracing-warned and dropped.
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

        Ok(parse_side_effects(effects_val))
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| {
        tracing::error!("nefor-tui-decl: mutex poisoned; recovering for best-effort progress");
        poisoned.into_inner()
    })
}

fn parse_side_effects(v: Value) -> Vec<SideEffect> {
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

fn install_tui(lua: &Lua, started: Arc<Mutex<StartedState>>) -> Result<(), TuiError> {
    let tui = lua.create_table()?;

    // tui.text { content, key?, style?, wrap? }
    let text_fn = lua.create_function(|lua, args: Table| {
        args.set(KIND_FIELD, "text")?;
        let _ = lua;
        Ok(args)
    })?;
    tui.set("text", text_fn)?;

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
                update = function(_, s) return s, { { kind = "send_to" } } end,
            }
        "#,
        );
        let msg = host.lua().create_table().expect("tbl");
        msg.set("kind", "key.x").expect("set");
        let effects = host.dispatch(msg).expect("dispatch");
        assert!(effects.is_empty());
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
}
