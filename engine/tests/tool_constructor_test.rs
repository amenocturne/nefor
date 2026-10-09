//! Unit tests for `lua/libs/tool` — the uniform tool constructor, its schema
//! validator, and its pattern subset. Assertions live in `tests/lua/tool/`;
//! this harness stubs the `nefor` global, wires `package.path`, and runs each.

use std::path::PathBuf;

use mlua::{Function, Lua, Value};

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

fn run(script: &str) {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    let lua_root = repo_root().join("lua").display().to_string();
    lua.load(format!(
        r#"package.path = "{lua_root}/?.lua;{lua_root}/?/init.lua;" .. package.path"#
    ))
    .exec()
    .expect("set package.path");
    let path = repo_root().join("tests/lua/tool").join(script);
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    if let Err(e) = lua.load(&src).set_name(path.display().to_string()).exec() {
        panic!("{script} failed:\n{e}");
    }
}

fn install_stub_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    nefor::lua::bindings::install_json(lua, &nefor)?;
    let log = lua.create_table()?;
    let no_op: Function = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    for level in ["info", "warn", "error", "debug"] {
        log.set(level, no_op.clone())?;
    }
    nefor.set("log", log)?;
    lua.globals().set("nefor", nefor)?;
    Ok(())
}

#[test]
fn pattern_subset_matches_and_rejects_unsupported_syntax() {
    run("pattern_test.lua");
}

#[test]
fn schema_check_and_closed_argument_validation() {
    run("schema_test.lua");
}

#[test]
fn tool_definition_advertisement_and_invocation() {
    run("source_test.lua");
}
