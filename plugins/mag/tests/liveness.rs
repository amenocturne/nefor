//! Liveness integration test for the mag plugin.
//!
//! Spawns the compiled `mag-plugin` binary as an NCP peer over stdio,
//! completes the ready handshake, asserts it loaded the stub kernel (via
//! the `mag.hello` advertisement), and round-trips a `mag.ping`/`mag.pong`.
//! Mirrors the subprocess-driving pattern from
//! `plugins/basic-tools/tests/concurrency.rs`.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::time::timeout;

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mag-plugin"))
}

/// The in-repo stub kernel, resolved relative to this crate.
fn kernel_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel/init.lua")
}

async fn spawn_mag() -> Child {
    let mut cmd = tokio::process::Command::new(binary_path());
    cmd.arg("--kernel")
        .arg(kernel_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn().expect("spawn mag-plugin")
}

/// Generous read timeout: a passing run completes in well under a second,
/// but on a heavily loaded machine (parallel workspace builds) the child
/// spawn itself can stall for tens of seconds. The bound only affects how
/// fast a genuine failure is reported.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

async fn read_outgoing<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    expecting: &str,
) -> PluginOutgoing {
    let mut line = String::new();
    match timeout(READ_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(0)) => panic!("mag stdout closed while expecting {expecting}"),
        Ok(Ok(_)) => PluginOutgoing::parse_line(line.trim_end()).expect("parse outgoing"),
        Ok(Err(e)) => panic!("read mag stdout while expecting {expecting}: {e}"),
        Err(_) => panic!("timed out waiting for mag output while expecting {expecting}"),
    }
}

async fn write_env(stdin: &mut ChildStdin, env: Envelope) {
    stdin
        .write_all(env.to_line().as_bytes())
        .await
        .expect("write envelope");
    stdin.write_all(b"\n").await.expect("write newline");
    stdin.flush().await.expect("flush envelope");
}

async fn send_ready_ok(stdin: &mut ChildStdin) {
    write_env(
        stdin,
        Envelope::system(
            PluginName::engine(),
            Timestamp::now(),
            SystemBody::ReadyOk {
                engine_version: "test".into(),
            },
        ),
    )
    .await;
}

async fn send_event(stdin: &mut ChildStdin, body: Map<String, Value>) {
    write_env(
        stdin,
        Envelope::event(PluginName::engine(), Timestamp::now(), body),
    )
    .await;
}

fn event_kind(out: &PluginOutgoing) -> Option<&str> {
    match &out.body {
        Body::Event(map) => map.get("kind").and_then(Value::as_str),
        _ => None,
    }
}

#[tokio::test]
async fn handshakes_loads_kernel_and_answers_ping() {
    let mut child = spawn_mag().await;
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    // Drain the plugin's stderr so its tracing output surfaces in the
    // test log on failure (and its pipe can never fill up).
    let stderr = child.stderr.take().expect("stderr");
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[mag stderr] {line}");
        }
    });

    // 1. Handshake: plugin emits `ready`, we reply `ready_ok`.
    let ready = read_outgoing(&mut reader, "system ready").await;
    assert!(
        matches!(ready.body, Body::System(SystemBody::Ready { .. })),
        "first message must be system ready"
    );
    send_ready_ok(&mut stdin).await;

    // 2. Kernel loaded: the next event is `mag.hello`, and because the
    //    stub kernel returns `{ name = "mag-kernel" }`, hello carries it.
    let hello = read_outgoing(&mut reader, "mag.hello").await;
    assert_eq!(event_kind(&hello), Some("mag.hello"));
    if let Body::Event(map) = &hello.body {
        assert_eq!(
            map.get("kernel").and_then(Value::as_str),
            Some("mag-kernel"),
            "hello must report the loaded stub kernel name"
        );
    }

    // 3. Liveness: ping -> pong echoing the id.
    let mut ping = Map::new();
    ping.insert("kind".into(), Value::String("mag.ping".into()));
    ping.insert("id".into(), Value::String("ping-1".into()));
    send_event(&mut stdin, ping).await;

    let pong = loop {
        let out = read_outgoing(&mut reader, "mag.pong").await;
        if event_kind(&out) == Some("mag.pong") {
            break out;
        }
    };
    if let Body::Event(map) = &pong.body {
        assert_eq!(
            map.get("in_reply_to").and_then(Value::as_str),
            Some("ping-1"),
            "pong must echo the ping id"
        );
    }

    // 4. Teardown.
    write_env(
        &mut stdin,
        Envelope::system(
            PluginName::engine(),
            Timestamp::now(),
            SystemBody::Shutdown {
                reason: None,
                grace_ms: None,
            },
        ),
    )
    .await;
    drop(stdin);
    let _ = timeout(Duration::from_secs(10), child.wait()).await;
}
