//! Pure domain tests for the inert conversation-manager foundation.

use std::path::PathBuf;

use mlua::Lua;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

#[test]
fn conversation_manager_domain_laws() {
    let lua = Lua::new();
    let nefor = lua.create_table().expect("create nefor table");
    nefor::lua::bindings::install_json(&lua, &nefor).expect("install json binding");
    lua.globals().set("nefor", nefor).expect("set nefor global");

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

    let test_path = root.join("tests/lua/conversation-manager/domain_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", test_path.display()));
    lua.load(&src)
        .set_name(test_path.display().to_string())
        .exec()
        .unwrap_or_else(|error| panic!("domain_test.lua failed:\n{error}"));
}
