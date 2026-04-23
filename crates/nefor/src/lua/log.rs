//! Temporary local `LogEntry` shape for the step-invocation machinery.
//!
//! This type exists only so Slice 2 I2 (step function binding + `nefor.engine.send`)
//! can compile and be tested independently of Slice 2 I1 (session log persistence).
//! In I3 the orchestrator will swap callers over to `session::LogEntry`; at that
//! point this module is deleted.
//!
//! Minimal fields — exactly what the step binding needs to build the Lua table:
//! `{ ts = "<iso>", origin = "<name-or-step>", target = "<name>" | nil, payload = "<string>" }`.

#![allow(dead_code)] // wired into broker in I3; until then only used by tests in this module

use mlua::{Lua, Table};

/// Single entry in a step-visible log.
///
/// TEMPORARY: superseded by `session::LogEntry` in I3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// ISO8601 timestamp. The step binding does not validate format — it
    /// forwards whatever string the engine recorded.
    pub ts: String,
    /// `"step"` when this entry was emitted by the step function itself, or
    /// the plugin name when it came in over NCP.
    pub origin: String,
    /// Target plugin for this message. `None` for broadcasts.
    pub target: Option<String>,
    /// Raw NCP line (envelope + body). The step function interprets this —
    /// the engine does not parse it.
    pub payload: String,
}

/// Convert a single [`LogEntry`] to a Lua table with the canonical field layout.
///
/// Missing `target` comes through as Lua `nil` (not the string `"nil"`) so
/// `e.target == nil` in Lua is the idiomatic broadcast check.
pub fn log_entry_to_lua_table(lua: &Lua, e: &LogEntry) -> mlua::Result<Table> {
    let tbl = lua.create_table()?;
    tbl.set("ts", lua.create_string(&e.ts)?)?;
    tbl.set("origin", lua.create_string(&e.origin)?)?;
    match &e.target {
        Some(t) => tbl.set("target", lua.create_string(t)?)?,
        None => tbl.set("target", mlua::Value::Nil)?,
    }
    tbl.set("payload", lua.create_string(&e.payload)?)?;
    Ok(tbl)
}

/// Convert a slice of [`LogEntry`] to a Lua array table (integer keys `1..=n`),
/// preserving slice order.
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

    fn lua() -> Lua {
        Lua::new()
    }

    #[test]
    fn log_entry_to_lua_table_origin_and_target() {
        let l = lua();
        let entry = LogEntry {
            ts: "2026-04-23T12:34:56Z".into(),
            origin: "mock-plugin".into(),
            target: Some("nefor-chat".into()),
            payload: "hello body".into(),
        };
        let tbl = log_entry_to_lua_table(&l, &entry).expect("convert ok");
        let ts: String = tbl.get("ts").unwrap();
        let origin: String = tbl.get("origin").unwrap();
        let target: String = tbl.get("target").unwrap();
        let payload: String = tbl.get("payload").unwrap();
        assert_eq!(ts, "2026-04-23T12:34:56Z");
        assert_eq!(origin, "mock-plugin");
        assert_eq!(target, "nefor-chat");
        assert_eq!(payload, "hello body");
    }

    #[test]
    fn log_entry_nil_target_is_lua_nil() {
        let l = lua();
        let entry = LogEntry {
            ts: "t".into(),
            origin: "step".into(),
            target: None,
            payload: "p".into(),
        };
        let tbl = log_entry_to_lua_table(&l, &entry).expect("convert ok");
        let target_val: mlua::Value = tbl.get("target").unwrap();
        assert!(matches!(target_val, mlua::Value::Nil));
    }

    #[test]
    fn log_to_lua_table_preserves_order() {
        let l = lua();
        let entries: Vec<LogEntry> = (0..5)
            .map(|i| LogEntry {
                ts: format!("t{i}"),
                origin: format!("o{i}"),
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
