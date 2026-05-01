//! Install `nefor.*` functions onto a Lua table.
//!
//! Each `install_*` function takes the shared `nefor` table and mutates it to
//! add a sub-table.
//!
//! Per D-02 the engine's Lua surface is intentionally small: logging, an
//! internal event bus (engine-private, not the NCP bus), process spawning
//! (for plugins that need to run OS commands from Lua), and a plugin
//! registration table that `init.lua` writes to so the engine knows which
//! plugin binaries to spawn.

pub mod bus;
pub mod engine;
pub mod io;
pub mod json;
pub mod plugins;
pub mod process;

pub use bus::{install_bus, EventSubscriptions, SharedSubscriptions};
pub use engine::{install_engine, EngineOps, SendTarget};
pub use io::{install_io, spawn_stdin_pump, SharedStdinPump, StdinPump};
pub use json::install_json;
pub use plugins::install_plugins;
pub use process::install_process;

use std::sync::Arc;

use mlua::{Function, Lua, Table, Value};

use crate::events::{EventBus, EventName, EventPayload, SubscriptionId};

/// Install `nefor.events.{on, off, emit}` onto `nefor_tbl`.
///
/// The bus this wires into is engine-internal — lifecycle events like
/// `startup` / `shutdown` / `tick`. Plugins observing the NCP bus never see
/// these; they're for in-engine composition (e.g. a Lua snippet in init.lua
/// that wants to react to engine shutdown).
///
/// ## Payload conversion
///
/// Event→Lua payload conversion is restricted:
/// - [`EventPayload::None`] → `nil`
/// - [`EventPayload::Tick`] → `nil`
/// - [`EventPayload::Custom`] — if the wrapped `Any` downcasts to `String`,
///   pass the string; otherwise `nil`.
///
/// Lua→payload conversion for `nefor.events.emit` is likewise restricted to
/// `nil` and `string`; anything else raises a typed error.
pub fn install_events(lua: &Lua, nefor_tbl: &Table, bus: Arc<EventBus>) -> mlua::Result<()> {
    let events = lua.create_table()?;

    let on_bus = Arc::clone(&bus);
    let on_lua = lua.clone();
    let on_fn = lua.create_function(move |_, (name, handler): (Value, Value)| {
        let name = validate_event_name(&name, "nefor.events.on")?;
        let handler = validate_handler(&handler, "nefor.events.on")?;

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
                    tracing::error!(error = %e, "Lua event handler raised");
                }
            }),
        );

        Ok(id.as_u64())
    })?;
    events.set("on", on_fn)?;

    let off_bus = Arc::clone(&bus);
    let off_fn = lua.create_function(move |_, sub_id: u64| {
        off_bus.off(SubscriptionId::from_u64(sub_id));
        Ok(())
    })?;
    events.set("off", off_fn)?;

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

struct LogArgs {
    msg: String,
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
pub(crate) fn validate_event_name(val: &Value, api: &'static str) -> mlua::Result<String> {
    match val {
        Value::String(s) => {
            let s = s.to_str()?.to_owned();
            if s.is_empty() {
                return Err(mlua::Error::runtime(format!(
                    "{api}: name must be a non-empty string (got \"\")"
                )));
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventBus, EventName, EventPayload, TICK};
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
        assert_eq!(id, 0);
    }

    #[test]
    fn emit_from_rust_invokes_lua_handler() {
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
    fn log_info_with_fields_does_not_error() {
        let (lua, _bus) = setup();
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
