//! Unit tests for `starter/ncp.lua` driven from Rust.
//!
//! The Lua module under test (`starter/ncp.lua`) depends only on the
//! `nefor.engine` surface — specifically `nefor.engine.send` and
//! `nefor.engine.plugins`. This harness installs a mock `nefor.engine` that
//! records calls + returns a caller-controlled plugin list, plus a `_test`
//! helper global for the Lua tests to drive it. It then loads and runs
//! `starter/ncp_test.lua`, which performs its own `assert`s and errors out
//! on failure.
//!
//! Running the `ncp_test.lua` file from Rust (rather than a dedicated Lua
//! CLI) keeps the tests inside `cargo test` and avoids a separate toolchain
//! dependency.

use std::path::PathBuf;
use std::sync::Mutex;

use mlua::{Lua, Value};

/// Resolve `<repo-root>/starter/`. `CARGO_MANIFEST_DIR` points at the
/// engine crate (`crates/nefor`), so we walk up two levels.
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

#[test]
fn starter_ncp_unit_tests() {
    let lua = Lua::new();
    install_mock_engine_and_test_helpers(&lua).expect("install mocks");
    set_package_path(&lua).expect("set package.path");

    let ncp_test = lua_dir().join("core").join("ncp_test.lua");
    let src = std::fs::read_to_string(&ncp_test)
        .unwrap_or_else(|e| panic!("read {}: {e}", ncp_test.display()));

    if let Err(e) = lua
        .load(&src)
        .set_name(ncp_test.display().to_string())
        .exec()
    {
        panic!("ncp_test.lua failed:\n{e}");
    }
}

/// Install a fake `nefor.engine` that records `send` and `deliver` calls
/// and reads its plugin list from a `_test`-owned slot. Also installs the
/// `_test` global the Lua tests use to reset state and inspect recorded
/// calls.
///
/// Post-refactor the mock distinguishes between calls (send + deliver)
/// for assertion convenience but also tracks **bus-log entries** —
/// `nefor.engine.send` appends a Step entry to a synthesized log so tests
/// can pass that log to `M.dispatch(log)` and exercise the wrapper
/// `to_plugin` fan-out. `_test.bus_log()` returns the accumulated log.
fn install_mock_engine_and_test_helpers(lua: &Lua) -> mlua::Result<()> {
    struct Shared {
        // Recorded calls (both send and deliver). Same list shape as the
        // pre-refactor harness so existing assertions keep working.
        calls: Mutex<Vec<(Option<String>, String)>>,
        // Synthesized bus log — append-only, mirrors what the production
        // broker would build. Each `send` adds an entry; `deliver` does
        // not.
        bus_log: Mutex<Vec<BusEntry>>,
        plugins: Mutex<Vec<String>>,
    }
    #[derive(Clone)]
    struct BusEntry {
        origin: String,
        target: Option<String>,
        payload: String,
    }
    let shared = std::sync::Arc::new(Shared {
        calls: Mutex::new(Vec::new()),
        bus_log: Mutex::new(Vec::new()),
        plugins: Mutex::new(Vec::new()),
    });

    // nefor.engine.send(payload, target?) — record the call. Matches the
    // real binding's signature modulo the fact we don't validate the target
    // against PluginName (the tests pass well-formed names).
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
        s1.calls
            .lock()
            .unwrap()
            .push((target.clone(), payload.clone()));
        s1.bus_log.lock().unwrap().push(BusEntry {
            origin: "step".into(),
            target,
            payload,
        });
        Ok(())
    })?;
    engine_tbl.set("send", send_fn)?;

    // nefor.engine.deliver(peer, payload) — mock surface for the
    // per-peer fan-out path. From the Lua test's point of view a
    // delivery looks identical to a targeted send (same recorded
    // shape in `_test.calls()`); the broker-side distinction (no
    // LogEntry append) is only visible in the production binding +
    // the broker tests in src/ncp/broker.rs. Recording deliveries as
    // ordinary calls keeps the existing assertions on `targets[<peer>]`
    // unchanged after the send → deliver split.
    let s_deliver = std::sync::Arc::clone(&shared);
    let deliver_fn = lua.create_function(move |_, args: mlua::Variadic<Value>| {
        let peer = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            other => {
                return Err(mlua::Error::runtime(format!(
                    "mock deliver: peer must be string; got {other:?}"
                )));
            }
        };
        let payload = match args.get(1) {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            other => {
                return Err(mlua::Error::runtime(format!(
                    "mock deliver: payload must be string; got {other:?}"
                )));
            }
        };
        s_deliver.calls.lock().unwrap().push((Some(peer), payload));
        Ok(())
    })?;
    engine_tbl.set("deliver", deliver_fn)?;

    let s2 = std::sync::Arc::clone(&shared);
    let plugins_fn = lua.create_function(move |lua, _: ()| {
        let names = s2.plugins.lock().unwrap().clone();
        let arr = lua.create_table()?;
        for (i, n) in names.into_iter().enumerate() {
            arr.set(i + 1, lua.create_string(&n)?)?;
        }
        Ok(arr)
    })?;
    engine_tbl.set("plugins", plugins_fn)?;

    // nefor.engine.now() — return a fixed ISO-8601 string. Tests don't
    // inspect timestamp values; a stable dummy is enough.
    let now_fn = lua.create_function(|_, _: ()| Ok("2026-04-23T00:00:00.000Z".to_owned()))?;
    engine_tbl.set("now", now_fn)?;

    nefor_tbl.set("engine", engine_tbl)?;
    nefor::lua::bindings::install_json(lua, &nefor_tbl)?;

    // Stub `nefor.log` so the starter modules' `nefor.log.info(...)`
    // diagnostic calls don't blow up at test time. Tests don't inspect
    // log output; a no-op accepting any arguments is enough.
    let log_tbl = lua.create_table()?;
    let no_op = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    log_tbl.set("info", no_op.clone())?;
    log_tbl.set("warn", no_op.clone())?;
    log_tbl.set("error", no_op.clone())?;
    log_tbl.set("debug", no_op)?;
    nefor_tbl.set("log", log_tbl)?;

    lua.globals().set("nefor", nefor_tbl)?;

    // _test — the Lua-side control surface.
    let test_tbl = lua.create_table()?;

    // _test.calls() -> array of { target = string|nil, payload = string }
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

    // _test.calls_clear() — drop the recorded history (calls only;
    // bus_log is preserved so dispatch tests can drive accumulated log).
    let s4 = std::sync::Arc::clone(&shared);
    let clear_fn = lua.create_function(move |_, _: ()| {
        s4.calls.lock().unwrap().clear();
        Ok(())
    })?;
    test_tbl.set("calls_clear", clear_fn)?;

    // _test.bus_log() — return the synthesized bus log as an array of
    // log-entry tables matching the shape the broker passes to dispatch:
    //   { ts, origin, target, payload }
    let s_log = std::sync::Arc::clone(&shared);
    let bus_log_fn = lua.create_function(move |lua, _: ()| {
        let arr = lua.create_table()?;
        let snap = s_log.bus_log.lock().unwrap().clone();
        for (i, entry) in snap.into_iter().enumerate() {
            let row = lua.create_table()?;
            row.set("ts", lua.create_string("2026-04-23T00:00:00.000Z")?)?;
            row.set("origin", lua.create_string(&entry.origin)?)?;
            match entry.target {
                Some(t) => row.set("target", lua.create_string(&t)?)?,
                None => row.set("target", Value::Nil)?,
            }
            row.set("payload", lua.create_string(&entry.payload)?)?;
            arr.set(i + 1, row)?;
        }
        Ok(arr)
    })?;
    test_tbl.set("bus_log", bus_log_fn)?;

    // _test.bus_log_clear() — wipe the synthesized bus log.
    let s_log_clear = std::sync::Arc::clone(&shared);
    let bus_log_clear_fn = lua.create_function(move |_, _: ()| {
        s_log_clear.bus_log.lock().unwrap().clear();
        Ok(())
    })?;
    test_tbl.set("bus_log_clear", bus_log_clear_fn)?;

    // _test.set_plugins({ "a", "b" }) — override the plugin list that
    // nefor.engine.plugins() returns.
    let s5 = std::sync::Arc::clone(&shared);
    let set_plugins_fn = lua.create_function(move |_, tbl: mlua::Table| {
        let mut out = Vec::new();
        for pair in tbl.pairs::<i64, String>() {
            let (_i, v) = pair?;
            out.push(v);
        }
        *s5.plugins.lock().unwrap() = out;
        Ok(())
    })?;
    test_tbl.set("set_plugins", set_plugins_fn)?;

    lua.globals().set("_test", test_tbl)?;
    Ok(())
}

fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let starter = starter_dir();
    let starter_str = starter.display().to_string();
    let lua_root = lua_dir();
    let lua_root_str = lua_root.display().to_string();
    let op_plugin_lua = repo_root()
        .join("plugins")
        .join("openai-provider")
        .join("lua");
    let op_plugin_lua_str = op_plugin_lua.display().to_string();
    let tg_plugin_lua = repo_root().join("plugins").join("tool-gate").join("lua");
    let tg_plugin_lua_str = tg_plugin_lua.display().to_string();
    let rg_plugin_lua = repo_root().join("plugins").join("reasoner-graph").join("lua");
    let rg_plugin_lua_str = rg_plugin_lua.display().to_string();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          "{op_plugin_lua}/?.lua",
          "{op_plugin_lua}/?/init.lua",
          "{tg_plugin_lua}/?.lua",
          "{tg_plugin_lua}/?/init.lua",
          "{rg_plugin_lua}/?.lua",
          "{rg_plugin_lua}/?/init.lua",
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")
        -- starter Lua code resolves sibling files relative to
        -- NEFOR_CONFIG_DIR; tests must set it for parity with engine
        -- startup.
        NEFOR_CONFIG_DIR = "{starter}"
        "#,
        starter = starter_str,
        lua_root = lua_root_str,
        op_plugin_lua = op_plugin_lua_str,
        tg_plugin_lua = tg_plugin_lua_str,
        rg_plugin_lua = rg_plugin_lua_str,
    );
    lua.load(&script).exec()
}
