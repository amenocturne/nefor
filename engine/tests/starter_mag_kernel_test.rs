//! Runs the mag-kernel Lua unit tests (`tests/lua/mag-kernel/*.lua`) in a
//! bare Lua VM. Mirrors the harness pattern in
//! `starter_loop_counter_reasoner_test.rs`: install a minimal `nefor` stub
//! (`nefor.log` as a function, matching the real mag plugin host —
//! `plugins/mag/src/kernel.rs`, `install_nefor`), point `package.path` at
//! the kernel directory so bare requires (`require("inventory")`,
//! `require("registry")`) resolve, then exec the test chunk (which
//! `error()`s on the first failed assertion).

use std::path::PathBuf;

use mlua::{Function, Lua, Value, Variadic};

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
/// no-op. Kernel modules take their logger by injection, so nothing else
/// is required.
fn install_stub_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    let log: Function = lua.create_function(|_, _: Variadic<Value>| Ok(()))?;
    nefor.set("log", log)?;
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
