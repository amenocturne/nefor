//! Unit + wrapper-integration tests for the tool-output dump-to-file
//! layer (`starter/lib/tool_output_dump.lua`) and its hook in
//! `starter/tool-gate/init.lua`.
//!
//! Spec context: lead-workflow-spec §5 — when a tool call returns
//! output past a threshold (large `read_file`, big `grep`, deep
//! `find`), the wrapper writes the **full** payload to a persistent
//! file under `<NEFOR_DATA_HOME>/tool-results/` and replaces the
//! inline `body.output` with a summary that names the path and
//! includes a 4 KiB preview. The model can grep the file later via
//! the bash tool to extract specifics it didn't get inline.
//!
//! These tests drive the Lua module directly under a stubbed
//! `nefor.*` surface (matches `starter_sessions_test.rs`'s harness).

use std::path::PathBuf;
use std::sync::Mutex;

use mlua::{Function, Lua, Table, Value};

fn starter_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root is two levels above crates/nefor")
        .join("starter")
}

// ----------------------------------------------------------------
// lib/tool_output_dump.lua — unit tests
// ----------------------------------------------------------------

#[test]
fn small_string_output_does_not_dump() {
    // Below the inline budget the wrapper must be a no-op. The whole
    // point of the threshold is to leave normal-size tool calls
    // untouched (no disk write, no summary swap).
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_HOME").ok();
    std::env::set_var("NEFOR_DATA_HOME", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let should_dump: bool = lua
        .load(
            r#"
            local d = require("lib.tool_output_dump")
            return d.should_dump("small string")
            "#,
        )
        .eval()
        .expect("should_dump");
    assert!(!should_dump, "small string must not trigger dump");

    // Empty / nil also never dumps.
    let nil_dump: bool = lua
        .load(r#"return require("lib.tool_output_dump").should_dump(nil)"#)
        .eval()
        .expect("nil should_dump");
    assert!(!nil_dump, "nil output must not trigger dump");

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_HOME", v),
        None => std::env::remove_var("NEFOR_DATA_HOME"),
    }
}

#[test]
fn large_output_writes_full_contents_to_file_and_returns_summary() {
    // The load-bearing assertion: 50 KiB of output → file exists at
    // <NEFOR_DATA_HOME>/tool-results/<chat_id>/<call_id>.txt with the
    // EXACT original bytes; the returned summary names the path and
    // includes the "Output written to" + "use `grep` … to extract
    // more" framing that the model reads to decide its next move.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_HOME").ok();
    std::env::set_var("NEFOR_DATA_HOME", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    // Build a 50 KiB payload from a repeating pattern that's distinct
    // enough we can grep for known offsets. 50 * 1024 / 5 = 10240
    // copies of "ABCDE\n" — close to 50 KiB exactly.
    let setup: Table = lua
        .load(
            r#"
            local d = require("lib.tool_output_dump")
            local payload = string.rep("ABCDE\n", 10240)
            local summary, path, err = d.dump("chat-1", "call-42", payload, { tool = "read_file" })
            return { summary = summary, path = path, err = err, payload_len = #payload }
            "#,
        )
        .eval()
        .expect("dump");

    let err: Option<String> = setup.get("err").expect("err");
    assert!(err.is_none(), "dump must not error: {err:?}");

    let summary: String = setup.get("summary").expect("summary");
    let path: String = setup.get("path").expect("path");
    let payload_len: i64 = setup.get("payload_len").expect("payload_len");

    // Path is under the configured data root.
    let expected_dir = tempdir.path().join("tool-results").join("chat-1");
    assert!(
        path.starts_with(&expected_dir.display().to_string()),
        "path under tool-results/<chat_id>/, got {path}"
    );
    assert!(path.ends_with("call-42.txt"), "leaf is <call_id>.txt: {path}");

    // File exists with the original bytes verbatim — the model relies
    // on `grep <pattern> <path>` working over the SAME bytes the
    // tool produced; any rewrite (compression, JSON-wrapping,
    // prefix/suffix injection) would break that contract.
    assert!(std::path::Path::new(&path).exists(), "dump file missing: {path}");
    let on_disk = std::fs::read_to_string(&path).expect("read dump file");
    assert_eq!(
        on_disk.len() as i64,
        payload_len,
        "dump file size must equal original payload size"
    );
    assert_eq!(
        on_disk,
        "ABCDE\n".repeat(10240),
        "dump file contents must match original payload byte-for-byte"
    );

    // Summary contains the "written to <path>" framing + a preview
    // (up to PREVIEW_BYTES = 4 KiB) drawn from the head of the
    // payload + the grep/head suggestion that tells the model how to
    // extract more.
    assert!(
        summary.contains("Output written to"),
        "summary missing written-to header: {summary}"
    );
    assert!(
        summary.contains(&path),
        "summary must name the on-disk path: {summary}"
    );
    assert!(
        summary.contains("grep"),
        "summary must point at grep as extraction tool: {summary}"
    );
    // First 4 KiB of "ABCDE\n" repeats start with "ABCDE" — preview is
    // contiguous so it must contain that prefix.
    assert!(
        summary.contains("ABCDE\nABCDE"),
        "summary must include preview prefix from payload"
    );
    // Summary should be much smaller than the original — that's the
    // entire reason to dump in the first place. PREVIEW_BYTES (4 KiB)
    // + framing should stay well under the 32 KiB inline budget.
    assert!(
        summary.len() < 32 * 1024,
        "summary should fit comfortably under inline budget; got {} bytes",
        summary.len()
    );

    // Meta companion file lands next to the dump with the per-call
    // metadata for debugging.
    let meta_path = path.replace(".txt", ".meta.json");
    assert!(
        std::path::Path::new(&meta_path).exists(),
        "meta companion missing: {meta_path}"
    );
    let meta = std::fs::read_to_string(&meta_path).expect("read meta");
    assert!(
        meta.contains("read_file"),
        "meta should record the tool name: {meta}"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_HOME", v),
        None => std::env::remove_var("NEFOR_DATA_HOME"),
    }
}

#[test]
fn missing_chat_id_falls_back_to_unscoped_directory() {
    // chat_id is best-effort surface; the lib must not refuse to dump
    // when it's absent. Falling back to `_unscoped/` keeps the dump
    // path stable for early firings (where the tool-executor's pending
    // entry doesn't carry chat_id) and for sub-graph runs whose chat
    // scoping isn't wired through to the wrapper layer yet.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_HOME").ok();
    std::env::set_var("NEFOR_DATA_HOME", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let path: String = lua
        .load(
            r#"
            local d = require("lib.tool_output_dump")
            local big = string.rep("X", 64 * 1024)
            local _, path, _ = d.dump(nil, "call-99", big, nil)
            return path
            "#,
        )
        .eval()
        .expect("dump nil chat_id");

    let unscoped_dir = tempdir.path().join("tool-results").join("_unscoped");
    assert!(
        path.starts_with(&unscoped_dir.display().to_string()),
        "missing chat_id must route under tool-results/_unscoped/: {path}"
    );
    assert!(std::path::Path::new(&path).exists(), "dump file missing: {path}");

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_HOME", v),
        None => std::env::remove_var("NEFOR_DATA_HOME"),
    }
}

#[test]
fn table_output_is_json_encoded_for_disk_write() {
    // Tools may legitimately return table-shaped output. The dump path
    // has to handle both: stringify via json.encode for the size check
    // AND for the on-disk write so the model can grep a textual form.
    // The summary still names the file — model reads it the same way.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_HOME").ok();
    std::env::set_var("NEFOR_DATA_HOME", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let setup: Table = lua
        .load(
            r#"
            local d = require("lib.tool_output_dump")
            -- Build a table whose JSON encoding exceeds the budget.
            local entries = {}
            for i = 1, 4000 do
                entries[i] = { idx = i, marker = "needle-" .. tostring(i) }
            end
            local _, path, _ = d.dump("chat-2", "call-77", entries, nil)
            return { path = path }
            "#,
        )
        .eval()
        .expect("dump table");

    let path: String = setup.get("path").expect("path");
    assert!(std::path::Path::new(&path).exists());
    let body = std::fs::read_to_string(&path).expect("read dump");
    // The body is JSON-encoded — assert it's valid JSON and contains
    // a known marker the test seeded.
    let json: serde_json::Value =
        serde_json::from_str(&body).expect("dumped body must be valid JSON");
    assert!(json.is_array(), "table dump should remain an array");
    assert!(
        body.contains("needle-1234"),
        "dumped JSON must preserve seeded markers"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_HOME", v),
        None => std::env::remove_var("NEFOR_DATA_HOME"),
    }
}

// ----------------------------------------------------------------
// tool-gate wrapper hook — integration test
// ----------------------------------------------------------------

#[test]
fn tool_gate_wrapper_swaps_huge_tool_result_output_to_summary() {
    // The wrapper hook in starter/tool-gate/init.lua: when an inbound
    // `tool.result` envelope carries an `output` past the budget, the
    // wrapper writes the full payload to disk and replaces
    // `body.output` with the summary string before republishing. The
    // model — which sees envelopes via the published bus traffic —
    // gets the small summary in its history. Smaller envelopes pass
    // through unchanged.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_HOME").ok();
    std::env::set_var("NEFOR_DATA_HOME", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor_with_send_recorder(&lua).expect("stub");
    install_agentic_loop_stub(&lua).expect("agentic-loop stub");
    set_package_path(&lua).expect("set package.path");

    // Build the wrapper and drive its `from_plugin` callback directly
    // with two envelopes: a large tool.result (must be swapped) and a
    // small tool.result (must pass through verbatim).
    lua.load(
        r#"
        local tool_gate = require("tool-gate")
        local spec = tool_gate.spawn_spec("tool-gate", { "fake-binary" })
        _from_plugin = spec.from_plugin

        local big = string.rep("PAYLOAD-LINE\n", 5000)  -- ~65 KiB
        _big_size = #big

        _from_plugin({
            { type = "event", from = "tool-gate",
              body = { kind = "tool.result", id = "call-big", output = big, name = "read_file" } },
            { type = "event", from = "tool-gate",
              body = { kind = "tool.result", id = "call-small", output = "ok", name = "read_file" } },
        })
        "#,
    )
    .exec()
    .expect("drive wrapper");

    // Read back the captured `engine.send` payloads. Two envelopes
    // got published — one per inbound tool.result. The big-id payload
    // must carry the summary, not the original bytes; the small-id
    // payload must be untouched.
    let trace: Table = lua.globals().get("_send_trace").expect("trace");
    let len = trace.len().expect("len") as usize;
    let payloads: Vec<String> = (1..=len)
        .map(|i| trace.get::<String>(i).expect("entry"))
        .collect();
    assert_eq!(payloads.len(), 2, "expected 2 publishes, got {}: {payloads:?}", payloads.len());

    // Small one (call-small) is verbatim — output stays "ok".
    let small = payloads
        .iter()
        .find(|p| p.contains("\"id\":\"call-small\""))
        .expect("small payload missing");
    assert!(
        small.contains("\"output\":\"ok\""),
        "small output must pass through verbatim: {small}"
    );

    // Big one (call-big) — output replaced with the summary.
    let big = payloads
        .iter()
        .find(|p| p.contains("\"id\":\"call-big\""))
        .expect("big payload missing");
    assert!(
        big.contains("Output written to"),
        "big payload must carry summary header: {big}"
    );
    // The summary inlines a 4 KiB preview, so a few hundred
    // PAYLOAD-LINE repetitions are expected in the published envelope.
    // The point of the dump is that the FULL output (5000 reps,
    // ~65 KiB) does NOT land inline. Bound the published-envelope
    // size to comfortably less than the original — well under the
    // 32 KiB inline budget.
    let big_size: i64 = lua
        .load(r#"return _big_size"#)
        .eval()
        .expect("big size");
    assert!(
        (big.len() as i64) < big_size / 2,
        "published envelope must be much smaller than original ({} bytes vs {} bytes)",
        big.len(),
        big_size
    );

    // The on-disk file landed with the FULL original bytes, ready
    // for the model to grep on a subsequent turn.
    let scope_dir = tempdir.path().join("tool-results").join("_unscoped");
    let dump_path = scope_dir.join("call-big.txt");
    assert!(dump_path.exists(), "dump file missing at {dump_path:?}");
    let on_disk = std::fs::read_to_string(&dump_path).expect("read dump");
    assert_eq!(
        on_disk,
        "PAYLOAD-LINE\n".repeat(5000),
        "dump file must contain the full original payload"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_HOME", v),
        None => std::env::remove_var("NEFOR_DATA_HOME"),
    }
}

// ----------------------------------------------------------------
// shared harness
// ----------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());

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

    let bus_tbl = lua.create_table()?;
    let on_event = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    bus_tbl.set("on_event", on_event)?;
    nefor.set("bus", bus_tbl)?;

    let events_tbl = lua.create_table()?;
    let events_on = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    events_tbl.set("on", events_on)?;
    nefor.set("events", events_tbl)?;

    let engine_tbl = lua.create_table()?;
    let send_fn = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    engine_tbl.set("send", send_fn)?;
    let now_fn = lua.create_function(|_, _: ()| Ok("2026-05-04T00:00:00.000Z".to_owned()))?;
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

/// Same as `install_stub_nefor` but `engine.send` records every
/// payload (the first arg) into a global `_send_trace` array so tests
/// can assert on what the wrapper republished.
fn install_stub_nefor_with_send_recorder(lua: &Lua) -> mlua::Result<()> {
    install_stub_nefor(lua)?;
    lua.load(
        r#"
        _send_trace = {}
        nefor.engine.send = function(payload, _target)
            _send_trace[#_send_trace + 1] = payload
        end
        "#,
    )
    .exec()?;
    Ok(())
}

/// Stand-in for `require("agentic-loop")` that the tool-gate wrapper
/// calls into. The dump-and-summarise hook fires BEFORE the
/// bookkeeping branch, so for these tests we just need the module to
/// answer `take_pending_for_tool` with nil (no pending firing) — the
/// wrapper then republishes the (now-summarised) envelope and returns.
fn install_agentic_loop_stub(lua: &Lua) -> mlua::Result<()> {
    lua.load(
        r#"
        package.preload["agentic-loop"] = function()
            return {
                take_pending_for_tool = function(_) return nil, nil end,
                clear_pending_key      = function(_) end,
                fire_tool_end_observers = function(_, _, _) end,
                queue_sub_graph         = function(_, _) return nil, "stub: no sub-graph" end,
            }
        end
        "#,
    )
    .exec()?;
    Ok(())
}

fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let starter = starter_dir();
    let starter_str = starter.display().to_string();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          package.path,
        }}, ";")
        "#,
        starter = starter_str
    );
    lua.load(&script).exec()
}
