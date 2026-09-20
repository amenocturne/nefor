//! Runs the process-factory kernel Lua unit tests
//! (`tests/lua/mag-kernel/process_test.lua`) in a bare Lua VM — the same harness
//! shape as `engine/tests/starter_mag_kernel_test.rs`: a minimal `nefor` stub
//! (`nefor.log`, matching the plugin host's `install_nefor`), `package.path`
//! pointed at `plugins/mag/lua/mag-kernel/`, then exec the chunk (which `error()`s on
//! the first failed assertion). Hosted here rather than in the engine's
//! harness because the process factories ships with the mag plugin's kernel.

use std::path::PathBuf;

use mlua::{Function, Lua, LuaSerdeExt, Value, Variadic};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves")
}

#[test]
fn mag_kernel_process_factories() {
    let lua = Lua::new();

    // Install the host surfaces exercised by the factory contract tests.
    let nefor = lua.create_table().expect("nefor table");
    let log: Function = lua
        .create_function(|_, _: Variadic<Value>| Ok(()))
        .expect("log stub");
    nefor.set("log", log).expect("set log");
    let json = lua.create_table().expect("json table");
    let encode = lua
        .create_function(|lua, value: Value| {
            let value: serde_json::Value = lua.from_value(value)?;
            serde_json::to_string(&value).map_err(mlua::Error::external)
        })
        .expect("json.encode");
    json.set("encode", encode).expect("set json.encode");
    let decode = lua
        .create_function(|lua, source: String| {
            let value: serde_json::Value =
                serde_json::from_str(&source).map_err(mlua::Error::external)?;
            lua.to_value(&value)
        })
        .expect("json.decode");
    json.set("decode", decode).expect("set json.decode");
    nefor.set("json", json).expect("set json");
    lua.globals().set("nefor", nefor).expect("set nefor");

    // Bare requires resolve resolve
    // against the kernel directory.
    let kernel_dir = repo_root().join("plugins/mag/lua/mag-kernel");
    let package: mlua::Table = lua.globals().get("package").expect("package");
    let current: String = package.get("path").expect("package.path");
    package
        .set(
            "path",
            format!(
                "{k}/?.lua;{k}/?/init.lua;{lua}/?.lua;{lua}/?/init.lua;{current}",
                k = kernel_dir.display(),
                lua = repo_root().join("lua").display()
            ),
        )
        .expect("set package.path");

    let test_path = repo_root().join("tests/lua/mag-kernel/process_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));

    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        panic!("process_test.lua failed:\n{e}");
    }
}
