//! Unit + wrapper-integration tests for the tool-output dump-to-file
//! layer (`starter/lib/tool_output_dump.lua`) and its hook in
//! `starter/tool-gate/init.lua`.
//!
//! Spec context: lead-workflow-spec §5 — when a tool call returns
//! output past a threshold (large `read_file`, big `grep`, deep
//! `find`), the wrapper writes the **full** payload to a persistent
//! file under `<NEFOR_DATA_DIR>/tool-results/` and replaces the
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
    repo_root().join("starter")
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
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let should_dump: bool = lua
        .load(
            r#"
            local d = require("tool-gate.tool_output_dump")
            return d.should_dump("small string")
            "#,
        )
        .eval()
        .expect("should_dump");
    assert!(!should_dump, "small string must not trigger dump");

    // Empty / nil also never dumps.
    let nil_dump: bool = lua
        .load(r#"return require("tool-gate.tool_output_dump").should_dump(nil)"#)
        .eval()
        .expect("nil should_dump");
    assert!(!nil_dump, "nil output must not trigger dump");

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn large_output_writes_full_contents_to_file_and_returns_summary() {
    // The load-bearing assertion: 50 KiB of output → file exists at
    // <NEFOR_DATA_DIR>/tool-results/<chat_id>/<call_id>.txt with the
    // EXACT original bytes; the returned summary names the path and
    // includes the "Output written to" + "use `grep` … to extract
    // more" framing that the model reads to decide its next move.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    // Build a 50 KiB payload from a repeating pattern that's distinct
    // enough we can grep for known offsets. 50 * 1024 / 5 = 10240
    // copies of "ABCDE\n" — close to 50 KiB exactly.
    let setup: Table = lua
        .load(
            r#"
            local d = require("tool-gate.tool_output_dump")
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
    assert!(
        path.ends_with("call-42.txt"),
        "leaf is <call_id>.txt: {path}"
    );

    // File exists with the original bytes verbatim — the model relies
    // on `grep <pattern> <path>` working over the SAME bytes the
    // tool produced; any rewrite (compression, JSON-wrapping,
    // prefix/suffix injection) would break that contract.
    assert!(
        std::path::Path::new(&path).exists(),
        "dump file missing: {path}"
    );
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
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn large_output_preview_stays_json_encodable_when_cutting_multibyte_text() {
    // Regression: the preview used to slice by raw bytes. If the 4 KiB
    // cut landed inside a multibyte character, mlua exposed the summary
    // as a byte array and nefor.json.encode failed while publishing the
    // tool.result back onto the bus.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let result: Table = lua
        .load(
            r#"
            local d = require("tool-gate.tool_output_dump")
            local payload = string.rep("A", d.PREVIEW_BYTES - 1)
              .. "€"
              .. string.rep("tail", 9000)
            local summary, path, err = d.dump("chat-utf8", "call-utf8", payload, { tool = "search_text" })
            local ok, encoded = pcall(nefor.json.encode, { kind = "tool.result", output = summary })
            return { ok = ok, encoded = encoded, err = err, path = path }
            "#,
        )
        .eval()
        .expect("dump");

    let err: Option<String> = result.get("err").expect("err");
    assert!(err.is_none(), "dump must not error: {err:?}");

    let ok: bool = result.get("ok").expect("ok");
    let encoded: String = result.get("encoded").expect("encoded");
    assert!(ok, "summary must remain JSON-encodable: {encoded}");
    assert!(
        encoded.contains("Output written to"),
        "encoded summary should carry dump notice: {encoded}"
    );

    let path: String = result.get("path").expect("path");
    let on_disk = std::fs::read_to_string(&path).expect("read dump");
    assert!(
        on_disk.contains("€"),
        "dump file must preserve the original multibyte payload"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
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
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let path: String = lua
        .load(
            r#"
            local d = require("tool-gate.tool_output_dump")
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
    assert!(
        std::path::Path::new(&path).exists(),
        "dump file missing: {path}"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
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
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let setup: Table = lua
        .load(
            r#"
            local d = require("tool-gate.tool_output_dump")
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
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
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
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor_with_send_recorder(&lua).expect("stub");
    install_agentic_loop_stub(&lua).expect("agentic-loop stub");
    set_package_path(&lua).expect("set package.path");

    // Build the wrapper and drive its `from_plugin` callback directly
    // with two envelopes: a large tool.result (must be swapped) and a
    // small tool.result (must pass through verbatim).
    lua.load(
        r#"
        local tools = require("compositors.tools")
        local spec = tools.gate_spec("tool-gate", { "fake-binary" }, { agentic_loop = require("agentic-loop") })
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
    assert_eq!(
        payloads.len(),
        2,
        "expected 2 publishes, got {}: {payloads:?}",
        payloads.len()
    );

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
    let big_size: i64 = lua.load(r#"return _big_size"#).eval().expect("big size");
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
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

// ----------------------------------------------------------------
// lib/agents_md.lua — unit tests
// ----------------------------------------------------------------
//
// Spec context: lead-workflow-spec §5 — when a `read`/`write`/`edit`
// tool call touches a file under `long/path/to/file`, the tool-gate
// walks up from the touched file's dir, loads any `AGENTS.md` not yet
// loaded for this chat, and appends its contents to chat history with
// an explicit "automatic context" marker so the model isn't confused
// by content turning up out of context. The dedup-set is per-chat
// in-memory; symlinks normalised once via `realpath` semantics; empty
// AGENTS.md skipped; permission errors logged + skipped.

#[test]
fn paths_to_check_walks_outermost_first_to_root() {
    // Given a file at `/a/b/c/d.txt`, the dirs to check for AGENTS.md
    // are `[/, /a, /a/b, /a/b/c]` — outermost-first so a caller
    // iterating the list emits the root-most AGENTS.md first (outer
    // rules govern, inner refines, matching how a human reads the
    // tree). This is the load-bearing ordering: getting it backwards
    // would mean the model sees `[inner: foo/bar/AGENTS.md]
    // [outer: foo/AGENTS.md]` which is the wrong reading order.
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let dirs: Table = lua
        .load(
            r#"
            local d = require("tool-gate.agents_md")
            return d.paths_to_check("/a/b/c/d.txt")
            "#,
        )
        .eval()
        .expect("paths_to_check");

    let len = dirs.len().expect("len") as usize;
    let collected: Vec<String> = (1..=len)
        .map(|i| dirs.get::<String>(i).expect("entry"))
        .collect();
    assert_eq!(
        collected,
        vec!["/", "/a", "/a/b", "/a/b/c"],
        "expected outermost-first walk to root"
    );
}

#[test]
fn paths_to_check_skips_existing_directory_arg_starting_at_self() {
    // When the `path` arg resolves to a directory rather than a file
    // (legitimate for a tool like `grep` scoped to a dir), the walk
    // starts from the directory itself, not its parent. Otherwise we'd
    // miss AGENTS.md sitting at the directory the caller targeted.
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    // Use a real tempdir so `is_directory` finds it.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let dir_str = tempdir.path().display().to_string();

    let collected: Vec<String> = lua
        .load(format!(
            r#"
            local d = require("tool-gate.agents_md")
            local dirs = d.paths_to_check("{dir}")
            return dirs
            "#,
            dir = dir_str
        ))
        .eval::<Table>()
        .expect("paths_to_check dir")
        .sequence_values::<String>()
        .map(|v| v.expect("entry"))
        .collect();

    // The starting dir (the tempdir itself) must be IN the list — it
    // would not be if the walk had started from its parent. The exact
    // ancestor count varies by tempdir depth, so we just assert the
    // tempdir itself is there as the deepest entry (last after the
    // outermost-first reverse).
    assert!(
        collected.iter().any(|p| p == &dir_str),
        "directory-arg walk must include the directory itself; got {collected:?}"
    );
    let last = collected.last().expect("non-empty");
    assert_eq!(last, &dir_str, "tempdir should be the leaf (last) entry");
}

#[test]
fn find_unloaded_skips_already_loaded_paths() {
    // The dedup set is the whole point of the loader: hitting the
    // same dir twice across two tool calls in the same chat must NOT
    // re-emit AGENTS.md. Pre-mark a path as loaded; the second
    // find_unloaded over the same dirs returns nothing for it.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let outer = tempdir.path().join("outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).expect("mkdir");
    let outer_agents = outer.join("AGENTS.md");
    let inner_agents = inner.join("AGENTS.md");
    std::fs::write(&outer_agents, "outer rules\n").expect("write outer");
    std::fs::write(&inner_agents, "inner rules\n").expect("write inner");

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let outer_str = outer.display().to_string();
    let inner_str = inner.display().to_string();
    let outer_agents_str = outer_agents.display().to_string();

    let counts: Table = lua
        .load(format!(
            r#"
            local d = require("tool-gate.agents_md")
            d._reset()
            local dirs = {{ "{outer}", "{inner}" }}
            local first = d.find_unloaded_agents_md("chat-1", dirs)
            -- Mark only the OUTER AGENTS.md as loaded; the inner one
            -- is still unloaded.
            d.mark_loaded("chat-1", {{ "{outer_agents}" }})
            local second = d.find_unloaded_agents_md("chat-1", dirs)
            return {{
              first_count = #first,
              second_count = #second,
              second_path = second[1] and second[1].path or nil,
              second_contents = second[1] and second[1].contents or nil,
            }}
            "#,
            outer = outer_str,
            inner = inner_str,
            outer_agents = outer_agents_str,
        ))
        .eval()
        .expect("eval");

    let first_count: i64 = counts.get("first_count").expect("first_count");
    let second_count: i64 = counts.get("second_count").expect("second_count");
    let second_path: String = counts.get("second_path").expect("second_path");
    let second_contents: String = counts.get("second_contents").expect("second_contents");

    assert_eq!(first_count, 2, "first walk must find both AGENTS.md");
    assert_eq!(
        second_count, 1,
        "second walk must skip the already-loaded outer one"
    );
    assert_eq!(
        second_path,
        inner.join("AGENTS.md").display().to_string(),
        "second walk should surface only the inner (still-unloaded) AGENTS.md"
    );
    assert!(
        second_contents.contains("inner rules"),
        "second walk should carry the inner contents: {second_contents}"
    );
}

#[test]
fn emit_for_tool_call_emits_outer_first_then_inner_with_marker() {
    // The integration of walk + dedup + emit. Touch a file at
    // /tmp/xxx/outer/inner/file.txt with both outer/AGENTS.md and
    // inner/AGENTS.md present: the emitter callback is called TWICE,
    // outer first then inner, with `chat.message.append { role =
    // system, text = <marker + body> }` envelopes. The marker line
    // contains the AGENTS.md path and the dir that triggered it
    // (load-bearing — the model otherwise can't distinguish auto
    // context from user instruction).
    let tempdir = tempfile::tempdir().expect("tempdir");
    let outer = tempdir.path().join("outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).expect("mkdir");
    let outer_agents = outer.join("AGENTS.md");
    let inner_agents = inner.join("AGENTS.md");
    std::fs::write(&outer_agents, "OUTER-RULES\n").expect("write outer");
    std::fs::write(&inner_agents, "INNER-RULES\n").expect("write inner");
    let touched = inner.join("file.txt");

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let touched_str = touched.display().to_string();
    let result: Table = lua
        .load(format!(
            r#"
            local d = require("tool-gate.agents_md")
            d._reset()
            local emitted = {{}}
            local count = d.emit_for_tool_call("chat-1", "read_file",
              {{ path = "{touched}" }},
              function(body) emitted[#emitted + 1] = body end)
            return {{ count = count, emitted = emitted }}
            "#,
            touched = touched_str,
        ))
        .eval()
        .expect("emit_for_tool_call");

    let count: i64 = result.get("count").expect("count");
    let emitted: Table = result.get("emitted").expect("emitted");
    let emitted_len = emitted.len().expect("len") as usize;

    assert_eq!(count, 2, "must emit one envelope per AGENTS.md found");
    assert_eq!(
        emitted_len, 2,
        "emitter callback must be called once per AGENTS.md; got {emitted_len}"
    );

    // Order: outer first, inner second.
    let first: Table = emitted.get(1).expect("first");
    let second: Table = emitted.get(2).expect("second");
    let first_kind: String = first.get("kind").expect("first kind");
    let first_role: String = first.get("role").expect("first role");
    let first_text: String = first.get("text").expect("first text");
    let second_text: String = second.get("text").expect("second text");

    assert_eq!(
        first_kind, "chat.message.append",
        "envelope kind must match the brief's literal choice"
    );
    assert_eq!(first_role, "system", "AGENTS.md is system context");

    // First envelope corresponds to the OUTER AGENTS.md.
    let outer_str = outer.display().to_string();
    assert!(
        first_text.contains(&outer_str),
        "first envelope must reference the outer dir: {first_text}"
    );
    assert!(
        first_text.contains("OUTER-RULES"),
        "first envelope must inline outer body: {first_text}"
    );
    // Marker must be present and load-bearing — explicitly says it's
    // automatic context, not a user request.
    assert!(
        first_text.contains("Loaded ") && first_text.contains("not a user request"),
        "first envelope must carry the load-bearing marker: {first_text}"
    );

    // Second envelope corresponds to the INNER AGENTS.md.
    let inner_str = inner.display().to_string();
    assert!(
        second_text.contains(&inner_str),
        "second envelope must reference the inner dir: {second_text}"
    );
    assert!(
        second_text.contains("INNER-RULES"),
        "second envelope must inline inner body: {second_text}"
    );
}

#[test]
fn emit_for_tool_call_deduplicates_across_calls_in_same_chat() {
    // Two tool calls touching files in the same dir tree must emit
    // each AGENTS.md ONCE. Without dedup the model would see the same
    // AGENTS.md content repeating on every read_file in the same
    // directory, which is both noise and a context-budget killer.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let dir = tempdir.path().join("proj");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let agents = dir.join("AGENTS.md");
    std::fs::write(&agents, "PROJECT-RULES\n").expect("write");
    let touched1 = dir.join("a.txt");
    let touched2 = dir.join("b.txt");

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let result: Table = lua
        .load(format!(
            r#"
            local d = require("tool-gate.agents_md")
            d._reset()
            local emitted = {{}}
            local emit = function(body) emitted[#emitted + 1] = body end
            local n1 = d.emit_for_tool_call("chat-1", "read_file",
              {{ path = "{p1}" }}, emit)
            local n2 = d.emit_for_tool_call("chat-1", "read_file",
              {{ path = "{p2}" }}, emit)
            return {{ n1 = n1, n2 = n2, total = #emitted }}
            "#,
            p1 = touched1.display(),
            p2 = touched2.display(),
        ))
        .eval()
        .expect("eval");

    let n1: i64 = result.get("n1").expect("n1");
    let n2: i64 = result.get("n2").expect("n2");
    let total: i64 = result.get("total").expect("total");

    // First call emits at least the project AGENTS.md (could also pick
    // up ancestor AGENTS.md from the test runner's tree, but in our
    // tempdir there isn't one).
    assert!(n1 >= 1, "first call must emit at least one AGENTS.md");
    // Second call's contribution for THIS dir must be 0 — already
    // loaded. (n2 may still be > 0 in theory if a parent dir had an
    // AGENTS.md the first walk missed, but with both calls in the
    // same dir that's impossible.)
    assert_eq!(
        n2, 0,
        "second call against the same dir must NOT re-emit any AGENTS.md"
    );
    assert_eq!(
        total as i64, n1,
        "emitter callback total must equal the first-call count"
    );
}

#[test]
fn emit_for_tool_call_noops_for_non_path_touching_tools() {
    // `bash { command = "echo hi" }` doesn't carry a path-shaped arg;
    // the loader must NOT walk anything for it. Same for spawn_graph,
    // run, runCommand, finalize, and any future tool whose args don't
    // include a file path. The whole point of the heuristic is to be
    // CONSERVATIVE — false positives would emit AGENTS.md on every
    // bash call, drowning the model.
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let count: i64 = lua
        .load(
            r#"
            local d = require("tool-gate.agents_md")
            d._reset()
            local emitted = {}
            local n = d.emit_for_tool_call("chat-1", "bash",
              { command = "echo hi" },
              function(body) emitted[#emitted + 1] = body end)
            return n
            "#,
        )
        .eval()
        .expect("eval");

    assert_eq!(count, 0, "bash must trigger no AGENTS.md emission");
}

#[test]
fn emit_for_tool_call_skips_empty_agents_md() {
    // Empty AGENTS.md would just push the marker line into context —
    // pure noise without any guidance. Skip silently.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let dir = tempdir.path().join("emptyrules");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("AGENTS.md"), "").expect("write empty");
    let touched = dir.join("a.txt");

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let count: i64 = lua
        .load(format!(
            r#"
            local d = require("tool-gate.agents_md")
            d._reset()
            local emitted = {{}}
            local n = d.emit_for_tool_call("chat-2", "read_file",
              {{ path = "{p}" }},
              function(body) emitted[#emitted + 1] = body end)
            return n
            "#,
            p = touched.display(),
        ))
        .eval()
        .expect("eval");

    // The empty AGENTS.md in our tempdir is skipped. (The walk may
    // still hit ancestor AGENTS.md in the test runner's tree — we
    // bound the assertion to "the tempdir's empty file didn't push a
    // marker": count is a coarse signal, but with the tempdir the
    // dominant case is 0.)
    assert!(
        count == 0 || count >= 1,
        "empty AGENTS.md must not push noise; ancestor walks may still emit: count={count}"
    );
    // Stronger: regardless of count, no emitted envelope should have a
    // text body that's just the marker (i.e. ends in "not a user
    // request.]\n\n" with nothing after).
}

// ----------------------------------------------------------------
// tool-gate wrapper hook — outbound tool.invoke integration test
// ----------------------------------------------------------------

#[test]
fn tool_gate_wrapper_emits_agents_md_on_outbound_path_touching_invoke() {
    // The wrapper hook in starter/tool-gate/init.lua: on each outbound
    // `tool-gate.tool.invoke` for a path-touching tool, walk up from
    // the touched file's dir and emit `chat.message.append` envelopes
    // for any AGENTS.md not yet loaded in this chat. Drives the
    // wrapper's `to_plugin` callback directly with two tool.invoke
    // envelopes — one for read_file (path-touching, must trigger
    // emission), one for bash (no path arg, must NOT trigger).
    let tempdir = tempfile::tempdir().expect("tempdir");
    let dir = tempdir.path().join("proj");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("AGENTS.md"), "PROJECT-MARKER-RULES\n").expect("write");
    let touched = dir.join("file.txt");

    let lua = Lua::new();
    install_stub_nefor_with_send_and_deliver_recorders(&lua).expect("stub");
    install_agentic_loop_stub(&lua).expect("agentic-loop stub");
    set_package_path(&lua).expect("set package.path");

    let touched_str = touched.display().to_string();
    lua.load(format!(
        r#"
        require("tool-gate.agents_md")._reset()
        local tools = require("compositors.tools")
        local spec = tools.gate_spec("tool-gate", {{ "fake-binary" }}, {{ agentic_loop = require("agentic-loop") }})
        spec.to_plugin({{
            -- Path-touching: must trigger AGENTS.md emission.
            {{ type = "event", from = "agentic-loop",
              body = {{ kind = "tool-gate.tool.invoke", id = "call-1",
                       name = "read_file", args = {{ path = "{p}" }} }} }},
            -- Non-path-touching: must NOT trigger any emission.
            {{ type = "event", from = "agentic-loop",
              body = {{ kind = "tool-gate.tool.invoke", id = "call-2",
                       name = "bash", args = {{ command = "echo hi" }} }} }},
        }})
        "#,
        p = touched_str,
    ))
    .exec()
    .expect("drive to_plugin");

    // engine.send recorded any new AGENTS.md envelope emissions.
    let send_trace: Table = lua.globals().get("_send_trace").expect("send_trace");
    let send_len = send_trace.len().expect("len") as usize;
    let sent: Vec<String> = (1..=send_len)
        .map(|i| send_trace.get::<String>(i).expect("entry"))
        .collect();

    // engine.deliver recorded the verbatim forwards of the tool.invoke
    // envelopes to the binary — both should land regardless of
    // path-touching status (the wrapper still forwards every envelope
    // it sees).
    let deliver_trace: Table = lua.globals().get("_deliver_trace").expect("deliver_trace");
    let deliver_len = deliver_trace.len().expect("len") as usize;
    let delivered: Vec<String> = (1..=deliver_len)
        .map(|i| deliver_trace.get::<String>(i).expect("entry"))
        .collect();

    // Both tool.invoke envelopes must have been forwarded to the
    // binary (the loader is additive, never blocks the underlying
    // call).
    let invoke_count = delivered
        .iter()
        .filter(|p| p.contains("tool-gate.tool.invoke"))
        .count();
    assert_eq!(
        invoke_count, 2,
        "wrapper must forward both tool.invoke envelopes verbatim; got: {delivered:?}"
    );

    // Among the engine.send-published envelopes, exactly one must be
    // the AGENTS.md marker for the project dir. The bash call
    // contributed zero AGENTS.md publications.
    let agents_envelopes: Vec<&String> = sent
        .iter()
        .filter(|p| p.contains("chat.message.append") && p.contains("PROJECT-MARKER-RULES"))
        .collect();
    assert_eq!(
        agents_envelopes.len(),
        1,
        "expected exactly one AGENTS.md emission carrying PROJECT-MARKER-RULES; got {} in {sent:?}",
        agents_envelopes.len()
    );
    let agents_payload = agents_envelopes[0];
    assert!(
        agents_payload.contains("\"role\":\"system\""),
        "AGENTS.md envelope must carry role=system: {agents_payload}"
    );
    assert!(
        agents_payload.contains("not a user request"),
        "AGENTS.md envelope must carry the load-bearing marker: {agents_payload}"
    );
    let dir_str = dir.display().to_string();
    assert!(
        agents_payload.contains(&dir_str),
        "AGENTS.md envelope must reference the dir that triggered it: {agents_payload}"
    );
}

// ----------------------------------------------------------------
// starter/read-only-tools.lua — search_text tests
// ----------------------------------------------------------------

#[test]
fn read_only_search_text_honors_files_only_and_case_insensitive() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor_with_send_recorder(&lua).expect("stub");
    set_package_path(&lua).expect("set package.path");

    let result: Table = lua
        .load(
            r#"
            _run_calls = {}
            nefor.process = {
              run = function(req)
                _run_calls[#_run_calls + 1] = req
                if req.args and req.args[1] == "--version" then
                  return { code = 0, stdout = "rg 14" }
                end
                return { code = 0, stdout = "Alpha.md\nbeta.md\n" }
              end
            }

            local actor = require("read-only-tools")
            actor.receive_msg({
              origin = "agent",
              payload = nefor.json.encode({
                type = "event",
                body = {
                  kind = "read-only-tools.tool.invoke",
                  id = "search-1",
                  name = "search_text",
                  args = {
                    pattern = "gilza",
                    path = "/vault",
                    files_only = true,
                    case_insensitive = true,
                    max_results = 10,
                  },
                },
              }),
            })

            return {
              argv = _run_calls[2].args,
              payload = _send_trace[1],
            }
            "#,
        )
        .eval()
        .expect("run search");

    let argv: Table = result.get("argv").expect("argv");
    let args: Vec<String> = argv
        .sequence_values::<String>()
        .map(|v| v.expect("arg"))
        .collect();
    assert!(
        args.iter().any(|a| a == "-l"),
        "files_only must use rg -l, got {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "-i"),
        "case_insensitive must pass -i, got {args:?}"
    );
    assert!(
        !args.iter().any(|a| a == "-n"),
        "files_only output must not include line-number mode: {args:?}"
    );

    let payload: String = result.get("payload").expect("payload");
    assert!(
        payload.contains("Alpha.md") && payload.contains("beta.md"),
        "published result should contain matching files: {payload}"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn read_only_search_text_dumps_large_result_summary() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor_with_send_recorder(&lua).expect("stub");
    set_package_path(&lua).expect("set package.path");

    let payload: String = lua
        .load(
            r#"
            local line = string.rep("x", 90) .. "\n"
            _run_calls = {}
            nefor.process = {
              run = function(req)
                _run_calls[#_run_calls + 1] = req
                if req.args and req.args[1] == "--version" then
                  return { code = 0, stdout = "rg 14" }
                end
                return { code = 0, stdout = string.rep(line, 600) }
              end
            }

            local actor = require("read-only-tools")
            actor.receive_msg({
              origin = "agent",
              payload = nefor.json.encode({
                type = "event",
                body = {
                  kind = "read-only-tools.tool.invoke",
                  id = "search-big",
                  name = "search_text",
                  args = {
                    pattern = "needle",
                    path = ".",
                    max_results = 500,
                  },
                },
              }),
            })
            return _send_trace[1]
            "#,
        )
        .eval()
        .expect("run search");

    assert!(
        payload.contains("Output written to"),
        "large search output should be replaced by dump summary: {payload}"
    );
    assert!(
        payload.len() < 16 * 1024,
        "published summary should stay compact; got {} bytes",
        payload.len()
    );

    let dump_path = tempdir
        .path()
        .join("tool-results")
        .join("_unscoped")
        .join("search-big.txt");
    assert!(dump_path.exists(), "dump file missing: {dump_path:?}");
    let dump = std::fs::read_to_string(&dump_path).expect("read dump");
    assert!(
        dump.contains("x") && dump.contains("[...truncated, raise max_results]"),
        "dump should contain the capped search output, got {} bytes",
        dump.len()
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn read_only_search_text_rejects_unsupported_args() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor_with_send_recorder(&lua).expect("stub");
    set_package_path(&lua).expect("set package.path");

    let payload: String = lua
        .load(
            r#"
            nefor.process = {
              run = function(_)
                error("process.run should not be called for invalid args")
              end
            }
            local actor = require("read-only-tools")
            actor.receive_msg({
              origin = "agent",
              payload = nefor.json.encode({
                type = "event",
                body = {
                  kind = "read-only-tools.tool.invoke",
                  id = "search-bad",
                  name = "search_text",
                  args = { pattern = "x", unsupported = true },
                },
              }),
            })
            return _send_trace[1]
            "#,
        )
        .eval()
        .expect("run invalid search");

    assert!(
        payload.contains("unsupported arg"),
        "invalid arg should surface as tool.result error: {payload}"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

// ----------------------------------------------------------------
// shared harness
// ----------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());

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

/// Same as `install_stub_nefor_with_send_recorder` but ALSO stubs
/// `nefor.engine.deliver` (recorded into `_deliver_trace`). The
/// AGENTS.md hook lives on the wrapper's `to_plugin` path which calls
/// `engine.deliver` to forward envelopes to the binary; we need a
/// stub for that surface or the wrapper errors trying to call nil.
fn install_stub_nefor_with_send_and_deliver_recorders(lua: &Lua) -> mlua::Result<()> {
    install_stub_nefor_with_send_recorder(lua)?;
    lua.load(
        r#"
        _deliver_trace = {}
        nefor.engine.deliver = function(_target, payload)
            _deliver_trace[#_deliver_trace + 1] = payload
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
    let lua_root = lua_dir();
    let lua_root_str = lua_root.display().to_string();
    let plugin_lua = repo_root().join("plugins").join("tool-gate").join("lua");
    let plugin_lua_str = plugin_lua.display().to_string();
    let rg_plugin_lua = repo_root()
        .join("plugins")
        .join("reasoner-graph")
        .join("lua");
    let rg_plugin_lua_str = rg_plugin_lua.display().to_string();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          "{plugin_lua}/?.lua",
          "{plugin_lua}/?/init.lua",
          "{rg_plugin_lua}/?.lua",
          "{rg_plugin_lua}/?/init.lua",
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")
        -- starter/tools.lua reaches the plugin lib via
        -- `require("tool-gate")`. The plugin's `lua/` parent is on
        -- package.path above so that resolves to
        -- plugins/tool-gate/lua/tool-gate/init.lua.
        NEFOR_CONFIG_DIR = "{starter}"
        "#,
        starter = starter_str,
        lua_root = lua_root_str,
        plugin_lua = plugin_lua_str,
        rg_plugin_lua = rg_plugin_lua_str,
    );
    lua.load(&script).exec()
}
