//! In-process load integration test for the mag plugin.
//!
//! Spawns the compiled `mag-plugin`, completes the handshake, then sends a
//! `mag.load` for the two-agents fixture workspace and asserts the plugin
//! evaluates it in-process (no `io.popen` bridge) and replies `mag.loaded` with
//! the initial graph modification. Mirrors the subprocess-driving pattern of
//! `liveness.rs`.

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

fn kernel_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel/init.lua")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
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

fn event_body(out: &PluginOutgoing) -> Option<&Map<String, Value>> {
    match &out.body {
        Body::Event(map) => Some(map),
        _ => None,
    }
}

/// Read outgoing events until one matches `kind`, skipping the plugin's own
/// unrelated broadcasts (e.g. `mag.hello`).
async fn read_until<R: AsyncBufReadExt + Unpin>(reader: &mut R, kind: &str) -> PluginOutgoing {
    loop {
        let out = read_outgoing(reader, kind).await;
        if event_kind(&out) == Some(kind) {
            return out;
        }
    }
}

#[tokio::test]
async fn loads_two_agents_fixture_and_returns_initial_modification() {
    let mut child = spawn_mag().await;
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    let stderr = child.stderr.take().expect("stderr");
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[mag stderr] {line}");
        }
    });

    // Handshake.
    let ready = read_outgoing(&mut reader, "system ready").await;
    assert!(matches!(ready.body, Body::System(SystemBody::Ready { .. })));
    write_env(
        &mut stdin,
        Envelope::system(
            PluginName::engine(),
            Timestamp::now(),
            SystemBody::ReadyOk {
                engine_version: "test".into(),
            },
        ),
    )
    .await;

    // Load the fixture workspace in-process.
    let mut load = Map::new();
    load.insert("kind".into(), Value::String("mag.load".into()));
    load.insert("id".into(), Value::String("load-1".into()));
    load.insert(
        "source_dir".into(),
        Value::String(fixtures_dir().display().to_string()),
    );
    load.insert("entry".into(), Value::String("two-agents.mag".into()));
    send_event(&mut stdin, load).await;

    // Expect `mag.loaded` with the initial modification.
    let loaded = read_until(&mut reader, "mag.loaded").await;
    let body = event_body(&loaded).expect("event body");
    assert_eq!(
        body.get("in_reply_to").and_then(Value::as_str),
        Some("load-1"),
        "reply must correlate to the request id"
    );
    assert!(
        body.get("hash")
            .and_then(Value::as_str)
            .is_some_and(|h| h.starts_with("sha256:")),
        "loaded reply must carry the program hash"
    );
    let modification = body
        .get("modification")
        .and_then(Value::as_object)
        .expect("modification object");
    let actors = modification
        .get("actors")
        .and_then(Value::as_array)
        .expect("actors array");
    assert!(
        actors
            .iter()
            .any(|a| a.get("id").and_then(Value::as_str) == Some("sink")),
        "the lowered modification must contain the program sink"
    );
    assert!(
        actors
            .iter()
            .any(|a| a.get("id").and_then(Value::as_str) == Some("docs-explorer.llm")),
        "the lowered modification must contain the namespaced agent actors"
    );

    // Teardown.
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
