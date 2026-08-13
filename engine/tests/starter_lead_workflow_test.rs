//! Unit tests for `examples/nefor-agent/lead-workflow/init.lua`. Mirrors the harness
//! pattern in `starter_agentic_workflow_test.rs` /
//! `starter_agent_reasoner_test.rs`: install a stub `nefor.*` surface,
//! then load the Lua test driver at `examples/nefor-agent/lead_workflow_test.lua`.

use std::path::PathBuf;

use mlua::{Function, Lua, Table, Value};

fn starter_dir() -> PathBuf {
    repo_root().join("examples/nefor-agent")
}

fn lua_dir() -> PathBuf {
    repo_root().join("lua")
}

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

#[test]
fn starter_lead_workflow_full() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let prev_data_dir = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let test_path = repo_root().join("tests/lua/lead-workflow/workflow_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));

    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        match prev_data_dir {
            Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
            None => std::env::remove_var("NEFOR_DATA_DIR"),
        }
        panic!("lead_workflow_test.lua failed:\n{e}");
    }

    match prev_data_dir {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn production_mag_eval_wrapper_compiles_direct_process_exec() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let build_source: Function = lua
        .load(
            r#"
            return require("libs.lead-workflow.mag-eval")._internals.build_source
            "#,
        )
        .eval()
        .expect("load production mag-eval wrapper");
    let expression = r#"(nefor.process.exec "step"
      (as nefor.contracts.ProcessExecParams
        {:argv ["printf" "ok"] :cwd nefor.process.cwd
         :timeout (nefor.contracts.no-timeout)}))"#;
    let (source, _): (String, Table) = build_source
        .call(expression)
        .expect("generate production mag-eval source");

    let inputs = serde_json::json!({
        "foreign_contracts": [
            {
                "identity": "nefor.factory.process-exec",
                "type_scheme": {
                    "input_tags": ["nefor.process.Input"],
                    "outputs": ["nefor.process.Result", "nefor.process.CapabilityFailed"]
                }
            },
            {
                "identity": "nefor.factory.source",
                "type_scheme": {
                    "input_tags": ["mag.Unit"],
                    "outputs": ["nefor.graph.Value"]
                }
            },
            {
                "identity": "nefor.factory.output",
                "type_scheme": {
                    "input_tags": ["nefor.graph.Value"],
                    "outputs": ["nefor.graph.Value"]
                }
            }
        ]
    });
    let module_root = starter_dir().join("mag/lib");
    let artifact = nefor_mag::compile_with_inputs_and_module_roots(
        &source,
        &module_root,
        inputs,
        std::slice::from_ref(&module_root),
    )
    .unwrap_or_else(|error| panic!("production mag-eval wrapper failed to compile: {error}"));

    assert_eq!(artifact.format, "nefor.graph-modification/v1");
}

/// The retired graph IR shape ({nodes, edges, terminal} + the popen compiler
/// bridge) must stay dead: the lead reads the modification off `mag.loaded`
/// replies (lua/libs/lead-workflow/init.lua resume_pending_load), never a
/// locally-compiled graph IR. Assert against the lib mechanism modules, not
/// the starter shims (which re-export them and would pass vacuously).
#[test]
fn lead_side_never_reads_the_retired_graph_ir_shape() {
    for rel in [
        "lua/libs/lead-workflow/init.lua",
        "lua/libs/mag-workspace/init.lua",
    ] {
        let path = repo_root().join(rel);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for needle in ["ir.nodes", "ir.edges", "ir.terminal"] {
            assert!(
                !src.contains(needle),
                "{rel} still reads the retired graph IR shape (`{needle}`)"
            );
        }
    }
    // The popen compiler bridge specifically: the `mag` CLI stays a dev tool
    // for humans; the lead's compile goes through the mag plugin.
    let mag_lua = repo_root().join("lua/libs/mag-workspace/init.lua");
    let src = std::fs::read_to_string(&mag_lua).expect("read lua/libs/mag-workspace/init.lua");
    assert!(
        !src.contains("io.popen"),
        "lua/libs/mag-workspace/init.lua still shells out to the mag CLI"
    );
}

fn install_stub_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;

    nefor::lua::bindings::install_json(lua, &nefor)?;

    // nefor.fs — lead-workflow's `compute_data_root` + plan-file mkdir
    // call into this binding. We snapshot NEFOR_DATA_DIR from the env
    // (the harness sets it per test if needed; otherwise the default
    // /var/empty path safely no-ops disk writes the test doesn't assert).
    let data_dir = std::env::var("NEFOR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/empty/lead-workflow-test"));
    nefor::lua::bindings::install_fs(lua, &nefor, nefor::paths::DataDir::new(data_dir))?;
    std::env::set_var("NEFOR_INSTALLATION_ID", "test-generation");

    let log_tbl = lua.create_table()?;
    let no_op: Function = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    log_tbl.set("info", no_op.clone())?;
    log_tbl.set("warn", no_op.clone())?;
    log_tbl.set("error", no_op.clone())?;
    log_tbl.set("debug", no_op.clone())?;
    nefor.set("log", log_tbl)?;

    let bus_tbl = lua.create_table()?;
    let bus_registry = lua.create_table()?;
    lua.globals().set("_bus_handlers", bus_registry)?;
    let on_event = lua.create_function(|lua, args: mlua::Variadic<Value>| {
        let kind = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            _ => {
                return Err(mlua::Error::runtime(
                    "stub bus.on_event: kind must be string",
                ));
            }
        };
        let handler: Function = match args.get(1) {
            Some(Value::Function(f)) => f.clone(),
            _ => {
                return Err(mlua::Error::runtime(
                    "stub bus.on_event: handler must be function",
                ));
            }
        };
        let registry: Table = lua.globals().get("_bus_handlers")?;
        let list: Table = match registry.get::<Value>(kind.as_str())? {
            Value::Table(t) => t,
            _ => {
                let t = lua.create_table()?;
                registry.set(kind.as_str(), t.clone())?;
                t
            }
        };
        let len = list.len()?;
        list.set(len + 1, handler)?;
        Ok(())
    })?;
    bus_tbl.set("on_event", on_event)?;
    nefor.set("bus", bus_tbl)?;

    let engine_tbl = lua.create_table()?;
    let calls_tbl = lua.create_table()?;
    lua.globals().set("_engine_calls", calls_tbl)?;
    let plugin_list = lua.create_table()?;
    lua.globals().set("_engine_plugins", plugin_list)?;
    let send_fn = lua.create_function(|lua, args: mlua::Variadic<Value>| {
        let payload = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            _ => return Ok(()),
        };
        let target = match args.get(1) {
            Some(Value::String(s)) => Some(s.to_str()?.to_owned()),
            _ => None,
        };
        let calls: Table = lua.globals().get("_engine_calls")?;
        let entry = lua.create_table()?;
        entry.set("payload", lua.create_string(&payload)?)?;
        match target {
            Some(t) => entry.set("target", lua.create_string(&t)?)?,
            None => entry.set("target", Value::Nil)?,
        }
        let n = calls.len()?;
        calls.set(n + 1, entry)?;
        Ok(())
    })?;
    engine_tbl.set("send", send_fn)?;
    let now_fn = lua.create_function(|_, _: ()| Ok("2026-05-08T00:00:00.000Z".to_owned()))?;
    engine_tbl.set("now", now_fn)?;
    let plugins_fn = lua.create_function(|lua, _: ()| {
        let arr: Table = match lua.globals().get::<Value>("_engine_plugins")? {
            Value::Table(t) => t,
            _ => lua.create_table()?,
        };
        Ok(arr)
    })?;
    engine_tbl.set("plugins", plugins_fn)?;
    let exit_fn = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    engine_tbl.set("exit", exit_fn)?;
    nefor.set("engine", engine_tbl)?;

    lua.globals().set("nefor", nefor)?;

    let test_tbl = lua.create_table()?;
    let test_calls = lua.create_function(|lua, _: ()| {
        let calls: Table = lua.globals().get("_engine_calls")?;
        Ok(calls)
    })?;
    test_tbl.set("calls", test_calls)?;
    let calls_clear = lua.create_function(|lua, _: ()| {
        let fresh = lua.create_table()?;
        lua.globals().set("_engine_calls", fresh)?;
        Ok(())
    })?;
    test_tbl.set("calls_clear", calls_clear)?;
    let set_plugins = lua.create_function(|lua, names: Table| {
        lua.globals().set("_engine_plugins", names)?;
        Ok(())
    })?;
    test_tbl.set("set_plugins", set_plugins)?;
    lua.globals().set("_test", test_tbl)?;
    Ok(())
}

fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let starter = starter_dir();
    let starter_str = starter.display().to_string();
    let lua_root = lua_dir();
    let lua_root_str = lua_root.display().to_string();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")
        "#,
        starter = starter_str,
        lua_root = lua_root_str,
    );
    lua.load(&script).exec()
}
