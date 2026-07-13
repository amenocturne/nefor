//! Unit tests for the starter-owned interactive startup parser.

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
fn starter_startup_parser_and_mode_application() {
    let lua = Lua::new();
    let root = repo_root();
    let starter = root.join("starter").display().to_string();
    let script = format!(
        r#"package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          package.path,
        }}, ";")"#,
    );
    lua.load(&script).exec().expect("set package.path");

    let test_path = root.join("tests/lua/starter/startup_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));
    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        panic!("starter_startup_test.lua failed:\n{e}");
    }

    let init = std::fs::read_to_string(root.join("starter/init.lua")).expect("read starter init");
    let gate = init
        .find(r#"actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv))"#)
        .expect("tool gate registration");
    let basic_tools = init
        .find("actor.spawn(tools.basic_actor_spec())")
        .expect("basic tools registration");
    let mode = init
        .find("startup_args.apply_mode(startup, agentic_loop)")
        .expect("startup mode application");
    let prompt = init
        .find("if startup.prompt ~= nil then")
        .expect("startup prompt submission");
    assert!(gate < basic_tools && basic_tools < mode && mode < prompt);
}
