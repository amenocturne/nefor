//! Runs the process-factory kernel Lua unit tests
//! (`tests/lua/mag-kernel/process_test.lua`) in a bare Lua VM — the same harness
//! shape as `engine/tests/starter_mag_kernel_test.rs`: a minimal `nefor` stub
//! (`nefor.log`, matching the plugin host's `install_nefor`), `package.path`
//! pointed at `plugins/mag/lua/mag-kernel/`, then exec the chunk (which `error()`s on
//! the first failed assertion). Hosted here rather than in the engine's
//! harness because the process factories ships with the mag plugin's kernel.

use std::path::PathBuf;

use mlua::{Function, Lua, Value, Variadic};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves")
}

#[test]
fn mag_kernel_process_factories() {
    let lua = Lua::new();

    // Stub nefor.log (the only host surface the factory modules need).
    let nefor = lua.create_table().expect("nefor table");
    let log: Function = lua
        .create_function(|_, _: Variadic<Value>| Ok(()))
        .expect("log stub");
    nefor.set("log", log).expect("set log");
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
