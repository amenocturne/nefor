//! Focused tool-leg integration test: one agent loop completes end-to-end
//! through the capability bridge's TOOL leg against a counterpart speaking the
//! REAL tool-gate protocol.
//!
//! This is the minimal shape of the test that would have caught the tool-leg
//! gap. The kernel's `run-tool` actor emits a `capability.invoke`; routing.lua
//! puts a double-wrapped `tool.invoke { id, name, args = { name, args,
//! allowlist, da-policy } }` on the plugin's emit queue — but nothing on the
//! bus subscribes to a bare `tool.invoke` (tool-gate prefix-routes on
//! `<gate>.tool.invoke`, basic-tools on `basic-tools.tool.invoke`), so before
//! the bridge's tool leg a real run hung at its first tool call. The plugin
//! must rewrite it to `<gate>.tool.invoke` with:
//!   - the payload unwrapped to the gate's contract (`args` = the tool's own
//!     args, asserted field-by-field);
//!   - the kernel correlation id kept as the gate's outer id;
//!   - per-node gating (`allowlist` / `da-policy` from the run-tool node's
//!     params) threaded top-level.
//!
//! The harness answers like the gate does — a broadcast `tool.result { id,
//! output }` keyed by the same id — and the run must proceed through
//! tool-result to a second provider turn and complete.
//!
//! The gate name is spawn-config-threaded (`--tool-gate`), mirroring how the
//! starter owns cross-plugin names; this test passes a NON-default name
//! (`custom-gate`) to pin that the target is threaded, not hard-coded.
//!
//! A scripted bare-`tool.invoke` responder (the old harness shape) would answer
//! an envelope the real bus never delivers anywhere, so this test hangs against
//! the pre-bridge plugin — exactly the gap.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::time::timeout;

const PROVIDER: &str = "test-provider";
/// Deliberately NOT the shipped default ("tool-gate"): pins that the gate
/// target comes from the spawn config, not a hard-coded name.
const GATE: &str = "custom-gate";
const SESSION_ID: &str = "tool-bridge-session";
const RUN_NAME: &str = "tool-bridge-run";
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
        .arg("--tool-gate")
        .arg(GATE)
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

/// A `<provider>.chat.complete.result { chat_id, output }` — the provider leg's
/// streamed final, answering each driven `chat.complete`.
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

/// The gate's reply shape (plugins/tool-gate handle_tool_result): a broadcast
/// `tool.result { id, output }` keyed by the caller's outer id — here the
/// kernel correlation id the bridge preserved.
fn tool_result(id: &str, output: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("tool.result".into()));
    m.insert("id".into(), Value::String(id.to_owned()));
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

/// One agent loop (`llm → run-tool → tool-result → llm`) with per-node
/// gating on the run-tool node, exiting to the program sink.
fn agent_loop_modification() -> Value {
    json!({
        "actors": [
            {
                "id": "agent.llm",
                "factory": "llm",
                "params": { "model": "opus", "provider": PROVIDER, "system": "work" },
                "routes": {
                    "generic-tool.ToolCalls": ["agent.run-tool"],
                    "generic-provider.FinalAnswer": ["sink"]
                }
            },
            {
                "id": "agent.run-tool",
                "factory": "run-tool",
                "params": {
                    "allowlist": ["list_dir", "read_file"],
                    "da-policy": { "git": "read" }
                },
                "routes": { "generic-tool.ToolHandle": ["agent.tool-result"] }
            },
            {
                "id": "agent.tool-result",
                "factory": "tool-result",
                "params": {},
                "routes": { "generic-provider.ProviderOut": ["agent.llm"] }
            },
            { "id": "sink", "factory": "sink", "params": {}, "routes": {} }
        ],
        "messages": [
            { "to": "agent.llm", "content": {
                "kind": "generic-provider.ProviderOut",
                "messages": [ { "role": "user", "content": "list the repo" } ]
            } }
        ],
        "kills": [],
        "rules": []
    })
}

#[tokio::test]
async fn tool_call_round_trips_through_the_gate_and_the_run_completes() {
    let data_dir = std::env::temp_dir().join(format!("mag-tool-bridge-{}", std::process::id()));
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

    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String("exec-tool-bridge".into()));
    execute.insert("session_id".into(), Value::String(SESSION_ID.into()));
    execute.insert("run_id".into(), Value::String(RUN_NAME.into()));
    execute.insert("run_name".into(), Value::String(RUN_NAME.into()));
    execute.insert("modification".into(), agent_loop_modification());
    send_event(&mut stdin, execute).await;

    let complete_kind = format!("{PROVIDER}.chat.complete");
    let gate_invoke_kind = format!("{GATE}.tool.invoke");
    let mut turn = 0u32;
    let mut gate_request_id: Option<String> = None;
    let run_result;

    loop {
        let out = read_outgoing(&mut reader, "tool-bridge event").await;
        let body = match event_body(&out) {
            Some(b) => b.clone(),
            None => continue,
        };
        let kind = match body_kind(&body) {
            Some(k) => k.to_owned(),
            None => continue,
        };

        if kind == "tool.invoke" {
            // The deadlock regression: nothing on the bus subscribes to a bare
            // tool.invoke, so the run would hang at this call forever.
            panic!(
                "bare tool.invoke reached the wire (name {:?}) — the tool leg \
                 must be rewritten to {gate_invoke_kind}",
                body.get("name")
            );
        } else if kind == complete_kind {
            let chat_id = body
                .get("chat_id")
                .and_then(Value::as_str)
                .expect("chat.complete carries chat_id")
                .to_owned();
            turn += 1;
            match turn {
                // Turn 1: the model asks for a tool. This drives run-tool into
                // its capability.invoke — the tool leg under test.
                1 => {
                    send_event(
                        &mut stdin,
                        chat_result(
                            &chat_id,
                            json!({ "tool_calls": [
                                { "id": "call-1", "name": "list_dir", "args": { "path": "." } }
                            ] }),
                        ),
                    )
                    .await;
                }
                // Turn 2 fires only after the tool result round-tripped back
                // into the loop. Exit with a final answer.
                2 => {
                    assert!(
                        gate_request_id.is_some(),
                        "the second provider turn must be driven by the tool result"
                    );
                    send_event(
                        &mut stdin,
                        chat_result(&chat_id, json!({ "text": "listed-and-done" })),
                    )
                    .await;
                }
                n => panic!("unexpected provider turn {n} (chat_id {chat_id})"),
            }
        } else if kind == gate_invoke_kind {
            // The rewritten invoke: gate contract, field by field.
            let id = body
                .get("id")
                .and_then(Value::as_str)
                .expect("gate invoke keeps the kernel correlation id")
                .to_owned();
            assert_eq!(
                body.get("name").and_then(Value::as_str),
                Some("list_dir"),
                "gate invoke names the tool"
            );
            // Observability stamp: the emitting actor's plain address rides
            // the gate envelope (routing.lua on_capability_invoke →
            // bridge.rs gate_invoke).
            assert_eq!(
                body.get("from").and_then(Value::as_str),
                Some("agent.run-tool"),
                "gate invoke carries the emitting actor id"
            );
            assert_eq!(
                body.get("args"),
                Some(&json!({ "path": "." })),
                "the kernel's double-wrapped payload is unwrapped to the tool's own args"
            );
            // Per-node gating from the run-tool node's params rides top-level.
            assert_eq!(
                body.get("allowlist"),
                Some(&json!(["list_dir", "read_file"])),
                "the node's allowlist is threaded to the gate"
            );
            assert_eq!(
                body.get("da-policy"),
                Some(&json!({ "git": "read" })),
                "the node's da-policy is threaded to the gate"
            );
            gate_request_id = Some(id.clone());
            // Answer like the gate: broadcast tool.result keyed by the same id.
            send_event(&mut stdin, tool_result(&id, json!("dir-listing"))).await;
        } else if kind == "mag.run_result" {
            run_result = body;
            break;
        }
    }

    assert!(
        gate_request_id.is_some(),
        "the run made a gated tool invocation"
    );
    assert_eq!(
        run_result.get("status").and_then(Value::as_str),
        Some("completed"),
        "the run completed after the tool round-trip; got {run_result:?}"
    );
    assert_eq!(
        run_result.get("in_reply_to").and_then(Value::as_str),
        Some("exec-tool-bridge")
    );
    let sink_path = run_result
        .get("output_path")
        .and_then(Value::as_str)
        .expect("run_result carries the sink output PATH");
    let sink_output = std::fs::read_to_string(sink_path).expect("read persisted sink output");
    assert!(
        sink_output.contains("listed-and-done"),
        "the sink persisted the post-tool final answer; got {sink_output:?}"
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
