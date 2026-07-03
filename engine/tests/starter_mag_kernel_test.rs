//! Runs the mag-kernel Lua unit tests (`tests/lua/mag-kernel/*.lua`) in a
//! bare Lua VM. Mirrors the harness pattern in
//! `starter_loop_counter_reasoner_test.rs`: install a minimal `nefor` stub
//! (`nefor.log` as a function, matching the real mag plugin host —
//! `plugins/mag/src/kernel.rs`, `install_nefor`), point `package.path` at
//! the kernel directory so bare requires (`require("inventory")`,
//! `require("registry")`) resolve, then exec the test chunk (which
//! `error()`s on the first failed assertion).

use std::path::PathBuf;

use mlua::{Function, Lua, LuaSerdeExt, Value, Variadic};

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

#[test]
fn starter_mag_kernel_fold() {
    run_lua_test("tests/lua/mag-kernel/fold_test.lua");
}

#[test]
fn starter_mag_kernel_factory_contracts() {
    run_lua_test("tests/lua/mag-kernel/factory_test.lua");
}

#[test]
fn starter_mag_kernel_routing() {
    run_lua_test("tests/lua/mag-kernel/routing_test.lua");
}

#[test]
fn starter_mag_kernel_flow_primitives() {
    run_lua_test("tests/lua/mag-kernel/flow_test.lua");
}

#[test]
fn starter_mag_kernel_ready_barrier() {
    run_lua_test("tests/lua/mag-kernel/barrier_test.lua");
}

#[test]
fn starter_mag_kernel_observability() {
    run_lua_test("tests/lua/mag-kernel/observability_test.lua");
}

#[test]
fn starter_mag_kernel_llm_factory() {
    run_lua_test("tests/lua/mag-kernel/llm_test.lua");
}

#[test]
fn starter_mag_kernel_tool_primitives() {
    run_lua_test("tests/lua/mag-kernel/tools_test.lua");
}

#[test]
fn starter_mag_kernel_adapter_factory() {
    run_lua_test("tests/lua/mag-kernel/adapter_test.lua");
}

fn run_lua_test(rel_path: &str) {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let test_path = repo_root().join(rel_path);
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));

    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        panic!("{rel_path} failed:\n{e}");
    }
}

/// Install the minimal `nefor` global the mag kernel needs at load time:
/// `nefor.log` as a function (matching the plugin host), captured as a
/// no-op, plus `nefor.json.{encode, decode}` over serde_json — the same
/// surface the plugin host installs (`plugins/mag/src/kernel.rs`,
/// `install_json`), which the llm factory uses to serialize tool-call
/// arguments for its transcript. Kernel modules take their logger by
/// injection, so nothing else is required.
fn install_stub_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    let log: Function = lua.create_function(|_, _: Variadic<Value>| Ok(()))?;
    nefor.set("log", log)?;

    let json = lua.create_table()?;
    let encode = lua.create_function(|lua, value: Value| {
        let v: serde_json::Value = lua.from_value(value)?;
        serde_json::to_string(&v).map_err(|e| mlua::Error::runtime(format!("json.encode: {e}")))
    })?;
    json.set("encode", encode)?;
    let decode = lua.create_function(|lua, s: String| {
        let v: serde_json::Value = serde_json::from_str(&s)
            .map_err(|e| mlua::Error::runtime(format!("json.decode: {e}")))?;
        lua.to_value(&v)
    })?;
    json.set("decode", decode)?;
    nefor.set("json", json)?;

    lua.globals().set("nefor", nefor)?;
    Ok(())
}

fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let kernel_dir = repo_root().join("starter").join("mag-kernel");
    let kernel_dir = kernel_dir.display().to_string();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{kernel_dir}/?.lua",
          "{kernel_dir}/?/init.lua",
          package.path,
        }}, ";")
        "#,
        kernel_dir = kernel_dir,
    );
    lua.load(&script).exec()
}
