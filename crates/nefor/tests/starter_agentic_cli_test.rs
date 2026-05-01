//! Unit tests for `starter/agentic_cli.lua`'s argv parser, driven from
//! Rust.
//!
//! The full module is heavy (it depends on agentic_workflow, which
//! pulls the bus, the in-process observers, etc.). For the parser we
//! only need a minimal `nefor.*` surface to be present so `require()`
//! resolves without errors. We install:
//!   - `nefor.json` — for the bundled JSON binding.
//!   - `nefor.log` — no-op shims so any startup-time logging in the
//!     loaded modules doesn't blow up.
//!   - `nefor.bus.on_event` — no-op so the install_stream_json_format
//!     code path doesn't error if invoked at module load (it isn't,
//!     but cheap insurance).
//!   - `nefor.io.read_line` — returns nil immediately (we never reach
//!     the REPL code path).
//!   - `nefor.engine` — send/now/plugins/exit no-ops.
//!
//! With those in place, `require("agentic_cli")` succeeds and
//! `_parse_argv` is callable.

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
fn starter_agentic_cli_parser_tests() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let test_path = starter_dir().join("agentic_cli_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));

    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        panic!("agentic_cli_test.lua failed:\n{e}");
    }
}

fn install_stub_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;

    // nefor.json (real binding).
    nefor::lua::bindings::install_json(lua, &nefor)?;

    // nefor.log — silent no-ops.
    let log_tbl = lua.create_table()?;
    let no_op: Function = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    log_tbl.set("info", no_op.clone())?;
    log_tbl.set("warn", no_op.clone())?;
    log_tbl.set("error", no_op.clone())?;
    log_tbl.set("debug", no_op.clone())?;
    nefor.set("log", log_tbl)?;

    // nefor.bus.on_event — accept a (string, function) pair and swallow.
    let bus_tbl = lua.create_table()?;
    let on_event = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    bus_tbl.set("on_event", on_event)?;
    nefor.set("bus", bus_tbl)?;

    // nefor.io.read_line — always nil. The parser doesn't call this.
    let io_tbl = lua.create_table()?;
    let read_line = lua.create_function(|_, _: ()| Ok(Value::Nil))?;
    io_tbl.set("read_line", read_line)?;
    nefor.set("io", io_tbl)?;

    // nefor.engine — minimal surface. Tests never invoke send/exit, but
    // module-load-time references would fail without these.
    let engine_tbl = lua.create_table()?;
    let send = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    engine_tbl.set("send", send)?;
    let now = lua.create_function(|_, _: ()| Ok("2026-05-01T00:00:00.000Z".to_owned()))?;
    engine_tbl.set("now", now)?;
    let plugins_fn = lua.create_function(|lua, _: ()| {
        let arr: Table = lua.create_table()?;
        Ok(arr)
    })?;
    engine_tbl.set("plugins", plugins_fn)?;
    let exit_fn = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    engine_tbl.set("exit", exit_fn)?;
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
