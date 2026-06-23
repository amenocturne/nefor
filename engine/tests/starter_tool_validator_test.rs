//! Unit tests for `starter/tool-validator/init.lua` permission modes.

use std::path::PathBuf;

use mlua::{Function, Lua, Table, Value};

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

#[test]
fn starter_tool_validator_modes() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let test_path = repo_root().join("tests/lua/tool-validator/mode_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));

    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        panic!("tool_validator_mode_test.lua failed:\n{e}");
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
    log_tbl.set("debug", no_op)?;
    nefor.set("log", log_tbl)?;

    let process_tbl = lua.create_table()?;
    let run_fn = lua.create_function(|lua, args: Value| {
        let (is_probe, stdin) = match args {
            Value::Table(t) => {
                let is_probe = match t.get::<Value>("args")? {
                    Value::Table(argv) => argv
                        .sequence_values::<String>()
                        .any(|v| matches!(v, Ok(s) if s == "--version")),
                    _ => false,
                };
                let stdin = match t.get::<Value>("stdin")? {
                    Value::String(s) => Some(s.to_str()?.to_owned()),
                    _ => None,
                };
                (is_probe, stdin)
            }
            _ => (false, None),
        };
        let t = lua.create_table()?;
        let code = if is_probe {
            0
        } else if stdin.as_deref().is_some_and(|s| s.contains("forbidden")) {
            2
        } else {
            1
        };
        t.set("code", code)?;
        Ok(t)
    })?;
    process_tbl.set("run", run_fn)?;
    nefor.set("process", process_tbl)?;

    let bus_tbl = lua.create_table()?;
    let on_event = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    bus_tbl.set("on_event", on_event)?;
    nefor.set("bus", bus_tbl)?;

    let engine_tbl = lua.create_table()?;
    let calls_tbl = lua.create_table()?;
    lua.globals().set("_engine_calls", calls_tbl)?;
    let send_fn = lua.create_function(|lua, args: mlua::Variadic<Value>| {
        let payload = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            _ => return Ok(()),
        };
        let calls: Table = lua.globals().get("_engine_calls")?;
        let entry = lua.create_table()?;
        entry.set("payload", lua.create_string(&payload)?)?;
        let n = calls.len()?;
        calls.set(n + 1, entry)?;
        Ok(())
    })?;
    engine_tbl.set("send", send_fn)?;
    let now_fn = lua.create_function(|_, _: ()| Ok("2026-05-08T00:00:00.000Z".to_owned()))?;
    engine_tbl.set("now", now_fn)?;
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
    lua.globals().set("_test", test_tbl)?;
    Ok(())
}

fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let root = repo_root();
    let starter = root.join("starter").display().to_string();
    let lua_root = root.join("lua").display().to_string();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")
        "#,
    );
    lua.load(&script).exec()
}
