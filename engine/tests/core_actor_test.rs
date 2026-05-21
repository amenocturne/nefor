//! Unit tests for `lua/core/actor.lua`'s `identity_spec` helper —
//! the generic identity-passthrough actor-spec factory used by every
//! plugin whose Rust binary speaks the canonical wire shape (basic-
//! tools, reasoner-graph, nefor-combinators).
//!
//! Drives the helper against a mock `nefor.engine` surface that
//! records every `send` / `deliver` call into a shared `_test.calls()`
//! buffer, then runs `tests/lua/core/actor_test.lua` for the assertions.
//! Test scripts live under repo-root `tests/lua/` rather than inside
//! the lib directories so the shipped libs are pure source.

use std::path::PathBuf;
use std::sync::Mutex;

use mlua::{Lua, Value};

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

fn lua_dir() -> PathBuf {
    repo_root().join("lua")
}

/// Shared call-recorder backing the `_test` Lua surface.
struct Shared {
    calls: Mutex<Vec<(Option<String>, String)>>,
}

fn install_mocks(lua: &Lua, shared: std::sync::Arc<Shared>) -> mlua::Result<()> {
    let nefor_tbl = lua.create_table()?;
    let engine_tbl = lua.create_table()?;

    let s1 = std::sync::Arc::clone(&shared);
    let send_fn = lua.create_function(move |_, args: mlua::Variadic<Value>| {
        let payload = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            other => {
                return Err(mlua::Error::runtime(format!(
                    "mock send: payload must be string; got {other:?}"
                )));
            }
        };
        let target = match args.get(1) {
            None | Some(Value::Nil) => None,
            Some(Value::String(s)) => Some(s.to_str()?.to_owned()),
            other => {
                return Err(mlua::Error::runtime(format!(
                    "mock send: target must be string or nil; got {other:?}"
                )));
            }
        };
        s1.calls.lock().unwrap().push((target, payload));
        Ok(())
    })?;
    engine_tbl.set("send", send_fn)?;

    let s2 = std::sync::Arc::clone(&shared);
    let deliver_fn = lua.create_function(move |_, (peer, payload): (String, String)| {
        s2.calls.lock().unwrap().push((Some(peer), payload));
        Ok(())
    })?;
    engine_tbl.set("deliver", deliver_fn)?;

    let now_fn = lua.create_function(|_, _: ()| Ok("2026-05-12T00:00:00.000Z".to_owned()))?;
    engine_tbl.set("now", now_fn)?;

    nefor_tbl.set("engine", engine_tbl)?;
    nefor::lua::bindings::install_json(lua, &nefor_tbl)?;

    let log_tbl = lua.create_table()?;
    let no_op = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    log_tbl.set("info", no_op.clone())?;
    log_tbl.set("warn", no_op.clone())?;
    log_tbl.set("error", no_op.clone())?;
    log_tbl.set("debug", no_op)?;
    nefor_tbl.set("log", log_tbl)?;

    lua.globals().set("nefor", nefor_tbl)?;

    // _test surface — calls() / calls_clear().
    let test_tbl = lua.create_table()?;

    let s3 = std::sync::Arc::clone(&shared);
    let calls_fn = lua.create_function(move |lua, _: ()| {
        let arr = lua.create_table()?;
        let snap = s3.calls.lock().unwrap().clone();
        for (i, (t, p)) in snap.into_iter().enumerate() {
            let entry = lua.create_table()?;
            match t {
                Some(s) => entry.set("target", lua.create_string(&s)?)?,
                None => entry.set("target", Value::Nil)?,
            }
            entry.set("payload", lua.create_string(&p)?)?;
            arr.set(i + 1, entry)?;
        }
        Ok(arr)
    })?;
    test_tbl.set("calls", calls_fn)?;

    let s4 = std::sync::Arc::clone(&shared);
    let clear_fn = lua.create_function(move |_, _: ()| {
        s4.calls.lock().unwrap().clear();
        Ok(())
    })?;
    test_tbl.set("calls_clear", clear_fn)?;

    lua.globals().set("_test", test_tbl)?;
    Ok(())
}

fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let lua_root = lua_dir().display().to_string();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")
        "#
    );
    lua.load(&script).exec()
}

#[test]
fn core_actor_identity_spec_tests() {
    let lua = Lua::new();
    let shared = std::sync::Arc::new(Shared {
        calls: Mutex::new(Vec::new()),
    });
    install_mocks(&lua, shared).expect("install mocks");
    set_package_path(&lua).expect("set package.path");

    let test_file = repo_root()
        .join("tests")
        .join("lua")
        .join("core")
        .join("actor_test.lua");
    let src = std::fs::read_to_string(&test_file)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_file.display()));
    if let Err(e) = lua
        .load(&src)
        .set_name(test_file.display().to_string())
        .exec()
    {
        panic!("actor_test.lua failed:\n{e}");
    }
}
