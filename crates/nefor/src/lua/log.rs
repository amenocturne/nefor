//! Lua-table conversion for `session::LogEntry`.
//!
//! The step function in `init.lua` takes two arrays of entries — the saved log
//! (from a parent session, if any) and the current log (everything routed
//! through the broker this run). Each entry arrives in Lua as:
//!
//! ```lua
//! { ts = "<iso>", origin = "<name-or-step>", target = "<name>" | nil, payload = "<raw-line>" }
//! ```
//!
//! `origin` serializes exactly as the session log does on disk — a plain
//! plugin name for `Origin::Plugin`, or the literal string `"step"` for
//! `Origin::Step`. `target` is Lua `nil` when the entry had no directed
//! target (plugin-originated lines and broadcast step-sends).

use mlua::{Lua, Table, Value};

use crate::session::{LogEntry, Origin};

/// Convert a single [`LogEntry`] to a Lua table with the canonical field
/// layout described in the module doc.
///
/// A missing `target` comes through as Lua `nil` (not the string `"nil"`) so
/// `e.target == nil` in Lua is the idiomatic broadcast check.
pub fn log_entry_to_lua_table(lua: &Lua, e: &LogEntry) -> mlua::Result<Table> {
    let tbl = lua.create_table()?;
    tbl.set("ts", lua.create_string(e.ts.to_iso8601())?)?;
    let origin_str = match &e.origin {
        Origin::Plugin(name) => name.as_str().to_owned(),
        Origin::Step => "step".to_owned(),
    };
    tbl.set("origin", lua.create_string(&origin_str)?)?;
    match &e.target {
        Some(t) => tbl.set("target", lua.create_string(t.as_str())?)?,
        None => tbl.set("target", Value::Nil)?,
    }
    tbl.set("payload", lua.create_string(&e.payload)?)?;
    Ok(tbl)
}

/// Convert a slice of [`LogEntry`] to a Lua array table (integer keys
/// `1..=n`), preserving slice order.
pub fn log_to_lua_table(lua: &Lua, log: &[LogEntry]) -> mlua::Result<Table> {
    let arr = lua.create_table()?;
    for (i, entry) in log.iter().enumerate() {
        // Lua arrays are 1-indexed.
        arr.set(i + 1, log_entry_to_lua_table(lua, entry)?)?;
    }
    Ok(arr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nefor_protocol::{PluginName, Timestamp};

    fn lua() -> Lua {
        Lua::new()
    }

    fn ts() -> Timestamp {
        Timestamp::parse("2026-04-23T12:34:56.000Z").expect("valid ts")
    }

    #[test]
    fn log_entry_to_lua_table_origin_and_target() {
        let l = lua();
        let entry = LogEntry {
            ts: ts(),
            origin: Origin::Plugin(PluginName::new("mock-plugin").unwrap()),
            target: Some(PluginName::new("nefor-chat").unwrap()),
            payload: "hello body".into(),
        };
        let tbl = log_entry_to_lua_table(&l, &entry).expect("convert ok");
        let ts_s: String = tbl.get("ts").unwrap();
        let origin: String = tbl.get("origin").unwrap();
        let target: String = tbl.get("target").unwrap();
        let payload: String = tbl.get("payload").unwrap();
        assert_eq!(ts_s, "2026-04-23T12:34:56.000Z");
        assert_eq!(origin, "mock-plugin");
        assert_eq!(target, "nefor-chat");
        assert_eq!(payload, "hello body");
    }

    #[test]
    fn log_entry_step_origin_serializes_as_step() {
        let l = lua();
        let entry = LogEntry {
            ts: ts(),
            origin: Origin::Step,
            target: None,
            payload: "p".into(),
        };
        let tbl = log_entry_to_lua_table(&l, &entry).expect("convert ok");
        let origin: String = tbl.get("origin").unwrap();
        assert_eq!(origin, "step");
        let target_val: Value = tbl.get("target").unwrap();
        assert!(matches!(target_val, Value::Nil));
    }

    #[test]
    fn log_to_lua_table_preserves_order() {
        let l = lua();
        let entries: Vec<LogEntry> = (0..5)
            .map(|i| LogEntry {
                ts: ts(),
                origin: Origin::Plugin(PluginName::new(format!("p{i}")).unwrap()),
                target: None,
                payload: format!("p{i}"),
            })
            .collect();
        let arr = log_to_lua_table(&l, &entries).expect("convert ok");
        let len = arr.len().expect("len ok");
        assert_eq!(len, 5);
        for i in 0..5 {
            let entry: Table = arr.get(i + 1).expect("entry present");
            let payload: String = entry.get("payload").unwrap();
            assert_eq!(payload, format!("p{i}"));
        }
    }

    #[test]
    fn log_to_lua_table_empty_slice_is_length_zero() {
        let l = lua();
        let arr = log_to_lua_table(&l, &[]).expect("convert ok");
        assert_eq!(arr.len().expect("len ok"), 0);
    }
}
