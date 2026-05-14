//! Integration tests — spawn the mock-plugin binary, drive it over
//! stdio, and assert its wire output.
//!
//! Each test writes a small Lua scenario to a temp file, launches the
//! binary with `--script <file>`, walks the handshake, streams a few
//! engine-authored lines, and reads back whatever the plugin emits.

use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nefor_protocol::{Envelope, ParseError, PluginName, PluginOutgoing, SystemBody, Timestamp};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

fn binary_path() -> PathBuf {
    // CARGO_BIN_EXE_<name> points at the built binary; cargo test sets
    // this for integration tests that reference the crate's binary.
    PathBuf::from(env!("CARGO_BIN_EXE_mock-plugin"))
}

fn temp_script(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "mock-plugin-test-{}-{}-{}.lua",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    let mut f = std::fs::File::create(&path).expect("temp file");
    f.write_all(source.as_bytes()).expect("write");
    path
}

async fn spawn_mock(script: &PathBuf) -> Child {
    Command::new(binary_path())
        .arg("--script")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mock-plugin")
}

async fn read_line<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Option<String> {
    let mut s = String::new();
    match timeout(Duration::from_secs(5), r.read_line(&mut s)).await {
        Ok(Ok(0)) => None,
        Ok(Ok(_)) => Some(s.trim_end_matches('\n').to_string()),
        Ok(Err(e)) => panic!("read line: {e}"),
        Err(_) => panic!("timed out waiting for plugin output"),
    }
}

async fn parse_outgoing(line: &str) -> Result<PluginOutgoing, ParseError> {
    PluginOutgoing::parse_line(line)
}

async fn send_ready_ok(stdin: &mut tokio::process::ChildStdin) {
    let env = Envelope::system(
        PluginName::engine(),
        Timestamp::now(),
        SystemBody::ReadyOk {
            engine_version: "fake-0.1.0".into(),
        },
    );
    stdin.write_all(env.to_line().as_bytes()).await.expect("w");
    stdin.write_all(b"\n").await.expect("nl");
    stdin.flush().await.expect("flush");
}

async fn send_shutdown(stdin: &mut tokio::process::ChildStdin) {
    let env = Envelope::system(
        PluginName::engine(),
        Timestamp::now(),
        SystemBody::Shutdown {
            reason: Some("test done".into()),
            grace_ms: Some(500),
        },
    );
    stdin.write_all(env.to_line().as_bytes()).await.expect("w");
    stdin.write_all(b"\n").await.expect("nl");
    stdin.flush().await.expect("flush");
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn minimal_script_sends_hello_and_exits_on_shutdown() {
    let script = temp_script(
        "minimal",
        r#"
        nefor.on_ready_ok(function()
            nefor.emit("hello", { greeting = "hi" })
        end)
        "#,
    );
    let mut child = spawn_mock(&script).await;
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    // 1) Plugin sends ready.
    let ready_line = read_line(&mut reader).await.expect("ready line");
    let ready = parse_outgoing(&ready_line).await.expect("parse ready");
    assert!(matches!(
        ready.body,
        nefor_protocol::Body::System(SystemBody::Ready { .. })
    ));

    // 2) We reply with ready_ok.
    send_ready_ok(&mut stdin).await;

    // 3) Plugin should emit our hello event.
    let hello_line = read_line(&mut reader).await.expect("hello line");
    let hello = parse_outgoing(&hello_line).await.expect("parse hello");
    let body = match hello.body {
        nefor_protocol::Body::Event(m) => m,
        _ => panic!("expected event"),
    };
    assert_eq!(
        body.get("kind").and_then(|v| v.as_str()),
        Some("mock-plugin.hello")
    );
    assert_eq!(body.get("greeting").and_then(|v| v.as_str()), Some("hi"));

    // 4) Tell it to shut down and wait for exit.
    send_shutdown(&mut stdin).await;
    drop(stdin);
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("exit in time")
        .expect("wait");
    assert!(status.success(), "plugin did not exit cleanly: {status:?}");

    cleanup(&script);
}

#[tokio::test]
async fn echo_script_mirrors_events_back() {
    let script = temp_script(
        "echo",
        r#"
        nefor.on_any(function(body, env)
            if body.kind == "mock-plugin.echo" then return end
            nefor.emit("echo", {
                echoed_kind = body.kind,
                echoed_from = env.from,
            })
        end)
        "#,
    );
    let mut child = spawn_mock(&script).await;
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    let _ready = read_line(&mut reader).await.expect("ready");
    send_ready_ok(&mut stdin).await;

    // Send an event that should be echoed.
    let mut body = serde_json::Map::new();
    body.insert("kind".into(), serde_json::Value::String("peer.ping".into()));
    let env = Envelope::event(
        PluginName::new("peer").expect("valid"),
        Timestamp::now(),
        body,
    );
    stdin.write_all(env.to_line().as_bytes()).await.expect("w");
    stdin.write_all(b"\n").await.expect("nl");
    stdin.flush().await.expect("flush");

    // Plugin emits its echo.
    let echo_line = read_line(&mut reader).await.expect("echo line");
    let echo = parse_outgoing(&echo_line).await.expect("parse echo");
    let b = match echo.body {
        nefor_protocol::Body::Event(m) => m,
        _ => panic!("expected event"),
    };
    assert_eq!(
        b.get("kind").and_then(|v| v.as_str()),
        Some("mock-plugin.echo")
    );
    assert_eq!(
        b.get("echoed_kind").and_then(|v| v.as_str()),
        Some("peer.ping")
    );
    assert_eq!(b.get("echoed_from").and_then(|v| v.as_str()), Some("peer"));

    send_shutdown(&mut stdin).await;
    drop(stdin);
    let _ = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("exit in time")
        .expect("wait");

    cleanup(&script);
}

/// Pinned regression for the cancel-mid-stream bug: while a Lua handler
/// streams chunks via `nefor.sleep`-paced loops, an inbound `interrupt`
/// envelope must land at the next sleep yield rather than waiting for
/// the full stream to drain. The mock plugin's `chat.complete` handler
/// can run for a long time (paced canned text); the wrapper translates
/// `chat.interrupt` to `<NAME>.interrupt`, and the handler is expected
/// to flip a per-chat flag the streaming loop checks. The fix has two
/// halves: (a) `main::run_dispatch_loop` spawns each dispatch as its own
/// tokio task so the loop itself never blocks on an in-flight handler;
/// (b) the streaming script uses `nefor.sleep` (yields the runtime) and
/// checks the flag between chunks. This test exercises both.
#[tokio::test]
async fn interrupt_envelope_breaks_streaming_loop_at_next_sleep_yield() {
    let script = temp_script(
        "interrupt-mid-stream",
        r#"
        local interrupted = false
        nefor.on("peer.start", function()
            for i = 1, 50 do
                if interrupted then
                    nefor.emit("stopped", { at = i })
                    return
                end
                nefor.emit("tick", { i = i })
                nefor.sleep(20)
            end
            nefor.emit("done", {})
        end)
        nefor.on("peer.stop", function()
            interrupted = true
        end)
        "#,
    );
    let mut child = spawn_mock(&script).await;
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    let _ready = read_line(&mut reader).await.expect("ready");
    send_ready_ok(&mut stdin).await;

    // Kick off the slow loop.
    let mut start_body = serde_json::Map::new();
    start_body.insert("kind".into(), serde_json::Value::String("peer.start".into()));
    let start = Envelope::event(
        PluginName::new("peer").expect("valid"),
        Timestamp::now(),
        start_body,
    );
    stdin.write_all(start.to_line().as_bytes()).await.expect("w");
    stdin.write_all(b"\n").await.expect("nl");
    stdin.flush().await.expect("flush");

    // Read a few ticks so we know we're mid-stream — confirms the
    // handler is yielding to the runtime instead of blocking.
    let tick1 = read_line(&mut reader).await.expect("tick1");
    let tick2 = read_line(&mut reader).await.expect("tick2");
    assert!(tick1.contains("\"mock-plugin.tick\""), "first line should be a tick: {tick1}");
    assert!(tick2.contains("\"mock-plugin.tick\""), "second line should be a tick: {tick2}");

    // Send the interrupt while the loop is paused at `nefor.sleep`.
    let mut stop_body = serde_json::Map::new();
    stop_body.insert("kind".into(), serde_json::Value::String("peer.stop".into()));
    let stop = Envelope::event(
        PluginName::new("peer").expect("valid"),
        Timestamp::now(),
        stop_body,
    );
    stdin.write_all(stop.to_line().as_bytes()).await.expect("w");
    stdin.write_all(b"\n").await.expect("nl");
    stdin.flush().await.expect("flush");

    // Drain remaining lines; expect a `stopped` envelope BEFORE we'd see
    // 50 ticks. With the buggy shape (inline await, no sleep yield) the
    // interrupt sits in the input queue and `stopped` never arrives —
    // we'd see all 50 ticks then `done`.
    let mut saw_stopped = false;
    let mut saw_done = false;
    let mut tick_count = 2; // already counted the first two
    for _ in 0..60 {
        let line = match timeout(Duration::from_secs(2), read_line(&mut reader)).await {
            Ok(Some(l)) => l,
            _ => break,
        };
        if line.contains("\"mock-plugin.tick\"") {
            tick_count += 1;
        } else if line.contains("\"mock-plugin.stopped\"") {
            saw_stopped = true;
            break;
        } else if line.contains("\"mock-plugin.done\"") {
            saw_done = true;
            break;
        }
    }

    send_shutdown(&mut stdin).await;
    drop(stdin);
    let _ = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("exit in time")
        .expect("wait");

    assert!(saw_stopped, "stopped envelope must arrive after interrupt; tick_count={tick_count} done={saw_done}");
    assert!(
        tick_count < 50,
        "interrupt should break the stream early; got {tick_count} ticks (full stream is 50)"
    );

    cleanup(&script);
}

#[tokio::test]
async fn emit_before_ready_errors_in_script_load() {
    // Calling nefor.emit at top level runs before the handshake, so the
    // script exec fails immediately and the binary exits non-zero
    // without sending a ready (it errors out before the handshake).
    let script = temp_script(
        "early-emit",
        r#"
        nefor.emit("too-early")
        "#,
    );
    let mut child = spawn_mock(&script).await;
    // We don't send ready_ok — the child should exit before asking.
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("exit in time")
        .expect("wait");
    assert!(
        !status.success(),
        "plugin should exit non-zero on early emit; got {status:?}"
    );
    cleanup(&script);
}

/// Regression: when /cancel fires mid-stream, the partial assistant
/// text the model has emitted so far MUST be persisted into the chat's
/// history table on the provider binary, so the next turn's request
/// includes "what the model was saying before being cut off". The
/// user-facing motivation is the "you started thinking wrongly,
/// reconsider" follow-up: without the partial in context the model
/// has nothing to reconsider against.
///
/// Mirrors the openai-provider's existing `push_assistant` on
/// `outcome.interrupted` (plugins/openai-provider/src/main.rs around
/// line 751) — both providers share the same wrapper, so the user-
/// visible chat-side `[interrupted]` system message is unchanged
/// here; the fix is purely on the provider binary's per-chat history
/// table.
///
/// Probe: production `mock_provider.lua` exposes a debug-only
/// `<NAME>.debug.history.dump` handler that emits
/// `<NAME>.debug.history.result { messages }`. Production code paths
/// don't subscribe to it; tests use it to peek at the in-process chats
/// table without re-driving a full chat.complete cycle.
///
/// Drive: spawn the production lua → chat.create → chat.append (user)
/// → chat.complete (long help-fallback canned text streams paced at
/// 20ms/chunk) → wait until enough deltas land that we know we're
/// mid-stream → interrupt → wait for chat.error("interrupted") →
/// debug.history.dump → assert the dump contains an assistant message
/// whose content is a non-empty prefix of the canned text.
#[tokio::test]
async fn interrupt_mid_stream_persists_partial_assistant_text_to_history() {
    // Production lua, not a temp script — we want this test to fail if
    // anyone reverts the partial-persistence in the real provider.
    let script_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("plugins/")
        .parent()
        .expect("repo root")
        .join("starter/mock_provider.lua");
    assert!(
        script_path.exists(),
        "production mock_provider.lua not found at {script_path:?}",
    );

    let mut child = Command::new(binary_path())
        .arg("--script")
        .arg(&script_path)
        // NEFOR_TEST_FAST_MOCK is the opt-in for instant streaming used
        // by agentic_cli_mock_e2e — leave it UNSET here so pacing is
        // active and the interrupt actually catches mid-stream.
        .env_remove("NEFOR_TEST_FAST_MOCK")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mock-plugin");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    // Handshake.
    let _ready = read_line(&mut reader).await.expect("ready");
    send_ready_ok(&mut stdin).await;

    // The production lua emits a few setup envelopes on ready_ok
    // (`hello`, `auth.status`). Drain them so subsequent reads are
    // deterministic against our test envelopes.
    let mut setup_drained = 0;
    while setup_drained < 4 {
        match timeout(Duration::from_millis(500), read_line(&mut reader)).await {
            Ok(Some(_)) => setup_drained += 1,
            _ => break,
        }
    }

    let chat_id = "regress-interrupt-c1";
    let plugin = PluginName::new("mock-plugin").expect("valid");

    // 1. chat.create
    let mut create_body = serde_json::Map::new();
    create_body.insert(
        "kind".into(),
        serde_json::Value::String("mock-plugin.chat.create".into()),
    );
    create_body.insert("chat_id".into(), serde_json::Value::String(chat_id.into()));
    let create = Envelope::event(plugin.clone(), Timestamp::now(), create_body);
    stdin.write_all(create.to_line().as_bytes()).await.expect("w");
    stdin.write_all(b"\n").await.expect("nl");

    // 2. chat.append { role=user, content=<gibberish, hits help fallback> }.
    //    The help fallback streams the long HELP_TEXT (~3KB), paced at
    //    20ms per chunk → plenty of room for the interrupt to land
    //    mid-stream.
    let mut append_body = serde_json::Map::new();
    append_body.insert(
        "kind".into(),
        serde_json::Value::String("mock-plugin.chat.append".into()),
    );
    append_body.insert("chat_id".into(), serde_json::Value::String(chat_id.into()));
    let mut msg = serde_json::Map::new();
    msg.insert("role".into(), serde_json::Value::String("user".into()));
    msg.insert(
        "content".into(),
        serde_json::Value::String("zxqv-no-trigger-route-to-help".into()),
    );
    append_body.insert("message".into(), serde_json::Value::Object(msg));
    let append = Envelope::event(plugin.clone(), Timestamp::now(), append_body);
    stdin.write_all(append.to_line().as_bytes()).await.expect("w");
    stdin.write_all(b"\n").await.expect("nl");
    stdin.flush().await.expect("flush");

    // 3. chat.complete — kicks off streaming.
    let mut complete_body = serde_json::Map::new();
    complete_body.insert(
        "kind".into(),
        serde_json::Value::String("mock-plugin.chat.complete".into()),
    );
    complete_body.insert("chat_id".into(), serde_json::Value::String(chat_id.into()));
    let complete = Envelope::event(plugin.clone(), Timestamp::now(), complete_body);
    stdin.write_all(complete.to_line().as_bytes()).await.expect("w");
    stdin.write_all(b"\n").await.expect("nl");
    stdin.flush().await.expect("flush");

    // 4. Wait until enough stream.delta envelopes have landed that we
    //    know we're mid-stream (and the partial buffer holds something).
    //    Three deltas is plenty — the help text is ~3KB / 16-char
    //    chunks ≈ 200 deltas, so we're far from the end at three.
    let mut delta_count = 0;
    let mut accumulated_partial = String::new();
    while delta_count < 3 {
        let line = match timeout(Duration::from_secs(3), read_line(&mut reader)).await {
            Ok(Some(l)) => l,
            _ => panic!(
                "timed out waiting for stream.delta #{}; saw {delta_count} so far",
                delta_count + 1,
            ),
        };
        if line.contains("\"mock-plugin.stream.delta\"") {
            delta_count += 1;
            // Extract the text field — the partial we expect to see
            // mirrored in history.
            if let Ok(env) = parse_outgoing(&line).await {
                if let nefor_protocol::Body::Event(map) = env.body {
                    if let Some(t) = map.get("text").and_then(|v| v.as_str()) {
                        accumulated_partial.push_str(t);
                    }
                }
            }
        }
    }
    assert!(
        !accumulated_partial.is_empty(),
        "partial accumulator should be non-empty after 3 deltas",
    );

    // 5. interrupt — flips the per-chat flag the streaming loop checks
    //    on each chunk boundary.
    let mut interrupt_body = serde_json::Map::new();
    interrupt_body.insert(
        "kind".into(),
        serde_json::Value::String("mock-plugin.interrupt".into()),
    );
    interrupt_body.insert("chat_id".into(), serde_json::Value::String(chat_id.into()));
    let interrupt = Envelope::event(plugin.clone(), Timestamp::now(), interrupt_body);
    stdin.write_all(interrupt.to_line().as_bytes()).await.expect("w");
    stdin.write_all(b"\n").await.expect("nl");
    stdin.flush().await.expect("flush");

    // 6. Drain remaining envelopes until chat.error lands. Anything else
    //    in between is a leftover delta or stream.end — just skip.
    let mut saw_chat_error = false;
    for _ in 0..400 {
        let line = match timeout(Duration::from_secs(3), read_line(&mut reader)).await {
            Ok(Some(l)) => l,
            _ => break,
        };
        if line.contains("\"mock-plugin.chat.error\"") {
            saw_chat_error = true;
            break;
        }
    }
    assert!(
        saw_chat_error,
        "expected mock-plugin.chat.error after interrupt"
    );

    // 7. debug.history.dump — peek at the in-process chats table.
    let mut dump_body = serde_json::Map::new();
    dump_body.insert(
        "kind".into(),
        serde_json::Value::String("mock-plugin.debug.history.dump".into()),
    );
    dump_body.insert("chat_id".into(), serde_json::Value::String(chat_id.into()));
    let dump = Envelope::event(plugin, Timestamp::now(), dump_body);
    stdin.write_all(dump.to_line().as_bytes()).await.expect("w");
    stdin.write_all(b"\n").await.expect("nl");
    stdin.flush().await.expect("flush");

    // 8. Read the result.
    let mut history_messages: Option<Vec<serde_json::Value>> = None;
    for _ in 0..20 {
        let line = match timeout(Duration::from_secs(3), read_line(&mut reader)).await {
            Ok(Some(l)) => l,
            _ => break,
        };
        if line.contains("\"mock-plugin.debug.history.result\"") {
            let env = parse_outgoing(&line).await.expect("parse history result");
            if let nefor_protocol::Body::Event(map) = env.body {
                if let Some(serde_json::Value::Array(msgs)) = map.get("messages").cloned() {
                    history_messages = Some(msgs);
                    break;
                }
            }
        }
    }
    let messages = history_messages.expect("debug.history.result with messages array");

    send_shutdown(&mut stdin).await;
    drop(stdin);
    let _ = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("exit in time")
        .expect("wait");

    // Expected layout: [user, assistant<partial>]. The partial assistant
    // message MUST exist with non-empty content matching the deltas
    // we observed on the wire.
    assert_eq!(
        messages.len(),
        2,
        "expected [user, assistant] in history; got {} messages: {:?}",
        messages.len(),
        messages,
    );
    let user = &messages[0];
    assert_eq!(
        user.get("role").and_then(|v| v.as_str()),
        Some("user"),
        "first history entry should be the user message; got: {user:?}",
    );
    let assistant = &messages[1];
    assert_eq!(
        assistant.get("role").and_then(|v| v.as_str()),
        Some("assistant"),
        "second history entry should be the assistant message; got: {assistant:?}",
    );
    let content = assistant
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        !content.is_empty(),
        "assistant content must be non-empty (the partial streamed text); \
         this is the regression — pre-fix the chat.complete handler stored \
         an empty string. accumulated_partial on wire was {} chars.",
        accumulated_partial.len(),
    );
    // The persisted partial must be a prefix of (or equal to) the wire
    // partial — `emit_stream` accumulates the same chunks it emits.
    // Streaming may have advanced one or two chunks past our last read
    // before the interrupt landed, so we accept "wire partial is a
    // prefix of stored partial OR stored partial is a prefix of wire
    // partial": both indicate the same underlying buffer.
    assert!(
        content.starts_with(&accumulated_partial)
            || accumulated_partial.starts_with(content),
        "stored partial and wire partial must share a prefix; \
         stored={:?} wire={:?}",
        truncate_str(content, 80),
        truncate_str(&accumulated_partial, 80),
    );
}

fn truncate_str(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_owned()
    } else {
        // Snap to a char boundary so multibyte sequences aren't sliced.
        let mut idx = n;
        while idx > 0 && !s.is_char_boundary(idx) {
            idx -= 1;
        }
        format!("{}…", &s[..idx])
    }
}
