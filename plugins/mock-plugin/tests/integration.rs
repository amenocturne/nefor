//! Integration tests — spawn the mock-plugin binary, drive it over
//! stdio, and assert its wire output.
//!
//! Each test writes a small Lua scenario to a temp file, launches the
//! binary with `--script <file>`, walks the handshake, streams a few
//! engine-authored lines, and reads back whatever the plugin emits.
//!
//! The target exercises only deterministic local process and protocol
//! boundaries, so every test runs in an ordinary Cargo invocation.

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
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("exit in time")
        .expect("wait");
    assert!(status.success(), "plugin did not exit cleanly: {status:?}");

    cleanup(&script);
}

/// Pinned regression for the cancel-mid-stream bug: while a Lua handler
/// streams chunks via `nefor.sleep`-paced loops, an inbound
/// `completion.cancel` envelope must land at the next sleep yield rather
/// than waiting for the full stream to drain. The handler flips a flag the
/// streaming loop checks at every chunk boundary. The fix has two
/// halves: (a) `main::run_dispatch_loop` spawns each `completion.request`
/// dispatch as its own tokio task so the loop itself never blocks on
/// an in-flight stream; (b) the streaming script uses `nefor.sleep`
/// (yields the runtime) and checks the flag between chunks. This test
/// exercises both.
///
/// The script uses a `*.completion.request`-shaped kind because that's the
/// kind the dispatch loop spawns for. Non-streaming kinds dispatch
/// direct provider request kind the dispatch loop spawns for. Other kinds
/// dispatch inline and retain input ordering.
#[tokio::test]
async fn completion_cancel_breaks_streaming_loop_at_next_sleep_yield() {
    let script = temp_script(
        "interrupt-mid-stream",
        r#"
        local interrupted = false
        nefor.on("peer.completion.request", function()
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
        nefor.on("peer.completion.cancel", function()
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
    start_body.insert(
        "kind".into(),
        serde_json::Value::String("peer.completion.request".into()),
    );
    let start = Envelope::event(
        PluginName::new("peer").expect("valid"),
        Timestamp::now(),
        start_body,
    );
    stdin
        .write_all(start.to_line().as_bytes())
        .await
        .expect("w");
    stdin.write_all(b"\n").await.expect("nl");
    stdin.flush().await.expect("flush");

    // Read a few ticks so we know we're mid-stream — confirms the
    // handler is yielding to the runtime instead of blocking.
    let tick1 = read_line(&mut reader).await.expect("tick1");
    let tick2 = read_line(&mut reader).await.expect("tick2");
    assert!(
        tick1.contains("\"mock-plugin.tick\""),
        "first line should be a tick: {tick1}"
    );
    assert!(
        tick2.contains("\"mock-plugin.tick\""),
        "second line should be a tick: {tick2}"
    );

    // Send the cancellation while the loop is paused at `nefor.sleep`.
    let mut stop_body = serde_json::Map::new();
    stop_body.insert(
        "kind".into(),
        serde_json::Value::String("peer.completion.cancel".into()),
    );
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
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("exit in time")
        .expect("wait");
    assert!(status.success(), "plugin did not exit cleanly: {status:?}");

    assert!(
        saw_stopped,
        "stopped envelope must arrive after interrupt; tick_count={tick_count} done={saw_done}"
    );
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

fn repo_lua_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../lua")
        .canonicalize()
        .expect("repository lua/ tree")
}

/// Spawn mock-plugin with a `libs.mock-provider` behavior script, complete the
/// handshake, and consume the two ready announcements.
async fn spawn_mock_provider(
    name: &str,
    behavior: &str,
) -> (
    Child,
    tokio::process::ChildStdin,
    BufReader<tokio::process::ChildStdout>,
    PathBuf,
) {
    let script = temp_script(name, behavior);
    let mut child = Command::new(binary_path())
        .arg("--script")
        .arg(&script)
        .arg("--lua-root")
        .arg(repo_lua_root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mock-plugin");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));

    let ready_line = read_line(&mut reader).await.expect("ready");
    let ready = parse_outgoing(&ready_line).await.expect("parse ready");
    assert!(matches!(
        ready.body,
        nefor_protocol::Body::System(SystemBody::Ready { .. })
    ));
    send_ready_ok(&mut stdin).await;
    for expected_kind in ["mock-plugin.hello", "mock-plugin.auth.status"] {
        let line = read_line(&mut reader).await.expect("ready announcement");
        let outgoing = parse_outgoing(&line).await.expect("parse announcement");
        let nefor_protocol::Body::Event(body) = outgoing.body else {
            panic!("expected ready announcement event: {line}");
        };
        assert_eq!(
            body.get("kind").and_then(serde_json::Value::as_str),
            Some(expected_kind),
        );
    }
    (child, stdin, reader, script)
}

async fn send_event(stdin: &mut tokio::process::ChildStdin, body: serde_json::Value) {
    let plugin = PluginName::new("mock-plugin").expect("valid");
    let body = body.as_object().expect("event object").clone();
    let envelope = Envelope::event(plugin, Timestamp::now(), body);
    stdin
        .write_all(envelope.to_line().as_bytes())
        .await
        .expect("w");
    stdin.write_all(b"\n").await.expect("nl");
    stdin.flush().await.expect("flush");
}

/// Next `completion.event` body for `request_id`, skipping everything else.
async fn next_completion_event<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    request_id: &str,
) -> serde_json::Map<String, serde_json::Value> {
    loop {
        let line = read_line(reader).await.expect("completion event");
        let outgoing = parse_outgoing(&line).await.expect("parse completion event");
        let nefor_protocol::Body::Event(body) = outgoing.body else {
            continue;
        };
        if body.get("kind").and_then(serde_json::Value::as_str)
            == Some("mock-plugin.completion.event")
            && body.get("request_id").and_then(serde_json::Value::as_str) == Some(request_id)
        {
            return body;
        }
    }
}

async fn finish(mut child: Child, mut stdin: tokio::process::ChildStdin, script: PathBuf) {
    send_shutdown(&mut stdin).await;
    drop(stdin);
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("exit in time")
        .expect("wait");
    assert!(status.success(), "plugin did not exit cleanly: {status:?}");
    cleanup(&script);
}

/// `libs.mock-provider` uses the same stateless direct-completion contract as
/// openai-provider: history arrives on each correlated request, and a hard
/// cancellation suppresses that request's terminal result. The cancellation
/// handler must nevertheless settle its registry entry immediately so the
/// correlation id can be reused without racing the detached stream task.
#[tokio::test]
async fn mock_provider_cancel_settles_request_without_terminal_result() {
    let (child, mut stdin, mut reader, script) = spawn_mock_provider(
        "provider-cancel",
        r#"
        local mock = require("libs.mock-provider")
        mock.serve {
          pace_ms = 20,
          respond = function(request)
            local prompt = request.messages[#request.messages].content
            if prompt == "fail" then return mock.error("scripted failure") end
            return mock.text(string.rep("streamed text ", 40))
          end,
        }
        "#,
    )
    .await;

    let request_id = "cancel-and-reuse-1";
    send_event(
        &mut stdin,
        serde_json::json!({
            "kind": "mock-plugin.completion.request",
            "request_id": request_id,
            "request": {"input": {"messages": [{"role": "user", "content": "stream"}]}}
        }),
    )
    .await;

    // Three correlated deltas establish that the request is actively
    // streaming through yield points, rather than merely queued.
    for _ in 0..3 {
        let body = next_completion_event(&mut reader, request_id).await;
        assert_eq!(
            body.get("event").and_then(serde_json::Value::as_str),
            Some("text_delta")
        );
    }

    send_event(
        &mut stdin,
        serde_json::json!({"kind": "mock-plugin.completion.cancel", "request_id": request_id}),
    )
    .await;

    // Reusing the id is the protocol-visible settlement probe. The new error
    // request must be accepted; "already in flight" would prove cancellation
    // left stale ownership behind. The cancelled request itself must not emit
    // usage or a successful terminal result.
    send_event(
        &mut stdin,
        serde_json::json!({
            "kind": "mock-plugin.completion.request",
            "request_id": request_id,
            "request": {"input": {"messages": [{"role": "user", "content": "fail"}]}}
        }),
    )
    .await;

    loop {
        let body = next_completion_event(&mut reader, request_id).await;
        match body.get("event").and_then(serde_json::Value::as_str) {
            Some("text_delta") => {}
            Some("error") => {
                assert_eq!(
                    body.get("message").and_then(serde_json::Value::as_str),
                    Some("scripted failure"),
                    "the reused request must be accepted after cancellation",
                );
                break;
            }
            other => panic!("cancelled request emitted unexpected terminal event {other:?}"),
        }
    }

    finish(child, stdin, script).await;
}

/// The behavior sees the request's system and messages unchanged, tool-call
/// responses complete without deltas, and a behavior fault becomes a
/// correlated completion error instead of a silent hang.
#[tokio::test]
async fn mock_provider_completes_tool_calls_and_reports_behavior_faults() {
    let (child, mut stdin, mut reader, script) = spawn_mock_provider(
        "provider-tools",
        r#"
        local mock = require("libs.mock-provider")
        mock.serve {
          respond = function(request)
            local last = request.messages[#request.messages]
            if last.content == "boom" then error("scenario has no answer") end
            return mock.tool_call("read_file", { path = request.system .. "/" .. last.content })
          end,
        }
        "#,
    )
    .await;

    send_event(
        &mut stdin,
        serde_json::json!({
            "kind": "mock-plugin.completion.request",
            "request_id": "tools-1",
            "request": {"system": "docs", "input": {"messages": [{"role": "user", "content": "README.md"}]}}
        }),
    )
    .await;
    let usage = next_completion_event(&mut reader, "tools-1").await;
    assert_eq!(usage["event"], "usage");
    let completed = next_completion_event(&mut reader, "tools-1").await;
    assert_eq!(completed["event"], "completed");
    let result = &completed["result"];
    assert_eq!(result["finish_reason"], "tool_calls");
    assert_eq!(result["tool_calls"][0]["name"], "read_file");
    assert_eq!(
        result["tool_calls"][0]["arguments"]["path"],
        "docs/README.md"
    );
    assert!(result["tool_calls"][0]["id"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));

    send_event(
        &mut stdin,
        serde_json::json!({
            "kind": "mock-plugin.completion.request",
            "request_id": "fault-1",
            "request": {"input": {"messages": [{"role": "user", "content": "boom"}]}}
        }),
    )
    .await;
    let fault = next_completion_event(&mut reader, "fault-1").await;
    assert_eq!(fault["event"], "error");
    assert!(fault["message"]
        .as_str()
        .is_some_and(|message| message.contains("scenario has no answer")));

    finish(child, stdin, script).await;
}
