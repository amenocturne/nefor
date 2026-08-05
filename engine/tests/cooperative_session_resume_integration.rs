use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nefor::events::EventBus;
use nefor::lua::bindings::EngineOps;
use nefor::lua::LuaHost;
use nefor::ncp::transport::Transport;
use nefor::ncp::{Broker, BrokerOps, BrokerShared, PluginRegistry};
use nefor::paths::DataDir;
use nefor_protocol::PluginName;
use nefor_tui::engine::Engine as TuiEngine;
use serde_json::{json, Map as JsonMap, Value as JsonValue};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const SESSION_ID: &str = "cooperative-resume";
const REPLAY_ENTRIES: usize = 130;
const REPLAY_MESSAGES: usize = 42;
const CONVERSATION_ID: &str = "cooperative-conversation";

fn format_replay_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn lua_string(path: &Path) -> String {
    serde_json::to_string(&path.display().to_string()).expect("path is JSON-encodable")
}

fn write_session_fixture(data_dir: &Path) {
    let sessions_dir = data_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("create sessions fixture directory");
    let mut lines = vec![serde_json::to_string(&json!({
        "_session": true,
        "session_id": SESSION_ID,
        "started_at": "2026-07-15T00:00:00.000Z"
    }))
    .expect("serialize header")];

    let mut facts = vec![
        json!({
            "kind": "created",
            "event_id": "fixture-created",
            "conversation_id": CONVERSATION_ID,
            "provenance": { "surface": "lead" },
        }),
        json!({
            "kind": "active_selected",
            "event_id": "fixture-active",
            "conversation_id": CONVERSATION_ID,
        }),
        json!({
            "kind": "provenance_updated",
            "event_id": "fixture-provider",
            "conversation_id": CONVERSATION_ID,
            "provenance": { "provider": "mock-plugin" },
        }),
        json!({
            "kind": "provenance_updated",
            "event_id": "fixture-model",
            "conversation_id": CONVERSATION_ID,
            "provenance": { "model": "mock-model" },
        }),
    ];
    for index in 0..REPLAY_MESSAGES {
        let message_id = format!("fixture-message-{index}");
        facts.push(json!({
            "kind": "message_started",
            "event_id": format!("{message_id}-started"),
            "conversation_id": CONVERSATION_ID,
            "message_id": message_id,
            "role": "user",
        }));
        facts.push(json!({
            "kind": "content_chunk_appended",
            "event_id": format!("{message_id}-content"),
            "conversation_id": CONVERSATION_ID,
            "message_id": message_id,
            "chunk": { "kind": "text", "data": format!("replayed message {index}") },
        }));
        facts.push(json!({
            "kind": "message_completed",
            "event_id": format!("{message_id}-completed"),
            "conversation_id": CONVERSATION_ID,
            "message_id": message_id,
        }));
    }
    assert_eq!(facts.len(), REPLAY_ENTRIES);

    for (index, mut event) in facts.into_iter().enumerate() {
        event["sequence"] = json!(index + 1);
        let payload = serde_json::to_string(&json!({
            "type": "event",
            "from": "conversation-manager",
            "ts": "2026-07-15T00:00:00.000Z",
            "body": {
                "kind": "conversation.fact.recorded",
                "duplicate": false,
                "session_id": SESSION_ID,
                "event": event,
            }
        }))
        .expect("serialize replay envelope");
        lines.push(
            serde_json::to_string(&json!({
                "ts": "2026-07-15T00:00:00.000Z",
                "origin": "step",
                "payload": payload
            }))
            .expect("serialize session row"),
        );
    }

    std::fs::write(
        sessions_dir.join(format!("{SESSION_ID}.jsonl")),
        format!("{}\n", lines.join("\n")),
    )
    .expect("write session fixture");
}

fn engine_init_source(root: &Path) -> String {
    let lua = lua_string(&root.join("lua"));
    format!(
        r#"
        package.path = table.concat({{
          {lua} .. "/?.lua",
          {lua} .. "/?/init.lua",
          package.path,
        }}, ";")

        local ncp = require("core.ncp")
        local actor = require("core.actor")
        local replay_window = require("core.replay_window")
        local sessions = require("libs.sessions")

        function dispatch(current_log) ncp.dispatch(current_log) end
        function invoke_from_plugin(source, payload)
          ncp.invoke_from_plugin(source, payload)
        end
        function invoke_from_plugin_batch(source, payloads)
          ncp.invoke_from_plugin_batch(source, payloads)
        end

        actor.install()
        replay_window.install()
        actor.spawn(sessions)
        actor.spawn(require("libs.conversation-manager.runtime").build())
        actor.spawn(require("libs.compositors.chat_bridge").spawn_spec({{ "nefor-tui" }}))
        sessions.init()
        "#
    )
}

fn tui_source(root: &Path, data_dir: &Path) -> String {
    let source = std::fs::read_to_string(root.join("starter/chat/init.lua"))
        .expect("read starter chat composition");
    let overrides = format!(
        r#"{{
          NEFOR_CONFIG_DIR = {},
          NEFOR_STARTER_CONFIG_DIR = {},
          NEFOR_STARTER_CHAT_DIR = {},
          NEFOR_TUI_LUA_DIR = {},
          NEFOR_LUA_DIR = {},
          NEFOR_DATA_DIR = {},
          NEFOR_DEFAULT_PROVIDER = "mock-plugin",
          NEFOR_DEFAULT_MODEL = "mock-model",
        }}"#,
        lua_string(&root.join("starter")),
        lua_string(&root.join("starter")),
        lua_string(&root.join("starter/chat")),
        lua_string(&root.join("plugins/nefor-tui/lua")),
        lua_string(&root.join("lua")),
        lua_string(data_dir),
    );
    format!(
        r#"
        local real_getenv = os.getenv
        local overrides = {overrides}
        os.getenv = function(name)
          local value = overrides[name]
          if value ~= nil then return value end
          return real_getenv(name)
        end
        {source}
        "#
    )
}

fn transport_pair() -> (
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    Transport,
) {
    let (plugin_side, broker_side) = tokio::io::duplex(4 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
    let (broker_read, broker_write) = tokio::io::split(broker_side);
    (
        plugin_write,
        BufReader::new(plugin_read),
        Transport {
            reader: Box::pin(broker_read),
            writer: Box::pin(broker_write),
            stderr: None,
            exit: None,
        },
    )
}

async fn send_line(writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, value: JsonValue) {
    writer
        .write_all(format!("{value}\n").as_bytes())
        .await
        .expect("write plugin line");
}

async fn read_line(
    reader: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
) -> JsonValue {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for broker delivery")
        .expect("read broker delivery");
    assert!(
        !line.is_empty(),
        "broker transport closed before resume completed"
    );
    serde_json::from_str(line.trim_end()).expect("delivered line is an NCP envelope")
}

fn dispatch_to_tui(tui: &mut TuiEngine, envelope: &JsonValue) {
    if envelope.get("type").and_then(JsonValue::as_str) != Some("event") {
        return;
    }
    let body: JsonMap<String, JsonValue> =
        envelope["body"].as_object().expect("event body").clone();
    let source = envelope["from"].as_str().unwrap_or("engine");
    tui.dispatch_envelope_from(&body, source)
        .expect("dispatch delivered event into TUI engine");
}

#[tokio::test]
async fn cooperative_resume_rebuilds_tui_across_multiple_replay_chunks() {
    let root = repo_root();
    let data_dir = TempDir::new().expect("engine data tempdir");
    let tui_data_dir = TempDir::new().expect("TUI data tempdir");
    write_session_fixture(data_dir.path());

    let shared = Arc::new(Mutex::new(BrokerShared::new()));
    let bus = Arc::new(EventBus::new());
    let plugins = Arc::new(Mutex::new(PluginRegistry::new()));
    let ops: Arc<dyn EngineOps> = Arc::new(BrokerOps::new(Arc::clone(&shared)));
    let mut host = LuaHost::new(
        bus,
        plugins,
        ops,
        DataDir::new(data_dir.path().to_path_buf()),
    )
    .expect("create real engine LuaHost");
    host.exec_str("cooperative-resume-init.lua", &engine_init_source(&root))
        .expect("load engine Lua composition");
    host.cache_dispatch().expect("cache NCP dispatch hooks");

    let mut broker = Broker::new(shared, host);
    let (mut writer, mut reader, transport) = transport_pair();
    broker.attach_transport(
        transport,
        PluginName::new("nefor-tui").expect("valid plugin name"),
    );
    let shutdown = broker.shutdown_handle();
    let broker_task = tokio::spawn(broker.run());

    let mut tui = TuiEngine::new(80, 24).expect("create real TUI engine");
    tui.load_scenario(&tui_source(&root, tui_data_dir.path()))
        .expect("load starter/chat/init.lua");
    let _ = tui.render_if_dirty().expect("render initial frame");

    send_line(
        &mut writer,
        json!({
            "type": "system",
            "body": {
                "kind": "ready",
                "protocol_version": "0.1",
                "plugin_version": "integration-test"
            }
        }),
    )
    .await;
    let ready_ok = read_line(&mut reader).await;
    assert_eq!(ready_ok["body"]["kind"], "ready_ok");

    send_line(
        &mut writer,
        json!({
            "type": "event",
            "body": { "kind": "sessions.resume_request", "session_id": SESSION_ID }
        }),
    )
    .await;

    let expected_messages: Vec<String> = (0..REPLAY_MESSAGES)
        .map(|index| format!("replayed message {index}"))
        .collect();
    let mut lifecycle = Vec::new();
    let mut replayed_messages = Vec::new();
    let mut projected_messages_completed = 0usize;
    let mut progress_values = Vec::new();
    let mut replay_frame_sizes = Vec::new();
    let mut current_replay_frame = None;
    let mut resume_done_replayed = None;
    let mut rendered_partial_progress = false;
    loop {
        let envelope = read_line(&mut reader).await;
        let kind = envelope["body"]["kind"]
            .as_str()
            .expect("delivered event kind")
            .to_owned();
        match kind.as_str() {
            "sessions.replay.start" => {
                assert!(
                    current_replay_frame.replace(0).is_none(),
                    "replay frames must not overlap"
                );
            }
            "conversation.fact.recorded" => {
                *current_replay_frame
                    .as_mut()
                    .expect("replayed facts must be framed") += 1;
            }
            "conversation.projection.delta"
                if envelope["body"]["change"]["kind"] == "content_chunk_appended" =>
            {
                if let Some(text) = envelope["body"]["change"]["chunk"]["data"].as_str() {
                    replayed_messages.push(text.to_owned());
                }
            }
            "conversation.projection.delta"
                if envelope["body"]["change"]["kind"] == "message_completed"
                    && envelope["body"]["change"]["message"]["role"] == "user" =>
            {
                projected_messages_completed += 1;
            }
            "sessions.replay.end" => replay_frame_sizes.push(
                current_replay_frame
                    .take()
                    .expect("replay.end must close an active frame"),
            ),
            "sessions.replay.progress" => {
                progress_values.push((
                    envelope["body"]["replayed"]
                        .as_u64()
                        .expect("numeric replayed progress"),
                    envelope["body"]["total"]
                        .as_u64()
                        .expect("numeric replay total"),
                ));
            }
            "sessions.resume_done" => {
                resume_done_replayed = Some(
                    envelope["body"]["replayed"]
                        .as_u64()
                        .expect("numeric resume_done replayed count"),
                );
            }
            _ => {}
        }
        if kind.starts_with("sessions.") {
            lifecycle.push(kind.clone());
        }
        dispatch_to_tui(&mut tui, &envelope);

        if kind == "sessions.replay.progress" && !rendered_partial_progress {
            let &(replayed, total) = progress_values.last().expect("recorded progress");
            assert!(replayed > 0 && replayed < total, "progress must be partial");
            assert!(
                replayed_messages.len() < REPLAY_MESSAGES,
                "partial render must precede the complete replay projection"
            );
            assert_eq!(
                replayed_messages,
                expected_messages[..replayed_messages.len()],
                "messages visible at partial progress must be the exact fixture prefix"
            );
            let rendered = tui.render_if_dirty().expect("render partial replay frame");
            assert!(
                rendered.is_some(),
                "partial numeric progress must cause an actual render"
            );
            let frame = tui.snapshot();
            let progress = format!(
                "bytes {} / {}",
                format_replay_bytes(replayed),
                format_replay_bytes(total)
            );
            assert!(
                frame.contains(&progress),
                "partial frame must show byte progress: {frame:?}"
            );
            assert!(
                frame.contains("loading session") && frame.contains("rebuilding session"),
                "partial frame must remain visibly unavailable: {frame:?}"
            );
            for message in &replayed_messages {
                assert!(
                    !frame.contains(message),
                    "partial progress frame must conceal replayed fixture history {message:?}: {frame:?}"
                );
            }
            assert!(
                !lifecycle
                    .iter()
                    .any(|event| event == "sessions.resume_done"),
                "partial frame must precede resume_done"
            );
            rendered_partial_progress = true;
        }

        if resume_done_replayed.is_some()
            && replayed_messages.len() == REPLAY_MESSAGES
            && projected_messages_completed == REPLAY_MESSAGES
        {
            break;
        }
    }

    assert!(
        rendered_partial_progress,
        "fixture must cross a cooperative replay boundary"
    );
    assert_eq!(
        replayed_messages, expected_messages,
        "all fixture messages must replay exactly in order"
    );
    assert_eq!(replay_frame_sizes, vec![64, 64, 2]);
    assert!(
        current_replay_frame.is_none(),
        "final replay frame must close"
    );
    assert_eq!(resume_done_replayed, Some(REPLAY_ENTRIES as u64));
    assert!(
        !progress_values.is_empty(),
        "fixture must emit byte progress"
    );
    assert!(
        progress_values.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "partial byte progress must be strictly monotonic: {progress_values:?}"
    );
    assert!(
        progress_values
            .iter()
            .all(|(replayed, total)| *replayed > 0 && *replayed < *total),
        "reported progress must remain partial until resume_done: {progress_values:?}"
    );
    assert!(
        progress_values
            .windows(2)
            .all(|pair| pair[0].1 == pair[1].1),
        "progress total must stay stable: {progress_values:?}"
    );
    let session_end = lifecycle
        .iter()
        .position(|kind| kind == "sessions.session_end")
        .expect("session_end lifecycle event");
    let start = lifecycle
        .iter()
        .enumerate()
        .skip(session_end + 1)
        .find_map(|(index, kind)| (kind == "sessions.session_start").then_some(index))
        .expect("resumed session_start lifecycle event");
    let loading = lifecycle
        .iter()
        .position(|kind| kind == "sessions.resume_loading")
        .expect("resume_loading lifecycle event");
    let first_replay = lifecycle
        .iter()
        .position(|kind| kind == "sessions.replay.start")
        .expect("first replay start");
    let done = lifecycle
        .iter()
        .position(|kind| kind == "sessions.resume_done")
        .expect("resume_done lifecycle event");
    assert!(
        session_end < start && start < loading && loading < first_replay && first_replay < done
    );
    let last_replay_end = lifecycle
        .iter()
        .rposition(|kind| kind == "sessions.replay.end")
        .expect("final replay end");
    assert!(
        last_replay_end < done,
        "replay.end must precede resume_done"
    );
    assert_eq!(
        lifecycle
            .iter()
            .filter(|kind| kind.as_str() == "sessions.replay.start")
            .count(),
        3,
        "130 entries must be split into exact 64/64/2 replay frames"
    );
    assert_eq!(
        lifecycle.last().map(String::as_str),
        Some("sessions.resume_done")
    );

    let _ = tui.render_if_dirty().expect("render completed resume");
    let completed = tui.snapshot();
    assert!(
        !completed.contains("loading session"),
        "loading state must clear"
    );
    assert!(
        !completed.contains("rebuilding session"),
        "input must be restored after resume_done"
    );
    assert!(
        completed.contains(expected_messages.last().expect("final fixture message")),
        "completed frame must reveal the resumed transcript tail: {completed:?}"
    );

    shutdown.shutdown(0).await;
    tokio::time::timeout(Duration::from_secs(5), broker_task)
        .await
        .expect("broker cleanup timed out")
        .expect("broker task panicked");
}

#[test]
fn switching_sessions_between_chunks_cancels_the_stale_replay() {
    let root = repo_root();
    let data_dir = TempDir::new().expect("engine data tempdir");
    write_session_fixture(data_dir.path());

    let shared = Arc::new(Mutex::new(BrokerShared::new()));
    let bus = Arc::new(EventBus::new());
    let plugins = Arc::new(Mutex::new(PluginRegistry::new()));
    let ops: Arc<dyn EngineOps> = Arc::new(BrokerOps::new(shared));
    let host = LuaHost::new(
        bus,
        plugins,
        ops,
        DataDir::new(data_dir.path().to_path_buf()),
    )
    .expect("create real engine LuaHost");
    host.exec_str(
        "cooperative-cancellation-init.lua",
        &engine_init_source(&root),
    )
    .expect("load engine Lua composition");
    host.exec_str(
        "cooperative-cancellation-observer.lua",
        r#"
        _cancel_trace = {}
        local real_send = nefor.engine.send
        nefor.engine.send = function(payload, target)
          local decoded = nefor.json.decode(payload)
          local body = decoded.body or {}
          if body.kind == "conversation.fact.recorded"
              or body.kind == "sessions.replay.start"
              or body.kind == "sessions.replay.end"
              or body.kind == "sessions.resume_done" then
            _cancel_trace[#_cancel_trace + 1] = {
              kind = body.kind,
              session_id = body.session_id,
              text = body.text,
            }
          end
          real_send(payload, target)
        end
        require("libs.sessions").resume("cooperative-resume")
        "#,
    )
    .expect("start first cooperative replay");

    assert!(
        host.run_one_cooperative_task()
            .expect("run first replay chunk"),
        "large fixture must schedule a first chunk"
    );
    host.exec_str(
        "cooperative-cancellation-switch.lua",
        r#"require("libs.sessions").resume("replacement-session")"#,
    )
    .expect("switch sessions between replay chunks");
    while host
        .run_one_cooperative_task()
        .expect("drain stale cooperative callback")
    {}

    host.exec_str(
        "cooperative-cancellation-assert.lua",
        r#"
        local first_messages = 0
        local first_starts = 0
        local first_ends = 0
        local replacement_done = false
        for _, event in ipairs(_cancel_trace) do
          if event.kind == "conversation.fact.recorded" then
            first_messages = first_messages + 1
          elseif event.session_id == "cooperative-resume" and event.kind == "sessions.replay.start" then
            first_starts = first_starts + 1
          elseif event.session_id == "cooperative-resume" and event.kind == "sessions.replay.end" then
            first_ends = first_ends + 1
          elseif event.session_id == "replacement-session" and event.kind == "sessions.resume_done" then
            replacement_done = true
          end
        end
        assert(first_messages == 64, "stale replay must stop after its completed first chunk")
        assert(first_starts == 1 and first_ends == 1, "stale replay must leave one balanced frame")
        assert(replacement_done, "replacement resume must complete")
        "#,
    )
    .expect("assert cancellation trace");
}
