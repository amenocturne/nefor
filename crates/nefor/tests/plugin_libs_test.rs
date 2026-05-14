//! Unit tests for the per-plugin Lua libs that still ship Lua source.
//!
//! Post Phase-2 shape consolidation:
//!   * `basic-tools`, `nefor-combinators` no longer ship Lua libs —
//!     their actor-spec wiring is the generic identity passthrough in
//!     `core.actor.identity_spec`, exercised by `tests/lua/core/actor_test.lua`.
//!   * `reasoner-graph` ships only `spawn_graph.lua` (protocol
//!     primitives); the actor spec is built inline in starter via the
//!     identity helper.
//!   * `nefor-tui` keeps its widget primitives.
//!
//! Each surviving lib lives at `plugins/<plugin>/lua/<plugin>/...`
//! and ships alongside its Rust binary. The libs are pure-Lua: their
//! dependencies are `nefor.json` and `nefor.engine.{send, deliver,
//! now}`. We install a minimal mock engine that records `send` /
//! `deliver` calls, point `package.path` at `lua/` and
//! `plugins/<plugin>/lua/`, then load the per-plugin test script
//! which performs `assert`s.
//!
//! The harness mirrors `starter_ncp_test.rs`'s shape: shared call
//! recorder + `_test.calls()` / `_test.calls_clear()` Lua helpers.
//! Each test creates its own `Lua` VM so state doesn't leak across
//! tests.

use std::path::PathBuf;
use std::sync::Mutex;

use mlua::{Lua, Value};

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root is two levels above crates/nefor")
        .to_path_buf()
}

fn lua_dir() -> PathBuf {
    repo_root().join("lua")
}

fn plugin_lua_dir(plugin: &str) -> PathBuf {
    repo_root().join("plugins").join(plugin).join("lua")
}

/// Shared call-recorder backing the `_test` Lua surface.
struct Shared {
    calls: Mutex<Vec<(Option<String>, String)>>,
}

/// Install a mock `nefor.engine` + `nefor.json` + `_test` helper
/// surface.
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

    // Stub `nefor.log.*` — the libs themselves don't log, but loaded
    // dependencies (core.envelope, etc.) might.
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

/// Point `package.path` at `lua/` (for `require("core.*")` / `require("libs.*")`) and
/// the plugin's own lua dir.
///
/// Two layouts are supported:
///   - nested: `plugins/<plugin>/lua/<plugin>/init.lua` (resolved via
///     `<plugin_lua>/?/init.lua`)
///   - flat:   `plugins/<plugin>/lua/init.lua` (resolved by staging a
///     symlink `<staging>/<plugin>` -> `<plugin_lua>` and grafting
///     `<staging>/?/init.lua` + `<staging>/?.lua` onto package.path —
///     same shape pm.install lays out at runtime for dir overrides)
fn set_package_path(lua: &Lua, plugin: &str) -> mlua::Result<()> {
    let lua_root = lua_dir().display().to_string();
    let plugin_lua = plugin_lua_dir(plugin);
    let plugin_lua_str = plugin_lua.display().to_string();

    // Stage a symlink so flat-layout plugins (init.lua directly at lua/)
    // resolve as require("<plugin>") and require("<plugin>.sub") through
    // the standard <dir>/?/init.lua + <dir>/?.lua patterns.
    let staging = std::env::temp_dir().join(format!(
        "nefor-pm-test-staging-{}-{}",
        plugin,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).expect("staging dir");
    let link = staging.join(plugin);
    if !link.exists() {
        std::os::unix::fs::symlink(&plugin_lua, &link).expect("symlink");
    }
    let staging_str = staging.display().to_string();

    let script = format!(
        r#"
        package.path = table.concat({{
          "{plugin_lua_str}/?.lua",
          "{plugin_lua_str}/?/init.lua",
          "{staging_str}/?.lua",
          "{staging_str}/?/init.lua",
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")
        "#
    );
    lua.load(&script).exec()
}

/// Build a Lua VM, install mocks + paths, optionally run a per-test
/// setup snippet, then load and run the plugin's test file.
///
/// The Lua test script lives at repo-root `tests/lua/<plugin>/<file>`.
/// Test fixtures live outside the shipped lib dirs so the libs are pure
/// source. The mock-engine + package.path setup still routes
/// `require("<plugin>...")` into `plugins/<plugin>/lua/`, so the require
/// resolves from the test-file's new location without further wiring.
fn run_lua_test(plugin: &str, test_file: &str, setup_script: &str) {
    let lua = Lua::new();
    let shared = std::sync::Arc::new(Shared {
        calls: Mutex::new(Vec::new()),
    });
    install_mocks(&lua, shared).expect("install mocks");
    set_package_path(&lua, plugin).expect("set package.path");

    if !setup_script.is_empty() {
        lua.load(setup_script)
            .set_name("test_setup")
            .exec()
            .expect("setup script");
    }

    let path = repo_root()
        .join("tests")
        .join("lua")
        .join(plugin)
        .join(test_file);
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    if let Err(e) = lua.load(&src).set_name(path.display().to_string()).exec() {
        panic!("{test_file} failed:\n{e}");
    }
}

#[test]
fn nefor_tui_widget_tests() {
    run_lua_test("nefor-tui", "widget_test.lua", "");
}
