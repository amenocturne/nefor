//! Unit tests for `examples/nefor-agent/tool-validator/init.lua` question forwarding and command classification.

use std::path::PathBuf;

use mlua::{Function, Lua, Table, Value};

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

#[test]
fn the_validator_forwards_questions_and_probes_its_classifier_once() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let test_path = repo_root().join("tests/lua/tool-validator/ask_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));

    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        panic!("tool_validator_ask_test.lua failed:\n{e}");
    }
}

#[test]
fn declared_da_version_is_verified_before_classification() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    let test_path = repo_root().join("tests/lua/tool-validator/version_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));
    lua.load(&src)
        .set_name(test_path.display().to_string())
        .exec()
        .expect("da version verification contract");
}

/// `classify_command` and argument-vector quoting against the fake classifier
/// executable beside the Lua test; `nefor.process.run` runs real processes.
#[test]
fn command_classification_per_policy_and_argv_quoting() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    install_real_process_run(&lua).expect("install process runner");
    set_package_path(&lua).expect("set package.path");
    let dir = repo_root().join("tests/lua/tool-validator");
    lua.globals()
        .set(
            "FAKE_CLASSIFIER",
            dir.join("fake-classifier.sh").display().to_string(),
        )
        .expect("set classifier path");
    let test_path = dir.join("classify_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));
    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        panic!("classify_test.lua failed:\n{e}");
    }
}

/// The starter's own classifier flags against a released `da` binary. Opt-in:
/// it runs only when NEFOR_TEST_REAL_DA names that binary, because the
/// repository's tests never download anything.
#[test]
fn the_starters_flags_classify_with_a_released_classifier() {
    let Some(real) = std::env::var_os("NEFOR_TEST_REAL_DA") else {
        eprintln!("skipped: NEFOR_TEST_REAL_DA is not set");
        return;
    };
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    install_real_process_run(&lua).expect("install process runner");
    set_package_path(&lua).expect("set package.path");
    lua.globals()
        .set("REAL_CLASSIFIER", real.to_string_lossy().to_string())
        .expect("set classifier path");
    let test_path = repo_root().join("tests/lua/tool-validator/released_classifier_test.lua");
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));
    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        panic!("released_classifier_test.lua failed:\n{e}");
    }
}

fn install_real_process_run(lua: &Lua) -> mlua::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let run = lua.create_function(|lua, opts: Table| {
        let cmd: String = opts.get("cmd")?;
        let args: Vec<String> = match opts.get::<Value>("args")? {
            Value::Table(list) => list.sequence_values::<String>().collect::<Result<_, _>>()?,
            _ => Vec::new(),
        };
        let stdin: Option<mlua::String> = opts.get("stdin")?;
        let result = lua.create_table()?;
        let spawned = Command::new(&cmd)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                result.set("code", -1)?;
                result.set("stdout", "")?;
                result.set("stderr", format!("cannot run {cmd}: {error}"))?;
                return Ok(result);
            }
        };
        if let Some(mut pipe) = child.stdin.take() {
            if let Some(text) = &stdin {
                pipe.write_all(&text.as_bytes())
                    .map_err(mlua::Error::external)?;
            }
        }
        let output = child.wait_with_output().map_err(mlua::Error::external)?;
        result.set("code", output.status.code().unwrap_or(-1))?;
        result.set("stdout", lua.create_string(&output.stdout)?)?;
        result.set("stderr", lua.create_string(&output.stderr)?)?;
        Ok(result)
    })?;
    let nefor: Table = lua.globals().get("nefor")?;
    let process: Table = nefor.get("process")?;
    process.set("run", run)
}

fn install_stub_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    nefor::lua::bindings::install_json(lua, &nefor)?;

    let log_tbl = lua.create_table()?;
    let no_op: Function = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    log_tbl.set("info", no_op.clone())?;
    log_tbl.set("warn", no_op.clone())?;
    log_tbl.set("error", no_op.clone())?;
    log_tbl.set("debug", no_op)?;
    nefor.set("log", log_tbl)?;

    let process_tbl = lua.create_table()?;
    let run_fn = lua.create_function(|lua, args: Value| {
        let (is_probe, stdin) = match args {
            Value::Table(t) => {
                let is_probe = match t.get::<Value>("args")? {
                    Value::Table(argv) => argv
                        .sequence_values::<String>()
                        .any(|v| matches!(v, Ok(s) if s == "--version")),
                    _ => false,
                };
                let stdin = match t.get::<Value>("stdin")? {
                    Value::String(s) => Some(s.to_str()?.to_owned()),
                    _ => None,
                };
                (is_probe, stdin)
            }
            _ => (false, None),
        };
        let t = lua.create_table()?;
        let code = if is_probe {
            0
        } else if stdin.as_deref().is_some_and(|s| s.contains("forbidden")) {
            2
        } else {
            1
        };
        t.set("code", code)?;
        Ok(t)
    })?;
    process_tbl.set("run", run_fn)?;
    nefor.set("process", process_tbl)?;

    let bus_tbl = lua.create_table()?;
    let on_event = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    bus_tbl.set("on_event", on_event)?;
    nefor.set("bus", bus_tbl)?;

    let engine_tbl = lua.create_table()?;
    let calls_tbl = lua.create_table()?;
    lua.globals().set("_engine_calls", calls_tbl)?;
    let send_fn = lua.create_function(|lua, args: mlua::Variadic<Value>| {
        let payload = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            _ => return Ok(()),
        };
        let calls: Table = lua.globals().get("_engine_calls")?;
        let entry = lua.create_table()?;
        entry.set("payload", lua.create_string(&payload)?)?;
        let n = calls.len()?;
        calls.set(n + 1, entry)?;
        Ok(())
    })?;
    engine_tbl.set("send", send_fn)?;
    let now_fn = lua.create_function(|_, _: ()| Ok("2026-05-08T00:00:00.000Z".to_owned()))?;
    engine_tbl.set("now", now_fn)?;
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
    lua.globals().set("_test", test_tbl)?;
    Ok(())
}

fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let root = repo_root();
    let starter = root.join("examples/nefor-agent").display().to_string();
    let lua_root = root.join("lua").display().to_string();
    let script = format!(
        r#"
        NEFOR_CONFIG_DIR = "{starter}"
        package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")
        "#,
    );
    lua.load(&script).exec()
}
