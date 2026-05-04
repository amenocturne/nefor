//! Unit tests for `starter/sessions.lua`'s persistence + control-event
//! filtering, driven from Rust. Mirrors the `starter_ncp_test.rs`
//! harness pattern: install a stub `nefor.*` surface, point
//! NEFOR_DATA_HOME at a tempdir, then exercise the module directly.

use std::path::PathBuf;
use std::sync::Mutex;

use mlua::{Function, Lua, Table, Value};

fn starter_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root is two levels above crates/nefor")
        .join("starter")
}

#[test]
fn jsonl_excludes_session_control_events() {
    // Sessions module's persistence hook drops any envelope whose inner
    // `body.kind` starts with "sessions." so a resume_request event
    // never lands in the on-disk jsonl. We exercise this by driving the
    // hook directly with two envelopes — one normal, one a
    // sessions.resume_request — and asserting the file contains only
    // the normal one (plus the header).
    let tempdir = tempfile::tempdir().expect("tempdir");
    // Point sessions.lua's data root at our tempdir.
    let prev = std::env::var("NEFOR_DATA_HOME").ok();
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("NEFOR_DATA_HOME", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    // Initialise the sessions module (mints a session id, opens jsonl).
    lua.load(
        r#"
        sessions = require("sessions")
        sessions.init()
        "#,
    )
    .exec()
    .expect("sessions init");

    let session_id: String = lua
        .load(r#"return sessions.current_id()"#)
        .eval()
        .expect("current_id");
    assert!(!session_id.is_empty(), "minted session id");
    let session_path: String = lua
        .load(r#"return sessions.current_path()"#)
        .eval()
        .expect("current_path");
    assert!(!session_path.is_empty(), "current_path");

    // Drive the persistence hook directly via the module's `_persist_envelope`
    // test handle. The hook expects a log-entry table {ts, origin, payload}
    // with payload as the JSON wire string of an NCP envelope.
    lua.load(
        r#"
        local json = nefor.json
        local function entry(origin, body)
            return {
                ts      = "2026-05-04T00:00:00.000Z",
                origin  = origin,
                payload = json.encode({ type = "event", body = body }),
            }
        end
        -- Normal traffic — must be persisted.
        sessions._persist_envelope(entry("ollama", { kind = "chat.message.append", role = "user", text = "hi" }))
        -- Session control event — must be DROPPED.
        sessions._persist_envelope(entry("engine", { kind = "sessions.resume_request", session_id = "x" }))
        sessions._persist_envelope(entry("engine", { kind = "sessions.session_end", session_id = "x" }))
        sessions._persist_envelope(entry("engine", { kind = "sessions.session_start", session_id = "y" }))
        sessions._persist_envelope(entry("engine", { kind = "sessions.resume_done", session_id = "y" }))
        -- Another normal entry.
        sessions._persist_envelope(entry("nefor-tui", { kind = "chat.input.submit", text = "hello" }))
        "#,
    )
    .exec()
    .expect("drive persistence");

    // Read the file back and assert the filter behaviour.
    let body = std::fs::read_to_string(&session_path).expect("read jsonl");
    let lines: Vec<&str> = body.lines().collect();
    // Header + 2 normal entries (the four sessions.* drops).
    assert_eq!(
        lines.len(),
        3,
        "expected header + 2 entries, got {}: {body}",
        lines.len()
    );
    // Header line carries `_session: true`.
    assert!(
        lines[0].contains("\"_session\":true"),
        "header line missing: {}",
        lines[0]
    );
    // Two retained entries — the chat.message.append and the
    // chat.input.submit, in append order.
    assert!(
        lines[1].contains("chat.message.append"),
        "first non-header entry should be chat.message.append: {}",
        lines[1]
    );
    assert!(
        lines[2].contains("chat.input.submit"),
        "second non-header entry should be chat.input.submit: {}",
        lines[2]
    );
    // Belt-and-braces: confirm no sessions.* string snuck through.
    for line in lines.iter().skip(1) {
        assert!(
            !line.contains("\"sessions."),
            "sessions.* control event leaked into jsonl: {line}",
        );
    }

    // Restore env.
    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_HOME", v),
        None => std::env::remove_var("NEFOR_DATA_HOME"),
    }
}

// Process-global lock to serialise tests that mutate NEFOR_DATA_HOME.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn install_stub_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    nefor::lua::bindings::install_json(lua, &nefor)?;

    // log.* — no-op
    let log_tbl = lua.create_table()?;
    let no_op: Function = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    log_tbl.set("info", no_op.clone())?;
    log_tbl.set("warn", no_op.clone())?;
    log_tbl.set("error", no_op.clone())?;
    log_tbl.set("debug", no_op)?;
    nefor.set("log", log_tbl)?;

    // bus.on_event — no-op (sessions.lua's resume_request listener fires
    // through here; this test doesn't need to drive it).
    let bus_tbl = lua.create_table()?;
    let on_event = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    bus_tbl.set("on_event", on_event)?;
    nefor.set("bus", bus_tbl)?;

    // events.on — no-op (handle_shutdown uses this).
    let events_tbl = lua.create_table()?;
    let events_on = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    events_tbl.set("on", events_on)?;
    nefor.set("events", events_tbl)?;

    // engine.{send, plugins, now}
    let engine_tbl = lua.create_table()?;
    let send_fn = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    engine_tbl.set("send", send_fn)?;
    let now_fn = lua.create_function(|_, _: ()| Ok("2026-05-04T00:00:00.000Z".to_owned()))?;
    engine_tbl.set("now", now_fn)?;
    let plugins_fn = lua.create_function(|lua, _: ()| {
        let arr: Table = lua.create_table()?;
        Ok(arr)
    })?;
    engine_tbl.set("plugins", plugins_fn)?;
    nefor.set("engine", engine_tbl)?;

    lua.globals().set("nefor", nefor)?;
    Ok(())
}

fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let starter = starter_dir();
    let starter_str = starter.display().to_string();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          package.path,
        }}, ";")
        "#,
        starter = starter_str
    );
    lua.load(&script).exec()
}
