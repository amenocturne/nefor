//! Unit tests for `starter/agentic_workflow.lua`'s for_chat adapter,
//! driven from Rust. Mirrors `starter_ncp_test.rs`'s harness pattern.
//!
//! The module under test depends on the same `nefor.*` surface as the
//! agentic_cli parser test plus a working `nefor.engine.{send, plugins,
//! exit, now}` and `nefor.json`. We never inspect `engine.send`
//! recordings here — for_chat's chat.model.set arm only mutates local
//! config — but the surface still has to be present so module load and
//! the for_chat closure don't blow up on first invocation.
//!
//! The Lua test file itself is `starter/agentic_workflow_test.lua`.

use std::path::PathBuf;

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
fn starter_agentic_workflow_for_chat_model_switch() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let test_path = starter_dir().join("agentic_workflow_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));

    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        panic!("agentic_workflow_test.lua failed:\n{e}");
    }
}

fn install_stub_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;

    nefor::lua::bindings::install_json(lua, &nefor)?;

    let log_tbl = lua.create_table()?;
    let no_op: Function = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    log_tbl.set("info", no_op.clone())?;
    log_tbl.set("warn", no_op.clone())?;
    log_tbl.set("error", no_op.clone())?;
    log_tbl.set("debug", no_op.clone())?;
    nefor.set("log", log_tbl)?;

    // Bus subscription stub: register handlers in a Lua-side table so
    // tests can drive them via `_test.fire_bus(kind, body)`. The real
    // broker dispatches via `dispatch_subscriptions`; here we fan out
    // synchronously inside a single VM with the same kind-keyed shape.
    let bus_tbl = lua.create_table()?;
    let bus_registry = lua.create_table()?;
    lua.globals().set("_bus_handlers", bus_registry)?;
    let on_event = lua.create_function(|lua, args: mlua::Variadic<Value>| {
        let kind = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            _ => {
                return Err(mlua::Error::runtime(
                    "stub bus.on_event: kind must be string",
                ));
            }
        };
        let handler: Function = match args.get(1) {
            Some(Value::Function(f)) => f.clone(),
            _ => {
                return Err(mlua::Error::runtime(
                    "stub bus.on_event: handler must be function",
                ));
            }
        };
        let registry: Table = lua.globals().get("_bus_handlers")?;
        let list: Table = match registry.get::<Value>(kind.as_str())? {
            Value::Table(t) => t,
            _ => {
                let t = lua.create_table()?;
                registry.set(kind.as_str(), t.clone())?;
                t
            }
        };
        let len = list.len()?;
        list.set(len + 1, handler)?;
        Ok(())
    })?;
    bus_tbl.set("on_event", on_event)?;
    nefor.set("bus", bus_tbl)?;

    // engine — captures send calls into `_engine_calls` so tests can
    // assert the orchestrator emitted the right cancel/reset envelopes
    // on session_end. plugins() returns a fixed list seeded by
    // `_test.set_plugins(...)`.
    let engine_tbl = lua.create_table()?;
    let calls_tbl = lua.create_table()?;
    lua.globals().set("_engine_calls", calls_tbl)?;
    let plugin_list = lua.create_table()?;
    lua.globals().set("_engine_plugins", plugin_list)?;
    let send_fn = lua.create_function(|lua, args: mlua::Variadic<Value>| {
        let payload = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            _ => return Ok(()),
        };
        let target = match args.get(1) {
            Some(Value::String(s)) => Some(s.to_str()?.to_owned()),
            _ => None,
        };
        let calls: Table = lua.globals().get("_engine_calls")?;
        let entry = lua.create_table()?;
        entry.set("payload", lua.create_string(&payload)?)?;
        match target {
            Some(t) => entry.set("target", lua.create_string(&t)?)?,
            None => entry.set("target", Value::Nil)?,
        }
        let n = calls.len()?;
        calls.set(n + 1, entry)?;
        Ok(())
    })?;
    engine_tbl.set("send", send_fn)?;
    let now_fn = lua.create_function(|_, _: ()| Ok("2026-05-03T00:00:00.000Z".to_owned()))?;
    engine_tbl.set("now", now_fn)?;
    let plugins_fn = lua.create_function(|lua, _: ()| {
        let arr: Table = match lua.globals().get::<Value>("_engine_plugins")? {
            Value::Table(t) => t,
            _ => lua.create_table()?,
        };
        Ok(arr)
    })?;
    engine_tbl.set("plugins", plugins_fn)?;
    let exit_fn = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    engine_tbl.set("exit", exit_fn)?;
    nefor.set("engine", engine_tbl)?;

    lua.globals().set("nefor", nefor)?;

    // _test surface for the Lua test driver.
    let test_tbl = lua.create_table()?;
    // _test.fire_bus(kind, body_table) — invoke every registered handler
    // for `kind` with a synthesised log-entry table. Mirrors the shape
    // dispatch_subscriptions builds: { ts, origin, payload }, where
    // payload is the JSON-encoded `{type:"event", body:{kind, ...}}`.
    let fire_bus = lua.create_function(|lua, args: mlua::Variadic<Value>| {
        let kind = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            _ => return Err(mlua::Error::runtime("_test.fire_bus: kind must be string")),
        };
        let body: Table = match args.get(1) {
            Some(Value::Table(t)) => t.clone(),
            _ => lua.create_table()?,
        };
        body.set("kind", lua.create_string(&kind)?)?;
        let envelope = lua.create_table()?;
        envelope.set("type", lua.create_string("event")?)?;
        envelope.set("body", body)?;
        let json: Table = lua.globals().get::<Table>("nefor")?.get::<Table>("json")?;
        let encode: Function = json.get("encode")?;
        let payload: String = encode.call(envelope)?;
        let entry = lua.create_table()?;
        entry.set("ts", lua.create_string("2026-05-04T00:00:00.000Z")?)?;
        entry.set("origin", lua.create_string("engine")?)?;
        entry.set("payload", lua.create_string(&payload)?)?;
        let registry: Table = lua.globals().get("_bus_handlers")?;
        let list: Value = registry.get::<Value>(kind.as_str())?;
        if let Value::Table(t) = list {
            let len = t.len()?;
            for i in 1..=len {
                let h: Function = t.get(i)?;
                h.call::<()>(entry.clone())?;
            }
        }
        Ok(())
    })?;
    test_tbl.set("fire_bus", fire_bus)?;
    // _test.calls() — drain captured engine.send calls and return them
    // as `{ { target?, payload } }`.
    let test_calls = lua.create_function(|lua, _: ()| {
        let calls: Table = lua.globals().get("_engine_calls")?;
        Ok(calls)
    })?;
    test_tbl.set("calls", test_calls)?;
    let calls_clear = lua.create_function(|lua, _: ()| {
        let fresh = lua.create_table()?;
        lua.globals().set("_engine_calls", fresh)?;
        Ok(())
    })?;
    test_tbl.set("calls_clear", calls_clear)?;
    let set_plugins = lua.create_function(|lua, names: Table| {
        lua.globals().set("_engine_plugins", names)?;
        Ok(())
    })?;
    test_tbl.set("set_plugins", set_plugins)?;
    lua.globals().set("_test", test_tbl)?;
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
