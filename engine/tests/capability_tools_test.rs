//! Lua tool definitions over the basic-tools and git-worktree capability
//! plugins, and the compositor wrapper that turns those processes into tool
//! sources. Assertions live in `tests/lua/tool/capability_tools_test.lua`.

use std::path::PathBuf;

use mlua::{Function, Lua, Value};

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

#[test]
fn capability_tools_advertise_validate_and_forward() {
    let lua = Lua::new();
    let nefor = lua.create_table().expect("nefor table");
    nefor::lua::bindings::install_json(&lua, &nefor).expect("json");
    let log = lua.create_table().expect("log table");
    let no_op: Function = lua
        .create_function(|_, _: mlua::Variadic<Value>| Ok(()))
        .expect("no-op");
    for level in ["info", "warn", "error", "debug"] {
        log.set(level, no_op.clone()).expect("log level");
    }
    nefor.set("log", log).expect("log");
    lua.globals().set("nefor", nefor).expect("nefor");

    let root = repo_root();
    let lua_root = root.join("lua").display().to_string();
    let gate_lua = root.join("plugins/tool-gate/lua").display().to_string();
    let fixtures = root.join("tests/lua/tool/fixtures").display().to_string();
    lua.load(format!(
        r#"
        package.path = table.concat({{
          "{lua_root}/?.lua", "{lua_root}/?/init.lua",
          "{gate_lua}/?.lua", "{gate_lua}/?/init.lua",
          package.path,
        }}, ";")
        TOOL_FIXTURES = "{fixtures}"
        "#
    ))
    .exec()
    .expect("set package.path");

    let path = root.join("tests/lua/tool/capability_tools_test.lua");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    if let Err(e) = lua.load(&src).set_name(path.display().to_string()).exec() {
        panic!("capability_tools_test.lua failed:\n{e}");
    }
}
