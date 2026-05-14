//! Smoke tests for `starter/lead_role.lua`. The loader has no bus
//! dependency — it just reads `prompts/<role>.md` files off disk and
//! exposes the contents through three module-level tables. The Rust
//! side here only needs to set `package.path` and run the Lua test
//! file; no bus or engine stubbing required.

use std::path::PathBuf;

use mlua::Lua;

fn starter_dir() -> PathBuf {
    repo_root().join("starter")
}

fn lua_dir() -> PathBuf {
    repo_root().join("lua")
}

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root is two levels above crates/nefor")
        .to_path_buf()
}

fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let starter = starter_dir();
    let starter_str = starter.display().to_string();
    let lua_root = lua_dir();
    let lua_root_str = lua_root.display().to_string();
    let rg_plugin_lua = repo_root().join("plugins").join("reasoner-graph").join("lua");
    let rg_plugin_lua_str = rg_plugin_lua.display().to_string();
    let script = format!(
        r#"
        NEFOR_CONFIG_DIR = "{starter}"
        package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          "{rg_plugin_lua}/?.lua",
          "{rg_plugin_lua}/?/init.lua",
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")
        "#,
        starter = starter_str,
        lua_root = lua_root_str,
        rg_plugin_lua = rg_plugin_lua_str,
    );
    lua.load(&script).exec()
}

#[test]
fn starter_lead_role_smoke() {
    let lua = Lua::new();
    set_package_path(&lua).expect("set package.path");

    let test_path = repo_root().join("tests/lua/lead-workflow/role_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));

    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        panic!("lead_role_test.lua failed:\n{e}");
    }
}
