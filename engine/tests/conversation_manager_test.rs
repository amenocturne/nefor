//! Domain and runtime tests for the canonical conversation manager.

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

#[test]
fn conversation_manager_shared_service_contract() {
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

    let test_path = root.join("tests/lua/conversation-manager/service_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", test_path.display()));
    lua.load(&src)
        .set_name(test_path.display().to_string())
        .exec()
        .unwrap_or_else(|error| panic!("service_test.lua failed:\n{error}"));
}

#[test]
fn conversation_manager_runtime_contract() {
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

    let test_path = root.join("tests/lua/conversation-manager/runtime_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", test_path.display()));
    lua.load(&src)
        .set_name(test_path.display().to_string())
        .exec()
        .unwrap_or_else(|error| panic!("runtime_test.lua failed:\n{error}"));

    let init = std::fs::read_to_string(root.join("examples/nefor-agent/init.lua"))
        .expect("read starter init");
    let sessions_spawn = init
        .find("actor.spawn(sessions)")
        .expect("sessions actor spawn");
    let manager_service = init
        .find("local conversation_service = require(\"libs.conversation-manager.service\").new()")
        .expect("conversation manager shared service");
    let manager_spawn = init
        .find("actor.spawn(require(\"libs.conversation-manager.runtime\").build({")
        .expect("conversation manager actor spawn");
    let sessions_init = init
        .find("sessions.init(startup.session_id)")
        .expect("session initialization");
    assert!(
        sessions_spawn < manager_service
            && manager_service < manager_spawn
            && manager_spawn < sessions_init,
        "manager must subscribe before the initial session lifecycle event"
    );

    let cli_init =
        std::fs::read_to_string(root.join("cli-config/init.lua")).expect("read cli config init");
    let cli_sessions_spawn = cli_init
        .find("actor.spawn(sessions)")
        .expect("CLI sessions actor spawn");
    let cli_manager_service = cli_init
        .find("local conversation_service = require(\"libs.conversation-manager.service\").new()")
        .expect("CLI conversation manager shared service");
    let cli_manager_spawn = cli_init
        .find("actor.spawn(require(\"libs.conversation-manager.runtime\").build({")
        .expect("CLI conversation manager actor spawn");
    let cli_sessions_init = cli_init
        .find("sessions.init()")
        .expect("CLI session initialization");
    assert!(
        cli_sessions_spawn < cli_manager_service
            && cli_manager_service < cli_manager_spawn
            && cli_manager_spawn < cli_sessions_init,
        "CLI manager must subscribe before the initial session lifecycle event"
    );
}
