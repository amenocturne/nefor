//! Unit tests for `plugins/tool-gate/lua/tool-gate/init.lua` — the
//! pure-primitive plugin lib extracted from `examples/nefor-agent/tool-gate/init.lua`.
//!
//! These tests drive each exported primitive directly under a stubbed
//! `nefor.*` surface. The lib never reaches into `agentic-loop` (the
//! starter wrapper owns that coupling), so no agentic-loop stub is
//! needed — the tests cover translation, body construction, output
//! shaping, and the AGENTS.md side-effect bridge in isolation.
//!
//! The wider integration shape — dump-swap + AGENTS.md emission driven
//! through `from_plugin` / `to_plugin` — is already covered by
//! `starter_tool_gate_test.rs`. That suite continues to run against
//! the rewritten composition file, so behaviour equivalence falls out
//! of it.

use std::path::PathBuf;
use std::sync::Mutex;

use mlua::{Function, Lua, Table, Value};

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

fn plugin_lib_dir() -> PathBuf {
    repo_root().join("plugins").join("tool-gate").join("lua")
}

/// Package path: plugins/tool-gate/lua/ FIRST so `require("tool-gate")`
/// resolves to the plugin lib (not the starter composition file). Then
/// lua/ for core.* and libs.*.
fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let plugin = plugin_lib_dir();
    let lua_root = lua_dir();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{plugin}/?.lua",
          "{plugin}/?/init.lua",
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")
        "#,
        plugin = plugin.display(),
        lua_root = lua_root.display(),
    );
    lua.load(&script).exec()
}

fn install_stub_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    nefor::lua::bindings::install_json(lua, &nefor)?;

    // nefor.fs — real binding, snapshotting NEFOR_DATA_DIR from the env
    // (test sets it before calling install_stub_nefor).
    let data_dir = std::env::var("NEFOR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/empty/nefor-test-data"));
    nefor::lua::bindings::install_fs(lua, &nefor, nefor::paths::DataDir::new(data_dir))?;

    let log_tbl = lua.create_table()?;
    let no_op: Function = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    log_tbl.set("info", no_op.clone())?;
    log_tbl.set("warn", no_op.clone())?;
    log_tbl.set("error", no_op.clone())?;
    log_tbl.set("debug", no_op)?;
    nefor.set("log", log_tbl)?;

    let engine_tbl = lua.create_table()?;
    let send_fn = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    engine_tbl.set("send", send_fn)?;
    let deliver_fn = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    engine_tbl.set("deliver", deliver_fn)?;
    let now_fn = lua.create_function(|_, _: ()| Ok("2026-05-12T00:00:00.000Z".to_owned()))?;
    engine_tbl.set("now", now_fn)?;
    let plugins_fn = lua.create_function(|lua, _: ()| {
        let arr: Table = lua.create_table()?;
        Ok(arr)
    })?;
    engine_tbl.set("plugins", plugins_fn)?;
    nefor.set("engine", engine_tbl)?;

    lua.globals().set("nefor", nefor)?;
    Ok(())
}

/// Variant that captures every `engine.send(payload, target)` call into
/// `_send_trace` so emit-by-side-effect tests can assert what the lib
/// published.
fn install_stub_nefor_with_send_recorder(lua: &Lua) -> mlua::Result<()> {
    install_stub_nefor(lua)?;
    lua.load(
        r#"
        _send_trace = {}
        nefor.engine.send = function(payload, target)
            _send_trace[#_send_trace + 1] = { payload = payload, target = target }
        end
        "#,
    )
    .exec()?;
    Ok(())
}

// ----------------------------------------------------------------
// translator() — kind constants + envelope predicates
// ----------------------------------------------------------------

#[test]
fn mag_run_binding_registry_rejects_forgery_mismatch_and_conflicting_replay() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let result: Table = lua
        .load(
            r#"
            local registry = require("libs.mag-run-bindings").new()
            local binding = {
              kind = "mag.run_started", session_id = "session-1", run_id = "run-1",
              scope = "r1", principal = "subagent",
            }
            local valid = {
              session_id = "session-1", run_id = "run-1", run_scope = "r1",
              actor_id = "scout.run-tool", capability_id = "r1/cap-1",
              principal = "subagent",
            }
            local direct_rejected = not registry.bind(binding, "direct-tool")
            local accepted = registry.bind(binding, "mag")
            local exact_replay = registry.bind(binding, "mag")
            local valid_notice = registry.validate(valid, "session-1")
            local failures = {}
            for field, value in pairs({
              session_id = "session-2", run_id = "run-2", run_scope = "r2",
              principal = "lead",
            }) do
              local forged = {}
              for k, v in pairs(valid) do forged[k] = v end
              forged[field] = value
              failures[field] = not registry.validate(forged, "session-1")
            end
            local conflict_rejected = not registry.bind({
              session_id = "session-1", run_id = "run-1", scope = "r1",
              principal = "lead",
            }, "mag")
            local poisoned = not registry.validate(valid, "session-1")
            return {
              direct_rejected = direct_rejected, accepted = accepted,
              exact_replay = exact_replay, valid_notice = valid_notice,
              failures = failures, conflict_rejected = conflict_rejected,
              poisoned = poisoned,
            }
            "#,
        )
        .eval()
        .expect("registry result");

    for field in [
        "direct_rejected",
        "accepted",
        "exact_replay",
        "valid_notice",
        "conflict_rejected",
        "poisoned",
    ] {
        assert!(result.get::<bool>(field).unwrap(), "{field}");
    }
    let failures: Table = result.get("failures").unwrap();
    for field in ["session_id", "run_id", "run_scope", "principal"] {
        assert!(failures.get::<bool>(field).unwrap(), "forged {field}");
    }
}

#[test]
fn translator_exposes_canonical_kinds_for_gate_name() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let kinds: Table = lua
        .load(
            r#"
            local lib = require("tool-gate")
            local t = lib.translator("tool-gate")
            return t.kinds
            "#,
        )
        .eval()
        .expect("kinds");

    let hello: String = kinds.get("hello").expect("hello");
    let outbound_tool_invoke: String = kinds.get("outbound_tool_invoke").expect("oti");
    let tool_result: String = kinds.get("tool_result").expect("tr");
    let tool_advertise: String = kinds.get("tool_advertise").expect("ta");

    assert_eq!(hello, "tool-gate.hello");
    assert_eq!(outbound_tool_invoke, "tool-gate.tool.invoke");
    assert_eq!(tool_result, "tool.result");
    assert_eq!(tool_advertise, "tool-gate.tools.advertise");
}

#[test]
fn translator_predicates_match_envelope_shape() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let results: Table = lua
        .load(
            r#"
            local lib = require("tool-gate")
            local t = lib.translator("tool-gate")

            local hello_env = { type = "event", from = "tool-gate",
              body = { kind = "tool-gate.hello" } }
            local tr_env = { type = "event", from = "tool-gate",
              body = { kind = "tool.result", id = "c" } }
            local oti_env = { type = "event", from = "agentic-loop",
              body = { kind = "tool-gate.tool.invoke", id = "i", name = "read_file" } }
            local other_env = { type = "event", from = "tool-gate",
              body = { kind = "tool.other" } }
            local nonevent = { type = "ack", from = "x", body = {} }

            return {
              hello_yes        = t.is_hello(hello_env),
              hello_no_other   = t.is_hello(other_env),
              tr_yes           = t.is_tool_result(tr_env),
              tr_no            = t.is_tool_result(hello_env),
              oti_yes          = t.is_outbound_tool_invoke(oti_env),
              oti_no           = t.is_outbound_tool_invoke(other_env),
              nonevent_rejects = t.is_hello(nonevent),
            }
            "#,
        )
        .eval()
        .expect("eval");

    let truthy: Vec<&str> = vec!["hello_yes", "tr_yes", "oti_yes"];
    let falsy: Vec<&str> = vec!["hello_no_other", "tr_no", "oti_no", "nonevent_rejects"];
    for k in &truthy {
        let v: bool = results.get(*k).expect(k);
        assert!(v, "{k} should be true");
    }
    for k in &falsy {
        let v: bool = results.get(*k).expect(k);
        assert!(!v, "{k} should be false");
    }
}

#[test]
fn translator_publish_routes_through_engine_send_with_gate_identity() {
    let lua = Lua::new();
    install_stub_nefor_with_send_recorder(&lua).expect("stub");
    set_package_path(&lua).expect("package.path");

    lua.load(
        r#"
        local lib = require("tool-gate")
        local t = lib.translator("tool-gate")
        t.publish({ kind = "tool-gate.hello" }, nil)
        t.publish({ kind = "tool-gate.tools.advertise" }, "nefor-tui")
        "#,
    )
    .exec()
    .expect("publish");

    let trace: Table = lua.globals().get("_send_trace").expect("trace");
    let entry_count = trace.len().expect("len");
    assert_eq!(entry_count, 2);

    // Both publish calls must stamp `from = "tool-gate"` (the gate
    // identity). The lib's contract is that publish() is the gate's
    // mouthpiece — anything emitted under a different `from` requires
    // emit_as(), used only for the closing tool-executor tool.result.
    for i in 1..=entry_count {
        let entry: Table = trace.get(i).expect("entry");
        let payload: String = entry.get("payload").expect("payload");
        assert!(
            payload.contains("\"from\":\"tool-gate\""),
            "publish entry {i} must carry gate identity: {payload}"
        );
    }
}

// ----------------------------------------------------------------
// tool_result_payload — chat-side rendering rules
// ----------------------------------------------------------------

#[test]
fn tool_result_payload_passes_through_normal_output() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let result: Table = lua
        .load(
            r#"
            local lib = require("tool-gate")
            local out, err = lib.tool_result_payload({
              kind = "tool.result", id = "x", output = "hello world",
            })
            return { out = out, err = err }
            "#,
        )
        .eval()
        .expect("eval");

    let out: String = result.get("out").expect("out");
    let err: bool = result.get("err").expect("err");
    assert_eq!(out, "hello world");
    assert!(!err);
}

#[test]
fn tool_result_payload_extracts_text_table_output() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let result: Table = lua
        .load(
            r#"
            local lib = require("tool-gate")
            local out, err = lib.tool_result_payload({
              kind = "tool.result", id = "x",
              output = { text = "hello from a read-only tool" },
            })
            return { out = out, err = err }
            "#,
        )
        .eval()
        .expect("eval");

    let out: String = result.get("out").expect("out");
    let err: bool = result.get("err").expect("err");
    assert_eq!(out, "hello from a read-only tool");
    assert!(!err);
}

#[test]
fn tool_result_payload_lifts_error_string_into_output_when_output_missing() {
    // Mirrors the bug fix in starter_tool_gate that drove the lift:
    // a denied tool comes back with { error = "denied by gate policy" }
    // and no output, and the chat-side payload must surface the error
    // string so the tool block renders the WHY of the denial.
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let result: Table = lua
        .load(
            r#"
            local lib = require("tool-gate")
            local out, err = lib.tool_result_payload({
              kind = "tool.result", id = "x",
              error = "denied by gate policy",
            })
            return { out = out, err = err }
            "#,
        )
        .eval()
        .expect("eval");

    let out: String = result.get("out").expect("out");
    let err: bool = result.get("err").expect("err");
    assert_eq!(out, "denied by gate policy");
    assert!(err, "error string should set err_bool");
}

#[test]
fn tool_result_payload_treats_bool_error_as_err_with_empty_output() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let result: Table = lua
        .load(
            r#"
            local lib = require("tool-gate")
            local out, err = lib.tool_result_payload({
              kind = "tool.result", id = "x", error = true,
            })
            return { out = out, err = err }
            "#,
        )
        .eval()
        .expect("eval");

    let out: String = result.get("out").expect("out");
    let err: bool = result.get("err").expect("err");
    assert_eq!(out, "");
    assert!(err);
}

#[test]
fn tool_result_payload_summarizes_image_media_output() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let result: Table = lua
        .load(
            r#"
            local lib = require("tool-gate")
            local out, err = lib.tool_result_payload({
              kind = "tool.result",
              id = "x",
              output = {
                type = "media",
                media_type = "image/png",
                filename = "paste.png",
                data = "abc",
              },
            })
            return { out = out, err = err }
            "#,
        )
        .eval()
        .expect("eval");

    let out: String = result.get("out").expect("out");
    let err: bool = result.get("err").expect("err");
    assert_eq!(out, "[image result: paste.png (image/png)]");
    assert!(!err);
}

#[test]
fn tool_result_payload_summarizes_image_media_json_string_output() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let result: Table = lua
        .load(
            r#"
            local lib = require("tool-gate")
            local out, err = lib.tool_result_payload({
              kind = "tool.result",
              id = "x",
              output = nefor.json.encode({
                type = "media",
                media_type = "image/png",
                filename = "paste.png",
                data = string.rep("a", 64 * 1024),
              }),
            })
            return { out = out, err = err }
            "#,
        )
        .eval()
        .expect("eval");

    let out: String = result.get("out").expect("out");
    let err: bool = result.get("err").expect("err");
    assert_eq!(out, "[image result: paste.png (image/png)]");
    assert!(!err);
}

#[test]
fn image_media_output_is_not_dumped() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let should_dump: bool = lua
        .load(
            r#"
            local d = require("tool-gate.tool_output_dump")
            return d.should_dump({
              type = "media",
              media_type = "image/jpeg",
              filename = "paste.jpg",
              data = string.rep("a", 64 * 1024),
            })
            "#,
        )
        .eval()
        .expect("eval");

    assert!(!should_dump);
}

#[test]
fn image_media_json_string_output_is_not_dumped() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let should_dump: bool = lua
        .load(
            r#"
            local d = require("tool-gate.tool_output_dump")
            return d.should_dump(nefor.json.encode({
              type = "media",
              media_type = "image/jpeg",
              filename = "paste.jpg",
              data = string.rep("a", 64 * 1024),
            }))
            "#,
        )
        .eval()
        .expect("eval");

    assert!(!should_dump);
}

// ----------------------------------------------------------------
// maybe_dump_output — disk-write side-effect bridge
// ----------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn maybe_dump_output_passes_through_small_output_unchanged() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let result: Table = lua
        .load(
            r#"
            local lib = require("tool-gate")
            local body = { kind = "tool.result", id = "call-1", output = "small" }
            local rewritten, path = lib.maybe_dump_output(body, "chat-1")
            return { same = rewritten == body, output = rewritten.output, path = path }
            "#,
        )
        .eval()
        .expect("eval");

    let same: bool = result.get("same").expect("same");
    let output: String = result.get("output").expect("output");
    let path: Option<String> = result.get("path").expect("path");
    // Below budget: identical table returned, no path, no disk write.
    assert!(same, "small output should not be copied — same table back");
    assert_eq!(output, "small");
    assert!(path.is_none());

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn maybe_dump_output_rewrites_huge_output_into_summary_and_writes_disk() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let result: Table = lua
        .load(
            r#"
            local lib = require("tool-gate")
            local big = string.rep("PAYLOAD\n", 5000)
            local body = { kind = "tool.result", id = "call-big", output = big, name = "read_file" }
            local rewritten, path = lib.maybe_dump_output(body, "chat-1")
            return {
              not_same = rewritten ~= body,
              original_output = body.output,
              new_output = rewritten.output,
              path = path,
              id = rewritten.id,
              name = rewritten.name,
            }
            "#,
        )
        .eval()
        .expect("eval");

    let not_same: bool = result.get("not_same").expect("not_same");
    let new_output: String = result.get("new_output").expect("new");
    let original_output: String = result.get("original_output").expect("orig");
    let path: String = result.get("path").expect("path");
    let id: String = result.get("id").expect("id");
    let name: String = result.get("name").expect("name");

    // The lib copies the body — the caller's body is never aliased.
    assert!(not_same, "rewritten body must be a fresh table");
    assert!(
        original_output.len() > new_output.len(),
        "summary should be smaller than original"
    );
    assert!(
        new_output.contains("Output written to"),
        "summary header present: {new_output}"
    );
    // Other body fields survive the copy.
    assert_eq!(id, "call-big");
    assert_eq!(name, "read_file");
    // The on-disk file under chat-1 carries the FULL original bytes.
    let expected_dir = tempdir.path().join("tool-results").join("chat-1");
    assert!(
        path.starts_with(&expected_dir.display().to_string()),
        "dump path under chat-1: {path}"
    );
    let on_disk = std::fs::read_to_string(&path).expect("read dump");
    assert_eq!(on_disk, "PAYLOAD\n".repeat(5000));

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn maybe_dump_output_ignores_bodies_without_string_id() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    // Without a tool_id we can't address a dump file. Lib must
    // short-circuit — no rewrite, no disk write.
    let result: Table = lua
        .load(
            r#"
            local lib = require("tool-gate")
            local big = string.rep("X", 64 * 1024)
            local body = { kind = "tool.result", output = big }
            local rewritten, path = lib.maybe_dump_output(body, "chat-1")
            return { same = rewritten == body, path = path }
            "#,
        )
        .eval()
        .expect("eval");

    let same: bool = result.get("same").expect("same");
    let path: Option<String> = result.get("path").expect("path");
    assert!(same);
    assert!(path.is_none());

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

// ----------------------------------------------------------------
// agents_md_emit_for_invoke — instruction reminder bridge
// ----------------------------------------------------------------

#[test]
fn agents_md_emit_for_invoke_no_ops_on_non_outbound_invoke_envelopes() {
    // The lib must short-circuit when the envelope isn't a
    // <gate>.tool.invoke. The starter wrapper drives every outbound
    // envelope through this helper, so a no-op on non-invoke shapes is
    // load-bearing.
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let count: i64 = lua
        .load(
            r#"
            local lib = require("tool-gate")
            local chat_emitter = require("libs.chat-emitter")
            local t = lib.translator("tool-gate")
            require("tool-gate.agents_md")._reset()
            local emitted = {}
            local emitter = chat_emitter.instruction(nil, function(body) emitted[#emitted + 1] = body end)
            local n = lib.agents_md_emit_for_invoke(
              t,
              { type = "event", from = "agentic-loop",
                body = { kind = "chat.input.submit" } },
              emitter
            )
            return n
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(count, 0);
}

#[test]
fn agents_md_emit_for_invoke_emits_for_folder_touching_outbound_invoke() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let dir = tempdir.path().join("proj");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("AGENTS.md"), "PROJ-RULES\n").expect("write");
    let touched = dir.join("file.txt");

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let result: Table = lua
        .load(format!(
            r#"
            local lib = require("tool-gate")
            local chat_emitter = require("libs.chat-emitter")
            local t = lib.translator("tool-gate")
            local agents_md = require("tool-gate.agents_md")
            agents_md._reset()
            agents_md.record_tool_contexts_from_advertise({{
              tools = {{
                {{ name = "read_file",
                   context = {{ folders = {{ {{ from = "file_path", arg = "path" }} }} }} }}
              }}
            }})
            local emitted = {{}}
            local emitter = chat_emitter.instruction({{
              session_id = "session-1", run_id = "sub-run", run_scope = "r2",
              actor_id = "scout.run-tool", capability_id = "r2/cap-1", principal = "subagent",
            }}, function(body) emitted[#emitted + 1] = body end)
            local n = lib.agents_md_emit_for_invoke(
              t,
              {{ type = "event", from = "agentic-loop",
                body = {{ kind = "tool-gate.tool.invoke",
                          id = "i1", name = "read_file",
                          args = {{ path = "{p}" }} }} }},
              emitter
            )
            return {{ n = n, emitted = emitted }}
            "#,
            p = touched.display(),
        ))
        .eval()
        .expect("eval");

    let n: i64 = result.get("n").expect("n");
    let emitted: Table = result.get("emitted").expect("emitted");
    let emitted_len: i64 = emitted.len().expect("len");
    assert_eq!(n, 1, "must emit one reminder for the project scope");
    assert_eq!(emitted_len, n);

    // Each emitted body is a dedicated scoped diagnostic event.
    let first: Table = emitted.get(1).expect("first");
    let kind: String = first.get("kind").expect("kind");
    let text: String = first.get("text").expect("text");
    let invocation: Table = first.get("invocation").expect("invocation");
    assert_eq!(kind, "chat.instruction.notice");
    assert_eq!(
        invocation.get::<String>("capability_id").unwrap(),
        "r2/cap-1"
    );
    assert!(text.contains("Local instruction files available"));
    assert!(text.contains("AGENTS.md"));
    assert!(
        !text.contains("PROJ-RULES"),
        "reminder must not include instruction contents: {text}"
    );
}

#[test]
fn agents_md_emit_for_invoke_swallows_underlying_errors_returns_zero() {
    // pcall-guard contract: a bug in `agents_md.emit_reminders_for_tool_call`
    // must not crash the wrapper. We monkey-patch the lib to error,
    // then verify the helper returns 0 instead of propagating.
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("nefor stub");
    set_package_path(&lua).expect("package.path");

    let n: i64 = lua
        .load(
            r#"
            local lib = require("tool-gate")
            local chat_emitter = require("libs.chat-emitter")
            local t = lib.translator("tool-gate")
            local agents_md = require("tool-gate.agents_md")
            agents_md._reset()
            local prev = agents_md.emit_reminders_for_tool_call
            agents_md.emit_reminders_for_tool_call = function() error("boom") end

            local emitter = chat_emitter.instruction(nil, function(_) end)
            local n = lib.agents_md_emit_for_invoke(
              t,
              { type = "event", from = "agentic-loop",
                body = { kind = "tool-gate.tool.invoke",
                         id = "i1", name = "read_file",
                         args = { path = "/some/path.txt" } } },
              emitter
            )

            agents_md.emit_reminders_for_tool_call = prev
            return n
            "#,
        )
        .eval()
        .expect("eval");

    assert_eq!(n, 0);
}
