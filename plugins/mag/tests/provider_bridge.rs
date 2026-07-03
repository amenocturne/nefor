//! Focused provider-bridge integration test: one `llm` actor completes
//! end-to-end through the bridge against a counterpart speaking the REAL
//! `chat.*` protocol.
//!
//! This is the minimal shape of the test that would have caught the
//! provider-capability-bridge gap. The kernel emits one `capability.invoke`; the
//! mag plugin's bridge must translate it into `chat.create` → `chat.append` →
//! `chat.complete` (asserted here field-by-field — model/system/tools threaded,
//! the factory's chat_id kept), and feed the single streamed
//! `chat.complete.result` back as the correlated reply so the `llm` exits to
//! `FinalAnswer` and the run completes. It also asserts:
//!   - deltas are the provider's own bus emissions: a `stream.delta` on the wire
//!     is ignored by the plugin (the kernel stays delta-blind) and does not
//!     disturb completion;
//!   - the bridge cleans up with a `chat.delete` after collecting the result.
//!
//! A scripted `tool.invoke` responder (the old harness shape) would never emit a
//! `chat.create`, so this hangs against the pre-bridge plugin — exactly the gap.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::time::timeout;

const PROVIDER: &str = "test-provider";
const SESSION_ID: &str = "bridge-session";
const RUN_NAME: &str = "bridge-run";
const READ_TIMEOUT: Duration = Duration::from_secs(30);

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

fn event_body(out: &PluginOutgoing) -> Option<&Map<String, Value>> {
    match &out.body {
        Body::Event(map) => Some(map),
        _ => None,
    }
}

fn body_kind(body: &Map<String, Value>) -> Option<&str> {
    body.get("kind").and_then(Value::as_str)
}

/// A `<provider>.stream.delta` — the provider's own streaming emission for the
/// TUI. The kernel is delta-blind; the plugin must ignore it.
fn stream_delta(chat_id: &str, text: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{PROVIDER}.stream.delta")),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_owned()));
    m.insert("text".into(), Value::String(text.to_owned()));
    m
}

/// A `<provider>.chat.complete.result { chat_id, output }` — the streamed final.
fn chat_result(chat_id: &str, output: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{PROVIDER}.chat.complete.result")),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_owned()));
    m.insert("output".into(), output);
    m
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

#[tokio::test]
async fn one_llm_actor_completes_end_to_end_through_the_bridge() {
    let data_dir = std::env::temp_dir().join(format!("mag-bridge-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
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

    // One llm actor seeded with a ProviderOut turn, routing its FinalAnswer to
    // the program sink. No adapter needed — seed the llm input directly.
    let modification = json!({
        "actors": [
            {
                "id": "agent",
                "factory": "llm",
                "params": {
                    "model": "opus",
                    "provider": PROVIDER,
                    "system": "be helpful",
                    "tools": ["fs/read", "grep"]
                },
                "routes": { "generic-provider.FinalAnswer": ["sink"] }
            },
            { "id": "sink", "factory": "sink", "params": {}, "routes": {} }
        ],
        "messages": [
            { "to": "agent", "content": {
                "kind": "generic-provider.ProviderOut",
                "messages": [ { "role": "user", "content": "explore the codebase" } ]
            } }
        ],
        "kills": [],
        "rules": []
    });

    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String("exec-bridge".into()));
    execute.insert("session_id".into(), Value::String(SESSION_ID.into()));
    execute.insert("run_id".into(), Value::String(RUN_NAME.into()));
    execute.insert("run_name".into(), Value::String(RUN_NAME.into()));
    execute.insert("modification".into(), modification);
    send_event(&mut stdin, execute).await;

    let create_kind = format!("{PROVIDER}.chat.create");
    let append_kind = format!("{PROVIDER}.chat.append");
    let complete_kind = format!("{PROVIDER}.chat.complete");
    let delete_kind = format!("{PROVIDER}.chat.delete");

    let mut saw_create = false;
    let mut saw_append = false;
    let mut saw_delete = false;
    let mut saw_bare_tool_invoke = false;
    let mut chat_id = String::new();
    let run_result;

    loop {
        let out = read_outgoing(&mut reader, "bridge event").await;
        let body = match event_body(&out) {
            Some(b) => b.clone(),
            None => continue,
        };
        let kind = match body_kind(&body) {
            Some(k) => k.to_owned(),
            None => continue,
        };

        if kind == "tool.invoke" {
            saw_bare_tool_invoke = true;
        } else if kind == create_kind {
            saw_create = true;
            chat_id = body
                .get("chat_id")
                .and_then(Value::as_str)
                .expect("create carries chat_id")
                .to_owned();
            // The factory's chat_id (agent@r<seq>) is kept, and call config is
            // threaded from the request.
            assert!(
                chat_id.starts_with("agent@r"),
                "create keeps the factory chat_id; got {chat_id}"
            );
            assert_eq!(body.get("model").and_then(Value::as_str), Some("opus"));
            assert_eq!(
                body.get("system").and_then(Value::as_str),
                Some("be helpful")
            );
            assert!(
                body.get("tools").and_then(Value::as_array).is_some(),
                "create threads the tool list"
            );
        } else if kind == append_kind {
            saw_append = true;
            let msg = body
                .get("message")
                .and_then(Value::as_object)
                .expect("append carries a message");
            assert_eq!(msg.get("role").and_then(Value::as_str), Some("user"));
            assert_eq!(
                msg.get("content").and_then(Value::as_str),
                Some("explore the codebase"),
                "the ProviderOut turn message is appended verbatim"
            );
        } else if kind == complete_kind {
            assert!(saw_create && saw_append, "complete follows create+append");
            let cid = body
                .get("chat_id")
                .and_then(Value::as_str)
                .expect("complete carries chat_id")
                .to_owned();
            // Provider emits a delta first (TUI streaming) — the plugin must
            // ignore it (kernel stays delta-blind) — then the final result.
            send_event(&mut stdin, stream_delta(&cid, "explor")).await;
            send_event(&mut stdin, chat_result(&cid, json!({ "text": "all done" }))).await;
        } else if kind == delete_kind {
            saw_delete = true;
            assert_eq!(
                body.get("chat_id").and_then(Value::as_str),
                Some(chat_id.as_str()),
                "the collected chat is cleaned up by chat_id"
            );
        } else if kind == "mag.run_result" {
            run_result = body;
            break;
        }
    }

    assert!(
        !saw_bare_tool_invoke,
        "a provider request must never be a bare tool.invoke"
    );
    assert!(saw_create && saw_append, "the bridge drove create + append");
    assert!(
        saw_delete,
        "the bridge cleaned up the chat with chat.delete"
    );

    assert_eq!(
        run_result.get("status").and_then(Value::as_str),
        Some("completed"),
        "the single-actor program ran to completion through the bridge"
    );
    assert_eq!(
        run_result.get("in_reply_to").and_then(Value::as_str),
        Some("exec-bridge")
    );
    let sink_path = run_result
        .get("output_path")
        .and_then(Value::as_str)
        .expect("run_result carries the sink output PATH");
    let sink_output = std::fs::read_to_string(sink_path).expect("read persisted sink output");
    assert!(
        sink_output.contains("all done"),
        "the sink persisted the provider's final answer; got {sink_output:?}"
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
