//! Unit tests for `starter/sessions.lua`'s persistence + control-event
//! filtering, driven from Rust. Mirrors the `starter_ncp_test.rs`
//! harness pattern: install a stub `nefor.*` surface, point
//! NEFOR_DATA_HOME at a tempdir, then exercise the module directly.

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
    let prev = std::env::var("NEFOR_DATA_HOME").ok();
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("NEFOR_DATA_HOME", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    // Initialise the sessions module (mints a session id, opens jsonl).
    lua.load(
        r#"
        sessions = require("sessions")
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
        sessions._persist_envelope(entry("ollama", { kind = "chat.message.append", role = "user", text = "hi" }))
        -- Session control event — must be DROPPED.
        sessions._persist_envelope(entry("engine", { kind = "sessions.resume_request", session_id = "x" }))
        sessions._persist_envelope(entry("engine", { kind = "sessions.session_end", session_id = "x" }))
        sessions._persist_envelope(entry("engine", { kind = "sessions.session_start", session_id = "y" }))
        sessions._persist_envelope(entry("engine", { kind = "sessions.resume_done", session_id = "y" }))
        -- Another normal entry.
        sessions._persist_envelope(entry("nefor-tui", { kind = "chat.input.submit", text = "hello" }))
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
        Some(v) => std::env::set_var("NEFOR_DATA_HOME", v),
        None => std::env::remove_var("NEFOR_DATA_HOME"),
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
    let prev = std::env::var("NEFOR_DATA_HOME").ok();
    std::env::set_var("NEFOR_DATA_HOME", tempdir.path());

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    lua.load(
        r#"
        sessions = require("sessions")
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
        sessions._persist_envelope(entry("nefor-tui", nil,
            { kind = "chat.input.submit", text = "hello" }))
        sessions._persist_envelope(entry("step", "nefor-tui",
            { kind = "chat.message.append", role = "user", text = "hello" }))
        sessions._persist_envelope(entry("ollama", nil,
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
        Some(v) => std::env::set_var("NEFOR_DATA_HOME", v),
        None => std::env::remove_var("NEFOR_DATA_HOME"),
    }
}

#[test]
fn resume_phase_hooks_fire_synchronously_before_emit() {
    // The one-tick replay-leak fix: `sessions.on_resume_phase(phase, fn)`
    // registers a synchronous callback that fires INSIDE
    // `sessions.resume()` before the corresponding `emit_control`
    // broadcast. Asserts the order:
    //   1. session_end hook runs.
    //   2. session_end emit.
    //   3. session_start hook runs.
    //   4. session_start emit.
    //   5. (no replay file → no replayed entries here).
    //   6. resume_done hook runs.
    //   7. resume_done emit.
    //
    // We verify by recording timestamps in a Lua-side trace log: each
    // hook records "phase:<name>", and we monkey-patch `nefor.engine.send`
    // to also append "emit:<kind>". The final order tells us the sync
    // hook ran before the broadcast.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_HOME").ok();
    std::env::set_var("NEFOR_DATA_HOME", tempdir.path());

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
        sessions.init()

        sessions.on_resume_phase("session_end", function(_id)
            _trace[#_trace + 1] = "phase:session_end"
        end)
        sessions.on_resume_phase("session_start", function(_id)
            _trace[#_trace + 1] = "phase:session_start"
        end)
        sessions.on_resume_phase("resume_done", function(_id)
            _trace[#_trace + 1] = "phase:resume_done"
        end)

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
        "phase:session_end",
        "emit:sessions.session_end",
        "phase:session_start",
        "emit:sessions.session_start",
        "phase:resume_done",
        "emit:sessions.resume_done",
    ];
    assert_eq!(
        ordered.iter().map(String::as_str).collect::<Vec<_>>(),
        expected,
        "phase hooks must fire BEFORE the corresponding emit; got {ordered:?}"
    );

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_HOME", v),
        None => std::env::remove_var("NEFOR_DATA_HOME"),
    }
}

#[test]
fn resume_replay_targets_only_tui_with_step_origin_entries() {
    // Issue 1 regression: the replay path used to broadcast plugin-
    // origin entries (raw `<provider-prefix>.stream.delta` etc.)
    // verbatim via `nefor.engine.send(payload)` — which bypasses the
    // per-edge `for_provider.from_plugin` translation, so nefor-tui
    // (which speaks `chat.*`) saw nothing match its handlers and the
    // transcript stayed empty after `/resume`. The fix is to replay
    // STEP-ORIGIN entries (post-transform per-peer fan-out the broker
    // captured in the live session) targeted at `nefor-tui` only —
    // side-effecting plugins (provider, reasoner-graph, tool-gate)
    // should never see replay traffic.
    //
    // We synthesise a session log with one of each origin/target shape
    // and a header, drive `sessions.resume()`, and assert (a) the
    // engine.send calls that came out target only `nefor-tui`, (b) the
    // payloads are the step-origin chat.* envelopes, never the raw
    // `<prefix>.stream.delta`.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_HOME").ok();
    std::env::set_var("NEFOR_DATA_HOME", tempdir.path());

    // Pre-seed a session jsonl on disk with a representative mix:
    //   * header
    //   * raw plugin-origin chat.input.submit (origin nefor-tui)
    //   * step-origin chat.message.append targeted nefor-tui (a user echo)
    //   * step-origin chat.stream.delta targeted nefor-tui
    //   * step-origin chat.stream.delta targeted reasoner-graph (NOT replayed)
    //   * raw plugin-origin ollama.stream.delta (NOT replayed)
    let target_id = "11111111-2222-4333-8444-555555555555";
    // sessions.lua's `data_root()` uses NEFOR_DATA_HOME as-is (no
    // `/nefor` suffix — that's the XDG fallback path). Sessions live
    // at `<root>/sessions/<id>.jsonl`.
    let sessions_dir = tempdir.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("mkdir sessions");
    let session_path = sessions_dir.join(format!("{target_id}.jsonl"));
    // Each line is a JSON document the engine's session writer (or
    // `sessions._persist_envelope`) would have written.
    let body = concat!(
        // header
        r#"{"_session":true,"session_id":"11111111-2222-4333-8444-555555555555","started_at":"2026-05-04T00:00:00.000Z"}"#,
        "\n",
        // plugin-origin raw inbound — must NOT be replayed
        r#"{"ts":"2026-05-04T00:00:00.001Z","origin":"nefor-tui","payload":"{\"type\":\"event\",\"body\":{\"kind\":\"chat.input.submit\",\"text\":\"hi\"}}"}"#,
        "\n",
        // step-origin chat.message.append targeted nefor-tui — MUST be replayed
        r#"{"ts":"2026-05-04T00:00:00.002Z","origin":"step","target":"nefor-tui","payload":"{\"ts\":\"2026-05-04T00:00:00.002Z\",\"type\":\"event\",\"from\":\"engine\",\"body\":{\"kind\":\"chat.message.append\",\"role\":\"user\",\"text\":\"hi\"}}"}"#,
        "\n",
        // step-origin chat.stream.delta targeted nefor-tui — MUST be replayed
        r#"{"ts":"2026-05-04T00:00:00.003Z","origin":"step","target":"nefor-tui","payload":"{\"ts\":\"2026-05-04T00:00:00.003Z\",\"type\":\"event\",\"from\":\"ollama\",\"body\":{\"kind\":\"chat.stream.delta\",\"text\":\"Hello\"}}"}"#,
        "\n",
        // step-origin chat.stream.delta targeted reasoner-graph — MUST NOT be replayed
        r#"{"ts":"2026-05-04T00:00:00.004Z","origin":"step","target":"reasoner-graph","payload":"{\"ts\":\"2026-05-04T00:00:00.004Z\",\"type\":\"event\",\"from\":\"ollama\",\"body\":{\"kind\":\"chat.stream.delta\",\"text\":\"Hello\"}}"}"#,
        "\n",
        // plugin-origin raw ollama.stream.delta — MUST NOT be replayed
        r#"{"ts":"2026-05-04T00:00:00.005Z","origin":"ollama","payload":"{\"type\":\"event\",\"body\":{\"kind\":\"ollama.stream.delta\",\"chat_id\":\"c1\",\"text\":\"Hello\"}}"}"#,
        "\n",
    );
    std::fs::write(&session_path, body).expect("write session");

    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    // Replace engine.send with a recorder. The control-event broadcasts
    // (sessions.session_end / start / resume_done) come through too; we
    // capture all of them and assert on the replay-specific subset.
    lua.load(
        r#"
        _sends = {}
        nefor.engine.send = function(payload, target)
            _sends[#_sends + 1] = { payload = payload, target = target }
        end
        "#,
    )
    .exec()
    .expect("install send recorder");

    lua.load(format!(
        r#"
        sessions = require("sessions")
        sessions.init()
        sessions.resume("{target_id}")
        "#,
        target_id = target_id,
    ))
    .exec()
    .expect("drive resume");

    let sends_tbl: Table = lua.globals().get("_sends").expect("_sends");
    let len = sends_tbl.len().expect("len") as usize;
    let mut chat_kind_sends: Vec<(String, Option<String>)> = Vec::new();
    for i in 1..=len {
        let entry: Table = sends_tbl.get(i).expect("entry");
        let payload: String = entry.get("payload").expect("payload");
        let target: Option<String> = match entry.get::<Value>("target").expect("target") {
            Value::String(s) => Some(s.to_str().expect("utf8").to_owned()),
            _ => None,
        };
        // Look at the body.kind. Skip control events (sessions.*).
        let v: serde_json::Value = match serde_json::from_str(&payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let kind = v
            .get("body")
            .and_then(|b| b.get("kind"))
            .and_then(|k| k.as_str())
            .unwrap_or("")
            .to_owned();
        if kind.starts_with("sessions.") {
            continue;
        }
        chat_kind_sends.push((kind, target));
    }

    // Replay should have produced exactly one send — the
    // chat.message.append. The chat.stream.delta is dropped by the
    // replay's REPLAY_DROP_KINDS filter (resume is instant: deltas
    // are subsumed by their stream.end finalizer, so re-streaming
    // them would just re-render every token live).
    assert_eq!(
        chat_kind_sends.len(),
        1,
        "replay sent unexpected non-control envelopes: {chat_kind_sends:?}",
    );
    for (kind, target) in &chat_kind_sends {
        assert_eq!(
            target.as_deref(),
            Some("nefor-tui"),
            "replay must target nefor-tui only; saw kind={kind} target={target:?}",
        );
        assert_eq!(
            kind, "chat.message.append",
            "replay carried unexpected kind {kind}",
        );
    }
    // Belt-and-braces: stream deltas are dropped, raw provider-prefix
    // kinds must not appear, and reasoner-graph is never targeted.
    for (kind, _) in &chat_kind_sends {
        assert_ne!(
            kind, "chat.stream.delta",
            "chat.stream.delta must be filtered out of replay (instant resume)",
        );
        assert!(
            !kind.starts_with("ollama."),
            "raw provider-prefix kind leaked into replay: {kind}",
        );
    }

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_HOME", v),
        None => std::env::remove_var("NEFOR_DATA_HOME"),
    }
}

#[test]
fn shutdown_prunes_session_with_no_user_submits() {
    // Picker-clutter regression: sessions that boot, run handshake
    // (combinators.hello, chat.model.set_ack, etc.), and quit without
    // a single `chat.input.submit` used to stick around as `(no
    // submits)` ghost rows in the picker. The fix is to count submits
    // (matching the picker's preview filter) rather than every non-
    // control envelope, then delete the file on shutdown when the
    // count is zero.
    //
    // Two shapes here:
    //   (a) handshake-only — must be pruned.
    //   (b) one chat.input.submit — must be preserved.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("NEFOR_DATA_HOME").ok();
    std::env::set_var("NEFOR_DATA_HOME", tempdir.path());

    // (a) handshake-only session.
    {
        let lua = Lua::new();
        install_stub_nefor(&lua).expect("install nefor stub");
        set_package_path(&lua).expect("set package.path");
        lua.load(
            r#"
            sessions = require("sessions")
            sessions.init()
            local json = nefor.json
            local function entry(origin, body)
                return {
                    ts      = "2026-05-04T00:00:00.000Z",
                    origin  = origin,
                    payload = json.encode({ type = "event", body = body }),
                }
            end
            -- Realistic handshake traffic — no chat.input.submit.
            sessions._persist_envelope(entry("nefor-combinators", { kind = "combinators.hello", version = "0.1.0" }))
            sessions._persist_envelope(entry("nefor-combinators", { kind = "combinators.ready" }))
            sessions._persist_envelope(entry("ollama", { kind = "chat.model.set_ack", model = "x", provider = "ollama" }))
            "#,
        )
        .exec()
        .expect("drive handshake");
        let path: String = lua
            .load(r#"return sessions.current_path()"#)
            .eval()
            .expect("current_path");
        assert!(
            std::path::Path::new(&path).exists(),
            "session file should exist before shutdown: {path}"
        );
        lua.load(r#"sessions._on_engine_shutdown(nil)"#)
            .exec()
            .expect("drive shutdown");
        assert!(
            !std::path::Path::new(&path).exists(),
            "handshake-only session must be pruned on shutdown: {path}"
        );
    }

    // (b) session with a real submit.
    {
        let lua = Lua::new();
        install_stub_nefor(&lua).expect("install nefor stub");
        set_package_path(&lua).expect("set package.path");
        lua.load(
            r#"
            sessions = require("sessions")
            sessions.init()
            local json = nefor.json
            local function entry(origin, body)
                return {
                    ts      = "2026-05-04T00:00:00.000Z",
                    origin  = origin,
                    payload = json.encode({ type = "event", body = body }),
                }
            end
            sessions._persist_envelope(entry("nefor-combinators", { kind = "combinators.hello" }))
            sessions._persist_envelope(entry("nefor-tui", { kind = "chat.input.submit", text = "hi" }))
            "#,
        )
        .exec()
        .expect("drive activity");
        let path: String = lua
            .load(r#"return sessions.current_path()"#)
            .eval()
            .expect("current_path");
        lua.load(r#"sessions._on_engine_shutdown(nil)"#)
            .exec()
            .expect("drive shutdown");
        assert!(
            std::path::Path::new(&path).exists(),
            "session with chat.input.submit must be preserved on shutdown: {path}"
        );
    }

    match prev.as_deref() {
        Some(v) => std::env::set_var("NEFOR_DATA_HOME", v),
        None => std::env::remove_var("NEFOR_DATA_HOME"),
    }
}

// Process-global lock to serialise tests that mutate NEFOR_DATA_HOME.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn install_stub_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    nefor::lua::bindings::install_json(lua, &nefor)?;

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
