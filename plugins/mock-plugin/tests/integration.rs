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

use nefor_protocol::{
    Envelope, ParseError, PluginName, PluginOutgoing, SystemBody, Timestamp,
};
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
    assert_eq!(
        body.get("greeting").and_then(|v| v.as_str()),
        Some("hi")
    );

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
    assert_eq!(
        b.get("echoed_from").and_then(|v| v.as_str()),
        Some("peer")
    );

    send_shutdown(&mut stdin).await;
    drop(stdin);
    let _ = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("exit in time")
        .expect("wait");

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
