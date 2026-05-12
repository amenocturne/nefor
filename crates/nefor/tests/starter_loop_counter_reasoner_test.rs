//! Unit tests for `starter/reasoners/loop_counter.lua`. Mirrors the
//! harness pattern in `starter_agentic_workflow_test.rs`.

use std::path::PathBuf;

use mlua::{Function, Lua, Table, Value};

fn starter_dir() -> PathBuf {
    repo_root().join("starter")
}

fn lua_dir() -> PathBuf {
    repo_root().join("lua")
}

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root is two levels above crates/nefor")
        .to_path_buf()
}

#[test]
fn starter_loop_counter_reasoner_full() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let test_path = starter_dir().join("reasoners/loop_counter_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));

    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        panic!("loop_counter_test.lua failed:\n{e}");
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
    let now_fn = lua.create_function(|_, _: ()| Ok("2026-05-08T00:00:00.000Z".to_owned()))?;
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

    let test_tbl = lua.create_table()?;
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
    let lua_root = lua_dir();
    let lua_root_str = lua_root.display().to_string();
    let rg_plugin_lua = repo_root().join("plugins").join("reasoner-graph").join("lua");
    let rg_plugin_lua_str = rg_plugin_lua.display().to_string();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          "{rg_plugin_lua}/?.lua",
          "{rg_plugin_lua}/?/init.lua",
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")
        "#,
        starter = starter_str,
        lua_root = lua_root_str,
        rg_plugin_lua = rg_plugin_lua_str,
    );
    lua.load(&script).exec()
}
