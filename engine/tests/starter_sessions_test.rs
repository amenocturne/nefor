//! Unit tests for `starter/sessions.lua`'s persistence + control-event
//! filtering, driven from Rust. Mirrors the `starter_ncp_test.rs`
//! harness pattern: install a stub `nefor.*` surface, point
//! NEFOR_DATA_DIR at a tempdir, then exercise the module directly.

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

#[test]
fn jsonl_excludes_session_control_events() {
    // Sessions module's persistence hook drops any envelope whose inner
    // `body.kind` starts with "sessions." so a resume_request event
    // never lands in the on-disk jsonl. We exercise this by driving the
    // hook directly with two envelopes — one normal, one a
    // sessions.resume_request — and asserting the file contains only
    // the normal one (plus the header).
    let tempdir = tempfile::tempdir().expect("tempdir");
    // Point sessions.lua's data root at our tempdir.
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    // Initialise the sessions module (mints a session id, opens jsonl).
    lua.load(
        r#"
        sessions = require("sessions")
        sessions_test = require("sessions.test")
        sessions.init()
        "#,
    )
    .exec()
    .expect("sessions init");

    let session_id: String = lua
        .load(r#"return sessions.current_id()"#)
        .eval()
        .expect("current_id");
    assert!(!session_id.is_empty(), "minted session id");
    let session_path: String = lua
        .load(r#"return sessions.current_path()"#)
        .eval()
        .expect("current_path");
    assert!(!session_path.is_empty(), "current_path");

    // Drive the persistence hook directly via the module's `_persist_envelope`
    // test handle. The hook expects a log-entry table {ts, origin, payload}
    // with payload as the JSON wire string of an NCP envelope.
    lua.load(
        r#"
        local json = nefor.json
        local function entry(origin, body)
            return {
                ts      = "2026-05-04T00:00:00.000Z",
                origin  = origin,
                payload = json.encode({ type = "event", body = body }),
            }
        end
        -- Normal traffic — must be persisted.
        sessions_test._persist_envelope(entry("ollama", { kind = "chat.message.append", role = "user", text = "hi" }))
        -- Session control event — must be DROPPED.
        sessions_test._persist_envelope(entry("engine", { kind = "sessions.resume_request", session_id = "x" }))
        sessions_test._persist_envelope(entry("engine", { kind = "sessions.session_end", session_id = "x" }))
        sessions_test._persist_envelope(entry("engine", { kind = "sessions.session_start", session_id = "y" }))
        sessions_test._persist_envelope(entry("engine", { kind = "sessions.resume_done", session_id = "y" }))
        -- Another normal entry.
        sessions_test._persist_envelope(entry("nefor-tui", { kind = "chat.input.submit", text = "hello" }))
        "#,
    )
    .exec()
    .expect("drive persistence");

    // Read the file back and assert the filter behaviour.
    let body = std::fs::read_to_string(&session_path).expect("read jsonl");
    let lines: Vec<&str> = body.lines().collect();
    // Header + 2 normal entries (the four sessions.* drops).
    assert_eq!(
        lines.len(),
        3,
        "expected header + 2 entries, got {}: {body}",
        lines.len()
    );
    // Header line carries `_session: true`.
    assert!(
        lines[0].contains("\"_session\":true"),
        "header line missing: {}",
        lines[0]
    );
    // Two retained entries — the chat.message.append and the
    // chat.input.submit, in append order.
    assert!(
        lines[1].contains("chat.message.append"),
        "first non-header entry should be chat.message.append: {}",
        lines[1]
    );
    assert!(
        lines[2].contains("chat.input.submit"),
        "second non-header entry should be chat.input.submit: {}",
        lines[2]
    );
    // Belt-and-braces: confirm no sessions.* string snuck through.
    for line in lines.iter().skip(1) {
        assert!(
            !line.contains("\"sessions."),
            "sessions.* control event leaked into jsonl: {line}",
        );
    }

    // Restore env.
    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn inbound_outbound_cycle_lands_in_jsonl() {
    // Engine-side persistence is gone: starter/sessions.lua is the sole
    // writer. Drive a realistic inbound→broadcast cycle through the
    // persistence hook and assert the jsonl mirrors what the broker
    // would feed it. The shape is the same `{ts, origin, target?,
    // payload}` row the engine used to write, so chat.lua's session
    // picker keeps working unchanged.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    lua.load(
        r#"
        sessions = require("sessions")
        sessions_test = require("sessions.test")
        sessions.init()
        "#,
    )
    .exec()
    .expect("sessions init");
    let session_path: String = lua
        .load(r#"return sessions.current_path()"#)
        .eval()
        .expect("current_path");

    // Three realistic envelopes the broker would have stamped + handed
    // to dispatch_subscriptions: an inbound chat.input.submit, an
    // outbound (Origin::Step) chat.input.echo, and another inbound
    // chat.message.append. The persistence hook receives them as the
    // same log-entry shape the engine used to clone into the session
    // writer.
    lua.load(
        r#"
        local json = nefor.json
        local function entry(origin, target, body)
            local payload = json.encode({ type = "event", body = body })
            local e = { ts = "2026-05-04T00:00:00.000Z", origin = origin, payload = payload }
            if target ~= nil then e.target = target end
            return e
        end
        sessions_test._persist_envelope(entry("nefor-tui", nil,
            { kind = "chat.input.submit", text = "hello" }))
        sessions_test._persist_envelope(entry("step", "nefor-tui",
            { kind = "chat.message.append", role = "user", text = "hello" }))
        sessions_test._persist_envelope(entry("ollama", nil,
            { kind = "chat.stream.end", chat_id = "c1" }))
        "#,
    )
    .exec()
    .expect("drive persistence");

    let body = std::fs::read_to_string(&session_path).expect("read jsonl");
    let lines: Vec<&str> = body.lines().collect();
    // Header + 3 entries.
    assert_eq!(
        lines.len(),
        4,
        "expected header + 3 entries, got {}: {body}",
        lines.len()
    );
    // Header carries the marker.
    assert!(
        lines[0].contains("\"_session\":true"),
        "header missing: {}",
        lines[0]
    );
    // Entries carry the engine-shape fields: ts, origin, optional
    // target, payload.
    assert!(
        lines[1].contains("\"origin\":\"nefor-tui\"")
            && lines[1].contains("chat.input.submit")
            && !lines[1].contains("\"target\""),
        "inbound entry shape wrong: {}",
        lines[1]
    );
    assert!(
        lines[2].contains("\"origin\":\"step\"")
            && lines[2].contains("\"target\":\"nefor-tui\"")
            && lines[2].contains("chat.message.append"),
        "outbound entry shape wrong: {}",
        lines[2]
    );
    assert!(
        lines[3].contains("\"origin\":\"ollama\"") && lines[3].contains("chat.stream.end"),
        "second inbound entry shape wrong: {}",
        lines[3]
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn resume_emits_lifecycle_markers_in_order() {
    // Phase 4.5 contract: `sessions.resume()` walks an explicit emit
    // sequence — `session_end`, `session_start`, `replay.start`,
    // (replayed entries), `replay.end`, `resume_done`. The synchronous
    // `on_resume_phase` hook registry is gone; pure-Lua actors observe
    // these phases by subscribing to the corresponding bus events
    // (`nefor.bus.on_event`). This test asserts the emit order on the
    // wire — that's the surface every consumer now reads against.
    //
    //   1. session_end emit.
    //   2. session_start emit.
    //   3. replay.start emit.
    //   4. (no replay file → no replayed entries here).
    //   5. replay.end emit.
    //   6. resume_done emit.
    //
    // We monkey-patch `nefor.engine.send` to capture every emission's
    // kind into a trace; the order assertion below is the contract.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    // Replace engine.send with a recorder that decodes the payload and
    // appends `emit:<kind>` to the trace.
    lua.load(
        r#"
        _trace = {}
        local json = nefor.json
        nefor.engine.send = function(payload, _target)
            local ok, decoded = pcall(json.decode, payload)
            if ok and type(decoded) == "table"
               and type(decoded.body) == "table"
               and type(decoded.body.kind) == "string" then
                _trace[#_trace + 1] = "emit:" .. decoded.body.kind
            end
        end
        "#,
    )
    .exec()
    .expect("install send recorder");

    lua.load(
        r#"
        sessions = require("sessions")
        sessions_test = require("sessions.test")
        sessions.init()

        -- Clear the trace so we only see what resume() does.
        _trace = {}

        -- Drive a resume to a fresh id (target file doesn't exist, so
        -- replay_jsonl is a no-op — exactly what we want for the order
        -- assertion).
        sessions.resume("11111111-2222-4333-8444-555555555555")
        "#,
    )
    .exec()
    .expect("drive resume");

    let trace: Table = lua.globals().get("_trace").expect("_trace");
    let len = trace.len().expect("len") as usize;
    let ordered: Vec<String> = (1..=len)
        .map(|i| trace.get::<String>(i).expect("trace entry"))
        .collect();
    let expected = vec![
        "emit:sessions.session_end",
        "emit:sessions.session_start",
        "emit:sessions.replay.start",
        "emit:sessions.replay.end",
        "emit:sessions.resume_done",
    ];
    assert_eq!(
        ordered.iter().map(String::as_str).collect::<Vec<_>>(),
        expected,
        "resume must emit lifecycle markers in this order; got {ordered:?}"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn shutdown_prunes_truly_empty_session_preserves_session_with_any_envelope() {
    // Picker-clutter cleanup: a session that boots and quits without
    // any envelope being persisted has nothing worth keeping, so it's
    // deleted on shutdown. Once any envelope lands in the jsonl, the
    // session is preserved.
    //
    //   (a) no envelopes persisted → pruned.
    //   (b) at least one envelope persisted → preserved.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    // (a) empty session.
    {
        let lua = Lua::new();
        install_stub_nefor(&lua).expect("install nefor stub");
        set_package_path(&lua).expect("set package.path");
        lua.load(
            r#"
            sessions = require("sessions")
            sessions_test = require("sessions.test")
            sessions.init()
            "#,
        )
        .exec()
        .expect("drive init");
        let path: String = lua
            .load(r#"return sessions.current_path()"#)
            .eval()
            .expect("current_path");
        assert!(
            std::path::Path::new(&path).exists(),
            "session file should exist before shutdown: {path}"
        );
        lua.load(r#"sessions_test._on_engine_shutdown(nil)"#)
            .exec()
            .expect("drive shutdown");
        assert!(
            !std::path::Path::new(&path).exists(),
            "truly empty session must be pruned on shutdown: {path}"
        );
    }

    // (b) session with at least one persisted envelope.
    {
        let lua = Lua::new();
        install_stub_nefor(&lua).expect("install nefor stub");
        set_package_path(&lua).expect("set package.path");
        lua.load(
            r#"
            sessions = require("sessions")
            sessions_test = require("sessions.test")
            sessions.init()
            local json = nefor.json
            sessions_test._persist_envelope({
                ts      = "2026-05-04T00:00:00.000Z",
                origin  = "nefor-combinators",
                payload = json.encode({ type = "event", body = { kind = "combinators.hello" } }),
            })
            "#,
        )
        .exec()
        .expect("drive activity");
        let path: String = lua
            .load(r#"return sessions.current_path()"#)
            .eval()
            .expect("current_path");
        lua.load(r#"sessions_test._on_engine_shutdown(nil)"#)
            .exec()
            .expect("drive shutdown");
        assert!(
            std::path::Path::new(&path).exists(),
            "session with any persisted envelope must be preserved on shutdown: {path}"
        );
    }

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn new_mints_fresh_session_and_prunes_empty_outgoing() {
    // `/new` regression: typing `/new` only cleared the transcript
    // visually; the on-disk session id stayed put so every subsequent
    // submit kept landing in the same jsonl. The picker therefore only
    // ever showed one growing entry, no matter how many `/new`s the
    // user typed. The fix is `sessions.new()` (driven by the
    // `sessions.new_request` bus event) — mints a fresh id, runs the
    // resume lifecycle (end → swap → start → resume_done), and prunes
    // the outgoing file when it had no submits. We assert (a) the id
    // changed, (b) the new file exists with a header, (c) the empty
    // outgoing file is gone.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    lua.load(
        r#"
        sessions = require("sessions")
        sessions_test = require("sessions.test")
        sessions.init()
        "#,
    )
    .exec()
    .expect("sessions init");

    let boot_id: String = lua
        .load(r#"return sessions.current_id()"#)
        .eval()
        .expect("boot id");
    let boot_path: String = lua
        .load(r#"return sessions.current_path()"#)
        .eval()
        .expect("boot path");
    assert!(
        std::path::Path::new(&boot_path).exists(),
        "boot session file should exist"
    );

    // Drive `/new` via the bus listener entry point so the test
    // exercises the same path the chat surface hits.
    lua.load(r#"sessions_test._on_new_request(nil)"#)
        .exec()
        .expect("drive new_request");

    let new_id: String = lua
        .load(r#"return sessions.current_id()"#)
        .eval()
        .expect("new id");
    let new_path: String = lua
        .load(r#"return sessions.current_path()"#)
        .eval()
        .expect("new path");
    assert_ne!(boot_id, new_id, "/new must mint a fresh id");
    assert!(
        std::path::Path::new(&new_path).exists(),
        "new session file should exist after /new"
    );
    assert!(
        !std::path::Path::new(&boot_path).exists(),
        "outgoing empty session file should be pruned by /new: {boot_path}"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn resume_to_existing_session_appends_in_order() {
    // After `M.resume(target)` swaps to an existing file, subsequent
    // persisted envelopes must APPEND — not overwrite the header, not
    // duplicate the prior content. This test prepopulates a target file
    // with one submit, resumes into it, drives a second submit through
    // the persistence hook, and asserts the file ends with both
    // submits in the original order.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let target_id = "44444444-2222-4333-8444-555555555555";
    let sessions_dir = tempdir.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("mkdir");
    let target_path = sessions_dir.join(format!("{target_id}.jsonl"));
    let preexisting = format!(
        concat!(
            r#"{{"_session":true,"session_id":"{id}","started_at":"2026-05-04T00:00:00.000Z"}}"#,
            "\n",
            r#"{{"ts":"2026-05-04T00:00:00.001Z","origin":"nefor-tui","payload":"{{\"type\":\"event\",\"body\":{{\"kind\":\"chat.input.submit\",\"text\":\"first\"}}}}"}}"#,
            "\n",
        ),
        id = target_id
    );
    std::fs::write(&target_path, &preexisting).expect("seed target file");

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    lua.load(
        r#"
        sessions = require("sessions")
        sessions_test = require("sessions.test")
        sessions.init()
        "#,
    )
    .exec()
    .expect("init");

    // Resume into the seeded file.
    let resume_call = format!(r#"sessions.resume("{target_id}")"#);
    lua.load(&resume_call).exec().expect("resume");

    // Drive a second submit through the persistence hook. The active
    // file is now the target — write should append.
    lua.load(
        r#"
        local json = nefor.json
        sessions_test._persist_envelope({
            ts      = "2026-05-04T00:00:01.000Z",
            origin  = "nefor-tui",
            payload = json.encode({ type = "event", body = { kind = "chat.input.submit", text = "second" } }),
        })
        "#,
    )
    .exec()
    .expect("drive second submit");

    let body = std::fs::read_to_string(&target_path).expect("read target");
    let lines: Vec<&str> = body.lines().collect();
    // header + first + second.
    assert!(lines.len() >= 3, "expected ≥3 lines, got {body}");
    let last = lines[lines.len() - 1];
    let prior = lines[lines.len() - 2];
    // Payload is a JSON-encoded string, so the inner `"text":"X"` is
    // written as `\"text\":\"X\"` in the row's bytes. Match that.
    assert!(
        last.contains(r#"\"text\":\"second\""#),
        "last line must be the second submit: {last}"
    );
    assert!(
        prior.contains(r#"\"text\":\"first\""#),
        "second-to-last line must be the preexisting first submit \
         (resume must not corrupt the prior content): {prior}"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn resume_to_self_replays_log_so_chat_repaints() {
    // Regression for: "/resume the active session leaves the chat blank."
    //
    // chat.lua's `/resume <id>` and picker handlers locally clear
    // `entries` BEFORE emitting `sessions.resume_request`, expecting the
    // imminent replay to repaint the transcript. The old contract had
    // `do_resume(current_id)` early-return as a no-op (protecting against
    // a duplicate resume_request tearing down state mid-turn). Result:
    // chat cleared, sessions did nothing, transcript stayed empty.
    //
    // The new contract: same-id resume cycles the full lifecycle —
    // session_end, session_start, replay.start, replay envelopes from
    // disk, replay.end, resume_done — so the chat's pre-cleared
    // transcript is rebuilt from the on-disk log. The "duplicate click"
    // case is covered by chat.lua already: each click clears entries and
    // each subsequent resume re-fills them; the final state matches the
    // log. Re-replay is idempotent at the chat-state level (entries are
    // rebuilt from `chat.message.append` envelopes either way).
    //
    // This test pins:
    //   1. the lifecycle markers fire in the standard order
    //   2. the on-disk persisted entry (a chat.input.submit) is replayed
    //      to its original target (broadcast, target=nil here).
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    // Recorder captures every emission's body.kind so we can assert the
    // lifecycle order AND the replayed-content kind.
    lua.load(
        r#"
        _trace = {}
        local json = nefor.json
        nefor.engine.send = function(payload, _target)
            local ok, decoded = pcall(json.decode, payload)
            if ok and type(decoded) == "table"
               and type(decoded.body) == "table"
               and type(decoded.body.kind) == "string" then
                _trace[#_trace + 1] = "emit:" .. decoded.body.kind
            end
        end
        sessions = require("sessions")
        sessions_test = require("sessions.test")
        sessions.init()

        -- Persist one chat.input.submit so replay has something to
        -- re-emit. Use the test handle that drives persist_envelope
        -- directly with a step-origin entry.
        sessions_test._persist_envelope({
            ts      = "2026-05-04T00:00:01.000Z",
            origin  = "step",
            payload = json.encode({
                type = "event",
                from = "nefor-tui",
                body = { kind = "chat.input.submit", text = "hello" },
            }),
        })

        -- Clear the trace so we only see what resume() does.
        _trace = {}
        "#,
    )
    .exec()
    .expect("init + recorder + seed");

    let id: String = lua
        .load(r#"return sessions.current_id()"#)
        .eval()
        .expect("current_id");
    let resume_call = format!(r#"sessions.resume("{id}")"#);
    lua.load(&resume_call).exec().expect("resume to self");

    let trace: Table = lua.globals().get("_trace").expect("_trace");
    let len = trace.len().expect("len") as usize;
    let ordered: Vec<String> = (1..=len)
        .map(|i| trace.get::<String>(i).expect("trace entry"))
        .collect();
    let expected = vec![
        "emit:sessions.session_end",
        "emit:sessions.session_start",
        "emit:sessions.replay.start",
        // The replayed entry — chat.input.submit was the seeded payload.
        "emit:chat.input.submit",
        "emit:sessions.replay.end",
        "emit:sessions.resume_done",
    ];
    assert_eq!(
        ordered.iter().map(String::as_str).collect::<Vec<_>>(),
        expected,
        "same-session resume must cycle the full lifecycle and replay \
         persisted entries so chat.lua's pre-cleared transcript repaints; \
         got {ordered:?}"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn resume_to_nonexistent_session_succeeds_with_zero_replayed() {
    // Resume's contract: the swap always happens (we own the new id)
    // even if the target file is missing. The bus broadcast carries
    // `replayed = 0` and a fresh file is created. This is what makes a
    // hand-typed `/resume <random-uuid>` recoverable rather than wedging
    // the engine.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let target_id = "55555555-2222-4333-8444-555555555555";
    let resume_call = format!(
        r#"
        _resume_done_payload = nil
        local original_send = nefor.engine.send
        local json = nefor.json
        nefor.engine.send = function(payload, target)
            local ok, decoded = pcall(json.decode, payload)
            if ok and decoded and decoded.body
               and decoded.body.kind == "sessions.resume_done" then
                _resume_done_payload = payload
            end
            return original_send(payload, target)
        end
        sessions = require("sessions")
        sessions_test = require("sessions.test")
        sessions.init()
        sessions.resume("{target_id}")
        "#
    );
    lua.load(&resume_call).exec().expect("resume to fresh");

    let new_id: String = lua
        .load(r#"return sessions.current_id()"#)
        .eval()
        .expect("current_id");
    assert_eq!(new_id, target_id, "swap must complete to target id");

    let new_path: String = lua
        .load(r#"return sessions.current_path()"#)
        .eval()
        .expect("current_path");
    assert!(
        std::path::Path::new(&new_path).exists(),
        "fresh file should exist with header at {new_path}"
    );

    let payload: Option<String> = lua
        .load(r#"return _resume_done_payload"#)
        .eval()
        .expect("payload");
    let payload = payload.expect("resume_done must broadcast");
    assert!(
        payload.contains("\"replayed\":0"),
        "resume_done must report replayed=0 for missing target file: {payload}"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

// `data_root_resolves_xdg_then_home_fallback` was deleted alongside the
// per-call Lua-side env-var resolver. sessions.lua now delegates to
// `nefor.fs.data_root()`, which snapshots the engine's resolved DataDir
// at install time — Rust-side `config::tests::data_*` already pins the
// CLI flag > NEFOR_DATA_DIR > XDG_DATA_HOME precedence the deleted test
// duplicated through the Lua layer.

#[test]
fn new_then_new_prunes_each_empty_predecessor() {
    // Repeated `/new` without typing must not leave a trail of empty
    // stubs. Each cycle prunes the prior file; the picker stays clean.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    lua.load(
        r#"
        sessions = require("sessions")
        sessions_test = require("sessions.test")
        sessions.init()
        sessions_test._on_new_request(nil)  -- /new #1
        sessions_test._on_new_request(nil)  -- /new #2
        sessions_test._on_new_request(nil)  -- /new #3
        "#,
    )
    .exec()
    .expect("triple-new");

    let sessions_dir = tempdir.path().join("sessions");
    let mut entries: Vec<_> = std::fs::read_dir(&sessions_dir)
        .expect("read sessions dir")
        .filter_map(|r| r.ok())
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    entries.sort_by_key(|e| e.path());
    // Only the latest active session file should remain — three
    // predecessors must have been pruned.
    assert_eq!(
        entries.len(),
        1,
        "expected one active session file, got {}: {:?}",
        entries.len(),
        entries.iter().map(|e| e.path()).collect::<Vec<_>>(),
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn new_after_submit_preserves_prior_session() {
    // Asymmetric prune: a session with at least one user submit must
    // SURVIVE `/new`. Otherwise the user's first session of the day
    // would vanish the moment they typed `/new`.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    lua.load(
        r#"
        sessions = require("sessions")
        sessions_test = require("sessions.test")
        sessions.init()
        local json = nefor.json
        -- Real user submit in the boot session.
        sessions_test._persist_envelope({
            ts      = "2026-05-04T00:00:00.000Z",
            origin  = "nefor-tui",
            payload = json.encode({ type = "event", body = { kind = "chat.input.submit", text = "ship-it" } }),
        })
        _boot_path = sessions.current_path()
        sessions_test._on_new_request(nil)
        _new_path = sessions.current_path()
        "#,
    )
    .exec()
    .expect("submit + /new");

    let boot_path: String = lua.load(r#"return _boot_path"#).eval().expect("boot path");
    let new_path: String = lua.load(r#"return _new_path"#).eval().expect("new path");

    assert!(
        std::path::Path::new(&boot_path).exists(),
        "session with prior submit must survive /new: {boot_path}"
    );
    assert!(
        std::path::Path::new(&new_path).exists(),
        "/new must open a fresh file: {new_path}"
    );
    assert_ne!(boot_path, new_path, "/new must mint a different id");

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

#[test]
fn replay_window_frames_resume_and_skips_persistence_inside() {
    // Phase 4: sessions emits `sessions.replay.start` / `replay.end`
    // around the replay loop, and persistence is suppressed inside the
    // window. Together these let pure-Lua actors process replays via
    // `nefor.bus.on_event` without re-recording derived emissions onto
    // disk (which would duplicate state on the next resume). The
    // markers themselves are NOT persisted (sessions.* filter).
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_DIR").ok();
    std::env::set_var("NEFOR_DATA_DIR", tempdir.path());

    // Pre-seed a target session jsonl with one step-origin entry so
    // replay_jsonl has something to do. The header line carries the
    // `_session: true` marker the resolver expects.
    let target_id = "66666666-2222-4333-8444-555555555555";
    let sessions_dir = tempdir.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("mkdir");
    let target_path = sessions_dir.join(format!("{target_id}.jsonl"));
    let preseed = format!(
        concat!(
            r#"{{"_session":true,"session_id":"{id}","started_at":"2026-05-04T00:00:00.000Z"}}"#,
            "\n",
            r#"{{"ts":"2026-05-04T00:00:00.001Z","origin":"step","payload":"{{\"type\":\"event\",\"from\":\"agentic-loop\",\"body\":{{\"kind\":\"chat.message.append\",\"role\":\"user\",\"text\":\"hi\"}}}}"}}"#,
            "\n",
        ),
        id = target_id
    );
    std::fs::write(&target_path, &preseed).expect("seed target");

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    // Record every emission so we can assert ordering of the markers
    // around the replayed envelope.
    lua.load(
        r#"
        _emits = {}
        local json = nefor.json
        nefor.engine.send = function(payload, target)
            local ok, decoded = pcall(json.decode, payload)
            if ok and type(decoded) == "table"
               and type(decoded.body) == "table"
               and type(decoded.body.kind) == "string" then
                _emits[#_emits + 1] = decoded.body.kind
            end
        end
        sessions = require("sessions")
        sessions_test = require("sessions.test")
        sessions.init()
        _emits = {}
        "#,
    )
    .exec()
    .expect("init");

    let resume_call = format!(r#"sessions.resume("{target_id}")"#);
    lua.load(&resume_call).exec().expect("resume");

    let trace: Table = lua.globals().get("_emits").expect("_emits");
    let len = trace.len().expect("len") as usize;
    let ordered: Vec<String> = (1..=len)
        .map(|i| trace.get::<String>(i).expect("entry"))
        .collect();
    let kinds: Vec<&str> = ordered.iter().map(String::as_str).collect();

    // Markers frame the replayed envelope. The replayed envelope
    // (chat.message.append) emerges between replay.start and replay.end.
    let start_idx = kinds
        .iter()
        .position(|k| *k == "sessions.replay.start")
        .expect("replay.start emitted");
    let end_idx = kinds
        .iter()
        .position(|k| *k == "sessions.replay.end")
        .expect("replay.end emitted");
    let replayed_idx = kinds
        .iter()
        .position(|k| *k == "chat.message.append")
        .expect("replayed envelope emerged");
    assert!(
        start_idx < replayed_idx && replayed_idx < end_idx,
        "replayed envelope must fall inside the replay window: {kinds:?}"
    );
    let resume_done_idx = kinds
        .iter()
        .position(|k| *k == "sessions.resume_done")
        .expect("resume_done emitted");
    assert!(
        end_idx < resume_done_idx,
        "replay.end must precede resume_done: {kinds:?}"
    );

    // Persistence skip: drive a derived emission through the persist
    // path while the in_replay_window flag is held true (simulating a
    // pure-Lua actor reacting to a replayed envelope). The marker
    // arrives via receive_msg; toggle it explicitly here so the test
    // doesn't depend on the actor.lua runtime.
    lua.load(
        r#"
        local json = nefor.json
        local function entry(origin, body)
            return {
                ts      = "2026-05-04T00:00:00.000Z",
                origin  = origin,
                payload = json.encode({ type = "event", body = body }),
            }
        end
        -- Open the window via the public bus protocol path.
        sessions.receive_msg(entry("sessions",
            { kind = "sessions.replay.start", session_id = "x", count = 0 }))
        -- A pure-Lua actor's derived emission lands at the persist
        -- handler — must be DROPPED.
        sessions.receive_msg(entry("agentic-loop",
            { kind = "chat.message.append", role = "system", text = "derived" }))
        -- Close the window.
        sessions.receive_msg(entry("sessions",
            { kind = "sessions.replay.end", session_id = "x" }))
        -- A live emission after the window must be PERSISTED.
        sessions.receive_msg(entry("agentic-loop",
            { kind = "chat.message.append", role = "user", text = "live" }))
        "#,
    )
    .exec()
    .expect("drive persistence around window");

    let body = std::fs::read_to_string(&target_path).expect("read target");
    // The persisted file must carry the seed entry + the live entry,
    // but NOT the derived entry from inside the window. Markers are
    // sessions.* — already filtered out by persist_envelope.
    assert!(
        body.contains(r#"\"text\":\"hi\""#),
        "seed entry must remain: {body}"
    );
    assert!(
        body.contains(r#"\"text\":\"live\""#),
        "post-window live entry must be persisted: {body}"
    );
    assert!(
        !body.contains(r#"\"text\":\"derived\""#),
        "in-window derived entry must NOT be persisted: {body}"
    );
    assert!(
        !body.contains("sessions.replay.start") && !body.contains("sessions.replay.end"),
        "replay markers must not be persisted: {body}"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
        None => std::env::remove_var("NEFOR_DATA_DIR"),
    }
}

// Process-global lock to serialise tests that mutate NEFOR_DATA_DIR.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn install_stub_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    nefor::lua::bindings::install_json(lua, &nefor)?;

    // nefor.fs — real binding, with data_dir captured from the env var
    // set by the test. Tests mutate NEFOR_DATA_DIR before calling
    // install_stub_nefor; the binding snapshots that value at install
    // time, matching production semantics.
    let data_dir = std::env::var("NEFOR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/empty/nefor-test-data"));
    nefor::lua::bindings::install_fs(lua, &nefor, nefor::paths::DataDir::new(data_dir))?;

    // log.* — no-op
    let log_tbl = lua.create_table()?;
    let no_op: Function = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    log_tbl.set("info", no_op.clone())?;
    log_tbl.set("warn", no_op.clone())?;
    log_tbl.set("error", no_op.clone())?;
    log_tbl.set("debug", no_op)?;
    nefor.set("log", log_tbl)?;

    // bus.on_event — no-op (sessions.lua's resume_request listener fires
    // through here; this test doesn't need to drive it).
    let bus_tbl = lua.create_table()?;
    let on_event = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    bus_tbl.set("on_event", on_event)?;
    nefor.set("bus", bus_tbl)?;

    // events.on — no-op (handle_shutdown uses this).
    let events_tbl = lua.create_table()?;
    let events_on = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    events_tbl.set("on", events_on)?;
    nefor.set("events", events_tbl)?;

    // engine.{send, plugins, now}
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

fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let starter = starter_dir();
    let starter_str = starter.display().to_string();
    let lua_root = lua_dir();
    let lua_root_str = lua_root.display().to_string();
    let rg_plugin_lua = repo_root()
        .join("plugins")
        .join("reasoner-graph")
        .join("lua");
    let rg_plugin_lua_str = rg_plugin_lua.display().to_string();
    // `tests/lua/?.lua` so `require("sessions.test")` resolves to the
    // test-only escape hatch at `tests/lua/sessions/test.lua` — the file
    // is not shipped under starter/ to keep the installed config pure.
    let tests_lua = repo_root().join("tests").join("lua");
    let tests_lua_str = tests_lua.display().to_string();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          "{rg_plugin_lua}/?.lua",
          "{rg_plugin_lua}/?/init.lua",
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          "{tests_lua}/?.lua",
          "{tests_lua}/?/init.lua",
          package.path,
        }}, ";")
        "#,
        starter = starter_str,
        lua_root = lua_root_str,
        rg_plugin_lua = rg_plugin_lua_str,
        tests_lua = tests_lua_str,
    );
    lua.load(&script).exec()
}
