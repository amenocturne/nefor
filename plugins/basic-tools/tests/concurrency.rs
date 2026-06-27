use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nefor_protocol::{
    Body, Envelope, MessageKind, PluginName, PluginOutgoing, SystemBody, Timestamp,
};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::time::timeout;

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_basic-tools"))
}

async fn spawn_basic_tools() -> Child {
    let mut cmd = Command::new(binary_path());
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn().expect("spawn basic-tools")
}

async fn read_outgoing<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> PluginOutgoing {
    let mut line = String::new();
    match timeout(Duration::from_secs(10), reader.read_line(&mut line)).await {
        Ok(Ok(0)) => panic!("basic-tools stdout closed"),
        Ok(Ok(_)) => PluginOutgoing::parse_line(line.trim_end()).expect("parse outgoing"),
        Ok(Err(e)) => panic!("read basic-tools stdout: {e}"),
        Err(_) => panic!("timed out waiting for basic-tools output"),
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

fn event(kind: &str, fields: &[(&str, Value)]) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert("kind".into(), Value::String(kind.into()));
    for (key, value) in fields {
        body.insert((*key).into(), value.clone());
    }
    body
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

async fn send_shutdown(stdin: &mut ChildStdin) {
    write_env(
        stdin,
        Envelope::system(
            PluginName::engine(),
            Timestamp::now(),
            SystemBody::Shutdown {
                reason: Some("test done".into()),
                grace_ms: Some(500),
            },
        ),
    )
    .await;
}

async fn send_bash(stdin: &mut ChildStdin, id: &str, command: &str) {
    let args = serde_json::json!({ "command": command, "timeout_ms": 10000 });
    write_env(
        stdin,
        Envelope::event(
            PluginName::engine(),
            Timestamp::now(),
            event(
                "basic-tools.tool.invoke",
                &[
                    ("id", Value::String(id.into())),
                    ("name", Value::String("bash".into())),
                    ("args", args),
                ],
            ),
        ),
    )
    .await;
}

fn tool_result_id(out: &PluginOutgoing) -> Option<String> {
    let body = match &out.body {
        Body::Event(body) if out.kind == MessageKind::Event => body,
        _ => return None,
    };
    if body.get("kind").and_then(Value::as_str) != Some("tool.result") {
        return None;
    }
    body.get("id").and_then(Value::as_str).map(str::to_owned)
}

#[tokio::test]
async fn bash_invocations_complete_as_their_processes_finish() {
    let mut child = spawn_basic_tools().await;
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    let ready = read_outgoing(&mut reader).await;
    assert!(matches!(ready.body, Body::System(SystemBody::Ready { .. })));
    send_ready_ok(&mut stdin).await;

    // Drain hello + tool registration.
    let _hello = read_outgoing(&mut reader).await;
    let _register = read_outgoing(&mut reader).await;

    send_bash(&mut stdin, "slow-call", "sleep 2; echo SLOW_DONE").await;
    send_bash(&mut stdin, "fast-call", "sleep 0.1; echo FAST_DONE").await;

    let mut results = Vec::new();
    while results.len() < 2 {
        let out = read_outgoing(&mut reader).await;
        if let Some(id) = tool_result_id(&out) {
            results.push(id);
        }
    }

    send_shutdown(&mut stdin).await;
    drop(stdin);
    let _ = timeout(Duration::from_secs(10), child.wait()).await;

    assert_eq!(
        results,
        vec!["fast-call".to_string(), "slow-call".to_string()],
        "basic-tools must not serialize independent bash invocations"
    );
}
