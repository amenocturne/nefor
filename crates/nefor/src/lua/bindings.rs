//! Install `nefor.*` functions onto a Lua table.
//!
//! Each `install_*` function takes the shared `nefor` table and mutates it to
//! add a sub-table. Kept separate from [`crate::lua::vm`] so that the VM
//! module just orchestrates and the actual API surface lives here.
//!
//! Per spec §Rust-caliber errors at the Lua boundary: every binding validates
//! its arguments eagerly and raises a descriptive Lua runtime error on
//! failure. The Lua-visible message is prefixed with the API path (e.g.,
//! `nefor.events.on:`) so a `pcall` inside `init.lua` gets a readable
//! diagnostic without the plugin author having to string-sniff.

use std::sync::Arc;

use mlua::{Function, Lua, Table, Value};

use crate::events::{EventBus, EventName, EventPayload, SubscriptionId};

/// Max length of an event name we echo back in an error message. 64 is long
/// enough for every reasonable name and short enough that a pathological
/// megabyte-long string can't show up in logs.
const MAX_ECHO_LEN: usize = 64;

/// Install `nefor.events.{on, off, emit}` onto `nefor_tbl`.
///
/// `bus` is shared with the rest of the binary — handlers registered from Lua
/// go on the same bus the TUI emits lifecycle events to, so `nefor.events.on`
/// sees `startup` / `key` / `tick` / `resize` / `shutdown` with zero extra
/// wiring.
///
/// ## Payload conversion
///
/// Event→Lua payload conversion is deliberately restricted:
/// - [`EventPayload::None`] → `nil`
/// - [`EventPayload::Tick`] → `nil` (the event name *is* the payload)
/// - [`EventPayload::Key`] → `{ code = <string>, char = <string?>, f = <int?>, modifiers = { ctrl, shift, alt } }`
/// - [`EventPayload::Resize`] → `{ cols = <int>, rows = <int> }`
/// - [`EventPayload::Custom`] — if the wrapped `Any` downcasts to `String`,
///   we pass the string; otherwise `nil`. Typed cross-plugin payloads land
///   when a plugin actually needs them.
///
/// Lua→payload conversion for `nefor.events.emit` is likewise restricted to
/// `nil` and `string` for MVP; anything else raises a typed error. This keeps
/// the cross-thread contract simple (no parking `mlua::Value` on the bus).
pub fn install_events(lua: &Lua, nefor_tbl: &Table, bus: Arc<EventBus>) -> mlua::Result<()> {
    let events = lua.create_table()?;

    // nefor.events.on(name, handler) -> sub_id (integer)
    let on_bus = Arc::clone(&bus);
    let on_lua = lua.clone();
    let on_fn = lua.create_function(move |_, (name, handler): (Value, Value)| {
        let name = validate_event_name(&name, "nefor.events.on")?;
        let handler = validate_handler(&handler, "nefor.events.on")?;

        // Stash the Lua function in the registry so the handler closure can
        // re-fetch it without keeping a live Lua reference alive across the
        // closure boundary.
        let key = on_lua.create_registry_value(handler)?;
        let key = Arc::new(key);

        let lua_for_cb = on_lua.clone();
        let id = on_bus.on(
            EventName::from(name.as_str()),
            Box::new(move |payload| {
                let func: Function = match lua_for_cb.registry_value(&key) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to resolve Lua handler from registry");
                        return;
                    }
                };
                let arg = match payload_to_lua(&lua_for_cb, payload) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to convert event payload to Lua");
                        return;
                    }
                };
                if let Err(e) = func.call::<()>(arg) {
                    // Spec: plugin errors during event callbacks — log + skip,
                    // don't crash.
                    tracing::error!(error = %e, "Lua event handler raised");
                }
            }),
        );

        Ok(id.as_u64())
    })?;
    events.set("on", on_fn)?;

    // nefor.events.off(sub_id)
    let off_bus = Arc::clone(&bus);
    let off_fn = lua.create_function(move |_, sub_id: u64| {
        off_bus.off(SubscriptionId::from_u64(sub_id));
        Ok(())
    })?;
    events.set("off", off_fn)?;

    // nefor.events.emit(name, payload) — payload is nil or string only.
    let emit_bus = Arc::clone(&bus);
    let emit_fn = lua.create_function(move |_, (name, payload): (Value, Value)| {
        let name = validate_event_name(&name, "nefor.events.emit")?;
        let payload = match payload {
            Value::Nil => EventPayload::None,
            Value::String(s) => {
                let s = s.to_str()?.to_owned();
                EventPayload::Custom(Arc::new(s))
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.events.emit: payload must be nil or string (got {}); \
                     richer typed payloads land post-MVP",
                    other.type_name(),
                )));
            }
        };
        emit_bus.emit(&EventName::from(name.as_str()), payload);
        Ok(())
    })?;
    events.set("emit", emit_fn)?;

    nefor_tbl.set("events", events)?;
    Ok(())
}

/// Install `nefor.log.{debug, info, warn, error}` onto `nefor_tbl`.
///
/// Each function takes `(msg: string, fields?: table)`. `fields` is optional;
/// when present we render k=v pairs into the log line because `tracing`'s
/// structured-fields macro is compile-time-dispatched and can't accept a
/// dynamic table. Good enough for plugins — richer telemetry is a post-MVP
/// concern once a concrete subscriber (file logger, plugin log pane) demands
/// it.
pub fn install_log(lua: &Lua, nefor_tbl: &Table) -> mlua::Result<()> {
    let log = lua.create_table()?;
    log.set(
        "debug",
        lua.create_function(|_, a: LogArgs| {
            tracing::debug!(target: "nefor::lua", "{}{}", a.msg, a.fields);
            Ok(())
        })?,
    )?;
    log.set(
        "info",
        lua.create_function(|_, a: LogArgs| {
            tracing::info!(target: "nefor::lua", "{}{}", a.msg, a.fields);
            Ok(())
        })?,
    )?;
    log.set(
        "warn",
        lua.create_function(|_, a: LogArgs| {
            tracing::warn!(target: "nefor::lua", "{}{}", a.msg, a.fields);
            Ok(())
        })?,
    )?;
    log.set(
        "error",
        lua.create_function(|_, a: LogArgs| {
            tracing::error!(target: "nefor::lua", "{}{}", a.msg, a.fields);
            Ok(())
        })?,
    )?;
    nefor_tbl.set("log", log)?;
    Ok(())
}

/// Parsed form of `log.<level>(msg, fields?)` — a message plus a pre-formatted
/// " k=v, k=v" suffix. Pre-formatting here keeps the `tracing::*!` call sites
/// tiny.
struct LogArgs {
    msg: String,
    /// Already leading-space prefixed when non-empty: `" k=v, k=v"`. Empty
    /// string when no fields were passed. Callers just concatenate — no
    /// conditional formatting needed at the tracing call site.
    fields: String,
}

impl mlua::FromLuaMulti for LogArgs {
    fn from_lua_multi(mut values: mlua::MultiValue, lua: &Lua) -> mlua::Result<Self> {
        let msg_val = values.pop_front().unwrap_or(Value::Nil);
        let msg = match msg_val {
            Value::String(s) => s.to_str()?.to_owned(),
            Value::Nil => {
                return Err(mlua::Error::runtime(
                    "nefor.log.<level>: missing message (first argument must be a string)",
                ));
            }
            other => other
                .to_string()
                .unwrap_or_else(|_| format!("<{}>", other.type_name())),
        };

        let fields_val = values.pop_front().unwrap_or(Value::Nil);
        let fields = match fields_val {
            Value::Nil => String::new(),
            Value::Table(t) => format_fields(lua, &t)?,
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.log.<level>: fields must be a table or nil (got {})",
                    other.type_name(),
                )));
            }
        };

        Ok(LogArgs { msg, fields })
    }
}

fn format_fields(_lua: &Lua, t: &Table) -> mlua::Result<String> {
    let mut out = String::new();
    let mut first = true;
    for pair in t.clone().pairs::<Value, Value>() {
        let (k, v) = pair?;
        if first {
            out.push_str(" [");
            first = false;
        } else {
            out.push_str(", ");
        }
        let k_str = value_to_display_str(&k);
        let v_str = value_to_display_str(&v);
        out.push_str(&k_str);
        out.push('=');
        out.push_str(&v_str);
    }
    if !first {
        out.push(']');
    }
    Ok(out)
}

fn value_to_display_str(v: &Value) -> String {
    match v {
        Value::Nil => "nil".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s
            .to_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "<bad utf8>".into()),
        other => other
            .to_string()
            .unwrap_or_else(|_| format!("<{}>", other.type_name())),
    }
}

/// Validate `name` is a non-empty, reasonably short Lua string.
fn validate_event_name(val: &Value, api: &'static str) -> mlua::Result<String> {
    match val {
        Value::String(s) => {
            let s = s.to_str()?.to_owned();
            if s.is_empty() {
                return Err(mlua::Error::runtime(format!(
                    "{api}: name must be a non-empty string (got \"\")"
                )));
            }
            // Bound the echoed-back value so a megabyte-long name doesn't end
            // up in logs. The user's own value isn't truncated on the wire;
            // only the diagnostic message trims it.
            if s.len() > 512 {
                return Err(mlua::Error::runtime(format!(
                    "{api}: name too long ({} bytes); max is 512",
                    s.len(),
                )));
            }
            Ok(s)
        }
        other => Err(mlua::Error::runtime(format!(
            "{api}: name must be a string (got {})",
            other.type_name(),
        ))),
    }
}

/// Validate `val` is a Lua function and return a clone.
fn validate_handler(val: &Value, api: &'static str) -> mlua::Result<Function> {
    match val {
        Value::Function(f) => Ok(f.clone()),
        other => Err(mlua::Error::runtime(format!(
            "{api}: handler must be a function (got {})",
            other.type_name(),
        ))),
    }
}

/// Convert an [`EventPayload`] into a Lua [`Value`] for handler dispatch.
fn payload_to_lua(lua: &Lua, payload: &EventPayload) -> mlua::Result<Value> {
    match payload {
        EventPayload::None | EventPayload::Tick => Ok(Value::Nil),
        EventPayload::Key(ke) => {
            let t = lua.create_table()?;
            let (code, maybe_char, maybe_f) = describe_key_code(ke.code);
            t.set("code", code)?;
            if let Some(c) = maybe_char {
                t.set("char", c.to_string())?;
            }
            if let Some(f) = maybe_f {
                t.set("f", f)?;
            }
            let mods = lua.create_table()?;
            mods.set(
                "ctrl",
                ke.modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL),
            )?;
            mods.set(
                "shift",
                ke.modifiers.contains(crossterm::event::KeyModifiers::SHIFT),
            )?;
            mods.set(
                "alt",
                ke.modifiers.contains(crossterm::event::KeyModifiers::ALT),
            )?;
            t.set("modifiers", mods)?;
            Ok(Value::Table(t))
        }
        EventPayload::Resize { cols, rows } => {
            let t = lua.create_table()?;
            t.set("cols", *cols)?;
            t.set("rows", *rows)?;
            Ok(Value::Table(t))
        }
        EventPayload::Custom(any) => {
            if let Some(s) = any.downcast_ref::<String>() {
                let ls = lua.create_string(s)?;
                Ok(Value::String(ls))
            } else {
                Ok(Value::Nil)
            }
        }
    }
}

/// Map a [`crossterm::event::KeyCode`] into the `(code_name, maybe_char, maybe_f)`
/// triple used by the Lua-visible event table. Keeping this centralized means
/// the `nefor.ui.subscribe_key` pattern matcher (next commit) can reuse the
/// same name strings.
fn describe_key_code(code: crossterm::event::KeyCode) -> (&'static str, Option<char>, Option<u8>) {
    use crossterm::event::KeyCode as K;
    match code {
        K::Backspace => ("Backspace", None, None),
        K::Enter => ("Enter", None, None),
        K::Left => ("Left", None, None),
        K::Right => ("Right", None, None),
        K::Up => ("Up", None, None),
        K::Down => ("Down", None, None),
        K::Home => ("Home", None, None),
        K::End => ("End", None, None),
        K::PageUp => ("PageUp", None, None),
        K::PageDown => ("PageDown", None, None),
        K::Tab => ("Tab", None, None),
        K::BackTab => ("BackTab", None, None),
        K::Delete => ("Delete", None, None),
        K::Insert => ("Insert", None, None),
        K::F(n) => ("F", None, Some(n)),
        K::Char(c) => ("Char", Some(c), None),
        K::Null => ("Null", None, None),
        K::Esc => ("Esc", None, None),
        K::CapsLock => ("CapsLock", None, None),
        K::ScrollLock => ("ScrollLock", None, None),
        K::NumLock => ("NumLock", None, None),
        K::PrintScreen => ("PrintScreen", None, None),
        K::Pause => ("Pause", None, None),
        K::Menu => ("Menu", None, None),
        K::KeypadBegin => ("KeypadBegin", None, None),
        K::Media(_) => ("Media", None, None),
        K::Modifier(_) => ("Modifier", None, None),
    }
}

/// Truncate an event-name echo for log/error messages.
#[allow(dead_code)]
fn echo(name: &str) -> String {
    if name.len() <= MAX_ECHO_LEN {
        name.to_string()
    } else {
        format!("{}…", &name[..MAX_ECHO_LEN])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventBus, EventName, EventPayload, KEY, TICK};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn setup() -> (Lua, Arc<EventBus>) {
        let lua = Lua::new();
        let bus = Arc::new(EventBus::new());
        let nefor = lua.create_table().unwrap();
        install_events(&lua, &nefor, Arc::clone(&bus)).unwrap();
        install_log(&lua, &nefor).unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        (lua, bus)
    }

    #[test]
    fn events_on_returns_integer_id() {
        let (lua, _bus) = setup();
        let id: u64 = lua
            .load(r#"return nefor.events.on("test", function() end)"#)
            .eval()
            .expect("eval ok");
        // First subscription starts at 0; the exact value isn't the contract
        // — that it's an integer is.
        assert_eq!(id, 0);
    }

    #[test]
    fn emit_from_rust_invokes_lua_handler() {
        let (lua, bus) = setup();
        // Lua handler that pokes a Rust-side counter via a global function.
        let counter = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&counter);
        let observe = lua
            .create_function(move |_, ()| {
                c.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        lua.globals().set("observe", observe).unwrap();

        lua.load(r#"nefor.events.on("tick", function() observe() end)"#)
            .exec()
            .expect("exec ok");

        bus.emit(&EventName::from(TICK), EventPayload::Tick);
        bus.emit(&EventName::from(TICK), EventPayload::Tick);
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn off_stops_further_invocations() {
        let (lua, bus) = setup();
        let counter = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&counter);
        let observe = lua
            .create_function(move |_, ()| {
                c.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        lua.globals().set("observe", observe).unwrap();

        lua.load(
            r#"
            sub = nefor.events.on("tick", function() observe() end)
            "#,
        )
        .exec()
        .unwrap();

        bus.emit(&EventName::from(TICK), EventPayload::Tick);
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        lua.load("nefor.events.off(sub)").exec().unwrap();
        bus.emit(&EventName::from(TICK), EventPayload::Tick);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn emit_from_lua_delivers_string_payload() {
        let (lua, bus) = setup();
        let got = Arc::new(std::sync::Mutex::new(None::<String>));
        let g = Arc::clone(&got);
        bus.on(
            EventName::from("plugin:hello"),
            Box::new(move |payload| {
                if let EventPayload::Custom(any) = payload {
                    if let Some(s) = any.downcast_ref::<String>() {
                        *g.lock().unwrap() = Some(s.clone());
                    }
                }
            }),
        );

        lua.load(r#"nefor.events.emit("plugin:hello", "world")"#)
            .exec()
            .unwrap();
        assert_eq!(*got.lock().unwrap(), Some("world".to_string()));
    }

    #[test]
    fn empty_event_name_is_rejected() {
        let (lua, _bus) = setup();
        let err = lua
            .load(r#"nefor.events.on("", function() end)"#)
            .exec()
            .expect_err("empty string must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("non-empty string"),
            "expected message to mention 'non-empty string'; got: {msg}"
        );
    }

    #[test]
    fn nil_handler_is_rejected() {
        let (lua, _bus) = setup();
        let err = lua
            .load(r#"nefor.events.on("tick", nil)"#)
            .exec()
            .expect_err("nil handler must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("must be a function"),
            "expected message to mention 'must be a function'; got: {msg}"
        );
    }

    #[test]
    fn non_string_non_nil_payload_is_rejected() {
        let (lua, _bus) = setup();
        let err = lua
            .load(r#"nefor.events.emit("x", 42)"#)
            .exec()
            .expect_err("integer payload must be rejected");
        assert!(err.to_string().contains("nil or string"));
    }

    #[test]
    fn key_payload_table_has_expected_shape() {
        let (lua, bus) = setup();
        let got = Arc::new(std::sync::Mutex::new(None::<(String, bool, bool)>));
        let g = Arc::clone(&got);
        let observe = lua
            .create_function(move |_, (code, ctrl, is_char): (String, bool, bool)| {
                *g.lock().unwrap() = Some((code, ctrl, is_char));
                Ok(())
            })
            .unwrap();
        lua.globals().set("observe", observe).unwrap();

        lua.load(
            r#"
            nefor.events.on("key", function(ev)
                observe(ev.code, ev.modifiers.ctrl, ev.char ~= nil)
            end)
            "#,
        )
        .exec()
        .unwrap();

        let ke = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        bus.emit(&EventName::from(KEY), EventPayload::Key(ke));

        let got = got.lock().unwrap().clone();
        assert_eq!(got, Some(("Char".to_string(), true, true)));
    }

    #[test]
    fn log_info_with_fields_does_not_error() {
        let (lua, _bus) = setup();
        // Happy path: a known level, a message, and a fields table with mixed
        // value types. We verify no error/panic.
        lua.load(
            r#"
            nefor.log.debug("debug msg")
            nefor.log.info("info msg", { user = "alice", count = 3 })
            nefor.log.warn("warn msg", { ok = true })
            nefor.log.error("error msg")
            "#,
        )
        .exec()
        .expect("all log calls should succeed");
    }

    #[test]
    fn log_with_non_table_fields_errors() {
        let (lua, _bus) = setup();
        let err = lua
            .load(r#"nefor.log.info("msg", 123)"#)
            .exec()
            .expect_err("non-table fields should error");
        assert!(err.to_string().contains("must be a table or nil"));
    }

    #[test]
    fn log_with_no_message_errors() {
        let (lua, _bus) = setup();
        let err = lua
            .load(r#"nefor.log.info()"#)
            .exec()
            .expect_err("missing message should error");
        assert!(err.to_string().contains("missing message"));
    }
}
