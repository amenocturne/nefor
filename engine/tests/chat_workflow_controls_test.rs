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

fn run_pure_chat_fixture(filename: &str) {
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

    let test_path = root.join("tests/lua/chat").join(filename);
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", test_path.display()));
    lua.load(&src)
        .set_name(test_path.display().to_string())
        .exec()
        .unwrap_or_else(|error| panic!("{filename} failed:\n{error}"));
}

#[test]
fn dispatch_registration_contract() {
    run_pure_chat_fixture("dispatch_test.lua");
}

#[test]
fn model_selection_correlation_laws() {
    run_pure_chat_fixture("model_selection_test.lua");
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
fn exit_controls_transition_laws() {
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

    let test_path = root.join("tests/lua/chat/exit_controls_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", test_path.display()));
    lua.load(&src)
        .set_name(test_path.display().to_string())
        .exec()
        .unwrap_or_else(|error| panic!("exit_controls_test.lua failed:\n{error}"));
}

#[test]
fn assistant_transcript_transition_laws() {
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

    let test_path = root.join("tests/lua/chat/transcript_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", test_path.display()));
    lua.load(&src)
        .set_name(test_path.display().to_string())
        .exec()
        .unwrap_or_else(|error| panic!("transcript_test.lua failed:\n{error}"));
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

#[test]
fn context_usage_projection_contract() {
    run_pure_chat_fixture("context_usage_test.lua");
}

#[test]
fn usage_reset_display_contract() {
    run_pure_chat_fixture("usage_test.lua");
}

#[test]
fn conversation_projection_surface_contract() {
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

    let test_path = root.join("tests/lua/chat/conversation_projection_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", test_path.display()));
    lua.load(&src)
        .set_name(test_path.display().to_string())
        .exec()
        .unwrap_or_else(|error| panic!("conversation_projection_test.lua failed:\n{error}"));
}

#[test]
fn semantic_projection_oracle_contract() {
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

    let test_path = root.join("tests/lua/chat/semantic_projection_oracle_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", test_path.display()));
    lua.load(&src)
        .set_name(test_path.display().to_string())
        .exec()
        .unwrap_or_else(|error| panic!("semantic projection oracle failed:\n{error}"));
}

#[test]
fn mag_runtime_projection_surface_contract() {
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

    let test_path = root.join("tests/lua/chat/mag_runtime_projection_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", test_path.display()));
    lua.load(&src)
        .set_name(test_path.display().to_string())
        .exec()
        .unwrap_or_else(|error| panic!("mag_runtime_projection_test.lua failed:\n{error}"));
}
