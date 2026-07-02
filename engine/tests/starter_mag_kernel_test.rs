//! Runs the mag-kernel fold Lua unit tests
//! (`tests/lua/mag-kernel/fold_test.lua`) in a bare Lua VM. Mirrors the
//! harness pattern in `starter_loop_counter_reasoner_test.rs`: install a
//! minimal `nefor` stub, point `package.path` at the kernel directory so
//! `require("inventory")` resolves, then exec the test chunk (which
//! `error()`s on the first failed assertion).

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
fn starter_mag_kernel_fold() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let test_path = repo_root().join("tests/lua/mag-kernel/fold_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));

    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        panic!("fold_test.lua failed:\n{e}");
    }
}

/// The kernel modules are pure Lua and take their logger by injection, so
/// the stub only needs `nefor.json` and a no-op `nefor.log` table for
/// parity with the other starter harnesses.
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
