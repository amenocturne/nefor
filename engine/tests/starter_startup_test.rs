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

fn run_lua_test(lua: &Lua, path: &std::path::Path) {
    let src =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    if let Err(e) = lua.load(&src).set_name(path.display().to_string()).exec() {
        panic!("{} failed:\n{e}", path.display());
    }
}

#[test]
fn starter_startup_parser_and_mode_application() {
    let lua = Lua::new();
    let root = repo_root();
    let starter = root.join("examples/nefor-agent").display().to_string();
    let lua_root = root.join("lua").display().to_string();
    let script = format!(
        r#"package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")"#,
    );
    lua.load(&script).exec().expect("set package.path");

    run_lua_test(&lua, &root.join("tests/lua/nefor-agent/startup_test.lua"));
    run_lua_test(
        &lua,
        &root.join("tests/lua/nefor-agent/startup_readiness_test.lua"),
    );

    let init = std::fs::read_to_string(root.join("examples/nefor-agent/init.lua"))
        .expect("read starter init");
    assert!(
        !init.contains("state-tracking") && !init.contains("state_tracking"),
        "starter composition must not import or spawn personal state tracking"
    );
    assert!(
        !root.join("lua/libs/state-tracking").exists(),
        "personal state-tracking library must not ship in the shared Lua tree"
    );

    let sessions = init
        .find("actor.spawn(sessions)")
        .expect("sessions actor registration");
    let conversation = init
        .find("actor.spawn(require(\"libs.conversation-manager.runtime\").build({")
        .expect("conversation manager actor registration");
    let session_init = init
        .find("sessions.init(startup.session_id)")
        .expect("session initialization");
    let agentic_loop = init
        .find("actor.spawn(agentic_loop)")
        .expect("agentic loop registration");
    let lead_workflow = init
        .find("actor.spawn(require(\"libs.lead-workflow\"))")
        .expect("lead workflow registration");
    let read_only_tools = init
        .find("actor.spawn(require(\"read-only-tools\"))")
        .expect("read-only tools registration");
    let tool_validator = init
        .find("actor.spawn(require(\"tool-validator\"))")
        .expect("tool validator registration");
    let gate = init
        .find(r#"actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv))"#)
        .expect("tool gate registration");
    let basic_tools = init
        .find("actor.spawn(tools.basic_actor_spec())")
        .expect("basic tools registration");
    let mode = init
        .find("startup_args.apply_mode(startup, agentic_loop)")
        .expect("startup mode application");
    let chat = init
        .find("actor.spawn(require(\"libs.compositors.chat_bridge\").spawn_spec({")
        .expect("chat surface registration");
    let prompt = init
        .find("if startup.prompt ~= nil then")
        .expect("startup prompt readiness barrier");

    assert!(sessions < conversation && conversation < session_init);
    assert!(session_init < agentic_loop && agentic_loop < lead_workflow);
    assert!(lead_workflow < read_only_tools && read_only_tools < tool_validator);
    assert!(tool_validator < gate && gate < basic_tools && basic_tools < mode);
    assert!(mode < chat && chat < prompt);
}
