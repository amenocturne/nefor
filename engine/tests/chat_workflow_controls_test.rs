//! Pure transition-law tests for reusable chat workflow controls.

use std::path::PathBuf;

use mlua::Lua;

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

#[test]
fn workflow_controls_transition_laws() {
    let lua = Lua::new();
    let root = repo_root();
    let lua_root = root.join("lua").display().to_string();
    lua.load(format!(
        r#"package.path = table.concat({{
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")"#,
    ))
    .exec()
    .expect("set package.path");

    let test_path = root.join("tests/lua/chat/workflow_controls_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", test_path.display()));
    lua.load(&src)
        .set_name(test_path.display().to_string())
        .exec()
        .unwrap_or_else(|error| panic!("workflow_controls_test.lua failed:\n{error}"));
}

#[test]
fn queued_input_ownership_laws() {
    let lua = Lua::new();
    let root = repo_root();
    let lua_root = root.join("lua").display().to_string();
    lua.load(format!(
        r#"package.path = table.concat({{
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")"#,
    ))
    .exec()
    .expect("set package.path");

    let test_path = root.join("tests/lua/chat/queued_input_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", test_path.display()));
    lua.load(&src)
        .set_name(test_path.display().to_string())
        .exec()
        .unwrap_or_else(|error| panic!("queued_input_test.lua failed:\n{error}"));
}
