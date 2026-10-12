//! Rendering tests for `lua/libs/mag-context`. Assertions live in
//! `tests/lua/mag-context/`; this harness provides the engine JSON binding, a
//! scratch directory, and `package.path`.

use std::path::PathBuf;

use mlua::Lua;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

#[test]
fn public_modules_and_toolsets_render_from_configured_files() {
    let lua = Lua::new();
    let nefor = lua.create_table().expect("nefor table");
    nefor::lua::bindings::install_json(&lua, &nefor).expect("install json");
    lua.globals().set("nefor", nefor).expect("set nefor");
    let scratch = tempfile::tempdir().expect("scratch directory");
    lua.globals()
        .set("_scratch", scratch.path().display().to_string())
        .expect("set scratch");
    let lua_root = repo_root().join("lua").display().to_string();
    lua.load(format!(
        r#"package.path = "{lua_root}/?.lua;{lua_root}/?/init.lua;" .. package.path"#
    ))
    .exec()
    .expect("set package.path");
    let path = repo_root().join("tests/lua/mag-context/render_test.lua");
    let source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    if let Err(error) = lua
        .load(&source)
        .set_name(path.display().to_string())
        .exec()
    {
        panic!("render_test.lua failed:\n{error}");
    }
}
