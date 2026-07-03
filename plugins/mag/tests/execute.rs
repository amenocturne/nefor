//! In-process execute integration test for the mag plugin.
//!
//! Spawns the compiled `mag-plugin`, completes the handshake, then drives a
//! `mag.execute` with an inline synchronous modification through the kernel and
//! asserts:
//!   - the constellation's lifecycle events stream onto the NCP wire
//!     (`mag.run_started`, actor spawn/ready, `mag.modification_applied`,
//!     `mag.run_complete`),
//!   - the ready barrier releases and the run completes in the same turn, and
//!   - the terminal `mag.run_result` carries the sink's output PATH (control
//!     plane reads paths, never node data), and that file was persisted.
//!
//! The modification is passed **inline** rather than compiled from a shipped
//! `.mag` fixture: the shipped programs (`two-agents.mag`) instantiate `llm`
//! actors bound to `chatgpt-provider`, which isn't present in tests and would
//! hang every actor pending on a provider round-trip. A single `sink` seeded
//! with a `generic-provider.FinalAnswer` uses only the synchronous shipped
//! factory and runs to completion with no bus round-trip — exercising the full
//! begin_run → start → barrier → deliver → run-complete drive.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::time::timeout;

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mag-plugin"))
}

fn kernel_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../starter/mag-kernel/init.lua")
}

async fn spawn_mag(data_dir: &std::path::Path) -> Child {
    let mut cmd = tokio::process::Command::new(binary_path());
    cmd.arg("--kernel")
        .arg(kernel_path())
        .env("NEFOR_DATA_DIR", data_dir)
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

/// Read outgoing events until one matches `kind`, collecting every event kind
/// seen along the way (so the test can assert the lifecycle stream), and
/// returning the matching event.
async fn read_collecting<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    kind: &str,
    seen: &mut Vec<String>,
) -> PluginOutgoing {
    loop {
        let out = read_outgoing(reader, kind).await;
        if let Some(k) = event_kind(&out) {
            seen.push(k.to_owned());
            if k == kind {
                return out;
            }
        }
    }
}

#[tokio::test]
async fn executes_synchronous_sink_program_and_streams_lifecycle_events() {
    let data_dir = std::env::temp_dir().join(format!("mag-execute-{}", std::process::id()));
    std::fs::create_dir_all(&data_dir).expect("mkdir data dir");

    let mut child = spawn_mag(&data_dir).await;
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

    // A single sink seeded with a FinalAnswer: synchronous, terminal, no bus
    // round-trip. session_id/run_name drive per-node output persistence.
    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String("exec-1".into()));
    execute.insert("session_id".into(), Value::String("test-session".into()));
    execute.insert("run_id".into(), Value::String("sink-run".into()));
    execute.insert("run_name".into(), Value::String("sink-run".into()));
    execute.insert(
        "modification".into(),
        json!({
            "actors": [ { "id": "sink", "factory": "sink", "params": {}, "routes": {} } ],
            "messages": [
                { "to": "sink", "content": { "kind": "generic-provider.FinalAnswer", "text": "hello from mag execute" } }
            ],
            "kills": [],
            "rules": []
        }),
    );
    send_event(&mut stdin, execute).await;

    // Drive to the terminal reply, collecting the lifecycle stream on the way.
    let mut seen: Vec<String> = Vec::new();
    let result = read_collecting(&mut reader, "mag.run_result", &mut seen).await;

    for expected in [
        "mag.run_started",
        "mag.actor_ready",
        "mag.modification_applied",
        "mag.run_complete",
    ] {
        assert!(
            seen.iter().any(|k| k == expected),
            "expected lifecycle event {expected} on the wire; saw {seen:?}"
        );
    }

    let body = event_body(&result).expect("run_result event body");
    assert_eq!(
        body.get("in_reply_to").and_then(Value::as_str),
        Some("exec-1"),
        "run_result correlates to the execute request"
    );
    assert_eq!(
        body.get("status").and_then(Value::as_str),
        Some("completed"),
        "synchronous sink run completes"
    );
    assert_eq!(body.get("run_id").and_then(Value::as_str), Some("sink-run"));
    let output_path = body
        .get("output_path")
        .and_then(Value::as_str)
        .expect("run_result carries the sink output PATH (control-plane semantics)");
    assert!(
        std::path::Path::new(output_path).exists(),
        "the persisted sink output file exists at {output_path}"
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
    std::fs::remove_dir_all(&data_dir).ok();
}

/// Complete the NCP ready handshake for a freshly spawned mag plugin.
async fn handshake<R: AsyncBufReadExt + Unpin>(reader: &mut R, stdin: &mut ChildStdin) {
    let ready = read_outgoing(reader, "system ready").await;
    assert!(matches!(ready.body, Body::System(SystemBody::Ready { .. })));
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

/// A `mag.execute` carrying a `params_overlay` runs to completion: the overlay
/// is parsed and applied before spawn (unknown ids ignored, named ids patched)
/// and never breaks the run. The precise merge semantics are unit-tested in
/// `apply_params_overlay`; this asserts the wire surface end-to-end.
#[tokio::test]
async fn execute_accepts_and_applies_params_overlay() {
    let data_dir = std::env::temp_dir().join(format!("mag-overlay-{}", std::process::id()));
    std::fs::create_dir_all(&data_dir).expect("mkdir data dir");

    let mut child = spawn_mag(&data_dir).await;
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

    handshake(&mut reader, &mut stdin).await;

    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String("exec-overlay".into()));
    execute.insert("session_id".into(), Value::String("test-session".into()));
    execute.insert("run_id".into(), Value::String("overlay-run".into()));
    execute.insert("run_name".into(), Value::String("overlay-run".into()));
    execute.insert(
        "modification".into(),
        json!({
            "actors": [ { "id": "sink", "factory": "sink", "params": {}, "routes": {} } ],
            "messages": [
                { "to": "sink", "content": { "kind": "generic-provider.FinalAnswer", "text": "overlaid" } }
            ],
            "kills": [],
            "rules": []
        }),
    );
    // A patch for the sink plus a no-op patch for an id that isn't in the
    // constellation — the unknown id must be ignored, not error.
    execute.insert(
        "params_overlay".into(),
        json!({
            "sink":    { "reasoning_effort": "high" },
            "ghost":   { "model": "does-not-exist" }
        }),
    );
    send_event(&mut stdin, execute).await;

    let mut seen: Vec<String> = Vec::new();
    let result = read_collecting(&mut reader, "mag.run_result", &mut seen).await;
    let body = event_body(&result).expect("run_result body");
    assert_eq!(
        body.get("status").and_then(Value::as_str),
        Some("completed"),
        "overlay-carrying execute still runs to completion; saw {seen:?}"
    );
    assert_eq!(
        body.get("in_reply_to").and_then(Value::as_str),
        Some("exec-overlay")
    );

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
    std::fs::remove_dir_all(&data_dir).ok();
}

/// `mag.apply` outside a live run is rejected with the named guard error — the
/// control plane's apply authority is scoped to an active session with an
/// in-flight run (docs/ir.md, Kernel operations).
#[tokio::test]
async fn apply_without_live_run_is_rejected() {
    let data_dir = std::env::temp_dir().join(format!("mag-apply-guard-{}", std::process::id()));
    std::fs::create_dir_all(&data_dir).expect("mkdir data dir");

    let mut child = spawn_mag(&data_dir).await;
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

    handshake(&mut reader, &mut stdin).await;

    // No mag.execute first → no live run. The apply must be rejected.
    let mut apply = Map::new();
    apply.insert("kind".into(), Value::String("mag.apply".into()));
    apply.insert("id".into(), Value::String("apply-norun".into()));
    apply.insert("source".into(), Value::String("test".into()));
    apply.insert(
        "modification".into(),
        json!({ "actors": [], "messages": [], "kills": ["whatever"], "rules": [] }),
    );
    send_event(&mut stdin, apply).await;

    let mut seen: Vec<String> = Vec::new();
    let applied = read_collecting(&mut reader, "mag.applied", &mut seen).await;
    let body = event_body(&applied).expect("applied body");
    assert_eq!(
        body.get("in_reply_to").and_then(Value::as_str),
        Some("apply-norun")
    );
    assert_eq!(
        body.get("ok").and_then(Value::as_bool),
        Some(false),
        "apply outside a live run is rejected"
    );
    let err = body
        .get("error")
        .and_then(Value::as_str)
        .expect("rejection carries a named error");
    assert!(
        err.contains("no live run"),
        "named guard error explains the policy; got {err:?}"
    );

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
    std::fs::remove_dir_all(&data_dir).ok();
}
