//! Human-gate approval round-trip across the plugin boundary
//! (docs/actor-model.md, The approval boundary).
//!
//! Spawns the compiled `mag-plugin`, drives a `mag.execute` whose program is a
//! human gate feeding a sink, and asserts:
//!   - the gate's subject surfaces on the NCP wire as the run_id-stamped
//!     `mag.approval_request` control-plane event (the notification the chat
//!     surface renders) while the run stays pending;
//!   - the control plane's reply — a `mag.apply` carrying a
//!     `mag.ApprovalReply` message at the gate — resolves the gate, routes
//!     `human.Approved` into the sink, and settles the execute with the
//!     approved result inline;
//!   - a reply at a gate that never constructed (no outstanding request) is
//!     rejected loudly at apply (`mag.applied ok:false`), leaving the run
//!     untouched.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::time::timeout;

const READ_TIMEOUT: Duration = Duration::from_secs(120);

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
        .env_remove("NEFOR_DEV_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn().expect("spawn mag-plugin")
}

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
/// seen along the way, and returning the matching event.
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

/// The gate program: a human approval gate whose `human.Approved` exit feeds
/// the sink. With `seed`, the gate is fired with a subject at start; without,
/// it registers but never constructs (lazy construction).
fn gate_execute(run_id: &str, seed: bool) -> Map<String, Value> {
    let messages = if seed {
        json!([ { "to": "gate", "content": { "kind": "generic-provider.FinalAnswer", "text": "the plan" } } ])
    } else {
        json!([])
    };
    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String(format!("exec-{run_id}")));
    execute.insert("session_id".into(), Value::String("test-session".into()));
    execute.insert("run_id".into(), Value::String(run_id.to_owned()));
    execute.insert("run_name".into(), Value::String(run_id.to_owned()));
    execute.insert(
        "modification".into(),
        json!({
            "actors": [
                { "id": "gate", "factory": "human",
                  "params": { "prompt": "Approve the plan?" },
                  "routes": { "human.Approved": ["sink"] } },
                { "id": "sink", "factory": "sink", "params": {}, "routes": {} }
            ],
            "messages": messages,
            "kills": [],
            "rules": []
        }),
    );
    execute
}

fn approval_reply_apply(run_id: &str, id: &str) -> Map<String, Value> {
    let mut apply = Map::new();
    apply.insert("kind".into(), Value::String("mag.apply".into()));
    apply.insert("id".into(), Value::String(id.to_owned()));
    apply.insert("run_id".into(), Value::String(run_id.to_owned()));
    apply.insert("source".into(), Value::String("test".into()));
    apply.insert(
        "modification".into(),
        json!({
            "messages": [
                { "to": "gate",
                  "content": { "kind": "mag.ApprovalReply", "approved": true, "content": "ship it" } }
            ]
        }),
    );
    apply
}

#[tokio::test]
async fn approval_reply_via_apply_resolves_the_gate_and_settles_the_run() {
    let data_dir = std::env::temp_dir().join(format!("mag-approval-{}", std::process::id()));
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

    send_event(&mut stdin, gate_execute("gate-run", true)).await;

    // The subject fires the gate and the notification surfaces on the wire —
    // run_id-stamped, naming the gate, carrying the prompt and the subject.
    // The run must NOT settle: the gate is pending on the human.
    let mut seen: Vec<String> = Vec::new();
    let request = read_collecting(&mut reader, "mag.approval_request", &mut seen).await;
    assert!(
        !seen.iter().any(|k| k == "mag.run_result"),
        "the run must wait on the human; saw {seen:?}"
    );
    let body = event_body(&request).expect("approval_request body");
    assert_eq!(body.get("run_id").and_then(Value::as_str), Some("gate-run"));
    assert_eq!(body.get("from").and_then(Value::as_str), Some("gate"));
    assert_eq!(
        body.get("correlation").and_then(Value::as_str),
        Some("gate")
    );
    assert_eq!(
        body.get("prompt").and_then(Value::as_str),
        Some("Approve the plan?")
    );
    assert_eq!(
        body.get("subject")
            .and_then(Value::as_object)
            .and_then(|s| s.get("text"))
            .and_then(Value::as_str),
        Some("the plan"),
        "the notification carries the subject; got {body:?}"
    );

    // The human approves: the control plane injects the reply via mag.apply.
    send_event(&mut stdin, approval_reply_apply("gate-run", "apply-reply")).await;

    let mut seen: Vec<String> = Vec::new();
    let applied = read_collecting(&mut reader, "mag.applied", &mut seen).await;
    let applied_body = event_body(&applied).expect("applied body");
    assert_eq!(
        applied_body.get("in_reply_to").and_then(Value::as_str),
        Some("apply-reply")
    );
    assert_eq!(
        applied_body.get("ok").and_then(Value::as_bool),
        Some(true),
        "the reply injection is accepted; got {applied_body:?}"
    );

    // The resolved gate routed human.Approved into the sink: the run settles
    // with the approved result inline.
    let result = read_collecting(&mut reader, "mag.run_result", &mut seen).await;
    let result_body = event_body(&result).expect("run_result body");
    assert_eq!(
        result_body.get("status").and_then(Value::as_str),
        Some("completed"),
        "the approval completes the run; got {result_body:?}"
    );
    assert_eq!(
        result_body.get("run_id").and_then(Value::as_str),
        Some("gate-run")
    );
    let final_result = result_body
        .get("result")
        .and_then(Value::as_object)
        .expect("run_result carries the approved result inline");
    assert_eq!(
        final_result.get("kind").and_then(Value::as_str),
        Some("human.Approved")
    );
    assert_eq!(
        final_result.get("content").and_then(Value::as_str),
        Some("ship it"),
        "the result carries the human's content"
    );
    assert_eq!(
        final_result
            .get("subject")
            .and_then(Value::as_object)
            .and_then(|s| s.get("text"))
            .and_then(Value::as_str),
        Some("the plan"),
        "the result carries the approved subject"
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

#[tokio::test]
async fn approval_reply_before_construction_is_rejected_loudly() {
    let data_dir = std::env::temp_dir().join(format!("mag-approval-reject-{}", std::process::id()));
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

    // No seed: the gate registers but never fires — lazy construction leaves
    // it unconstructed, so no request is ever outstanding.
    send_event(&mut stdin, gate_execute("idle-run", false)).await;
    let mut seen: Vec<String> = Vec::new();
    read_collecting(&mut reader, "mag.modification_applied", &mut seen).await;

    send_event(
        &mut stdin,
        approval_reply_apply("idle-run", "apply-premature"),
    )
    .await;
    let applied = read_collecting(&mut reader, "mag.applied", &mut seen).await;
    let body = event_body(&applied).expect("applied body");
    assert_eq!(
        body.get("in_reply_to").and_then(Value::as_str),
        Some("apply-premature")
    );
    assert_eq!(
        body.get("ok").and_then(Value::as_bool),
        Some(false),
        "a reply at an unconstructed gate is rejected; got {body:?}"
    );
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .expect("the rejection carries the error");
    assert!(
        error.contains("no outstanding approval request"),
        "the error names the contract; got {error:?}"
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
