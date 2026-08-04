//! Canonical MAG → ChatGPT Responses tool regression.
//!
//! This routes real plugin processes over NCP. The fake HTTP endpoint only scripts the model:
//! the first response calls the shipped `basic-tools` `read_file` tool, MAG routes that call
//! through the configured tool gate, and the second response returns the structured final answer
//! after observing the real file contents.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::time::timeout;

const READ_TIMEOUT: Duration = Duration::from_secs(90);
const PROVIDER: &str = "chatgpt-tools-e2e";
const FIXTURE_CONTENT: &str = "content from shipped basic-tools";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn built_binary(package: &str, binary: &str) -> PathBuf {
    let root = repo_root();
    let target_dir = root.join("target/chatgpt-tools-e2e");
    let status = Command::new(env!("CARGO"))
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(["build", "--locked", "-p", package, "--bin", binary])
        .status()
        .unwrap_or_else(|error| panic!("build {package}: {error}"));
    assert!(status.success(), "{package} build failed: {status}");
    target_dir
        .join("debug")
        .join(format!("{binary}{}", std::env::consts::EXE_SUFFIX))
}

fn provider_binary() -> &'static PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| built_binary("chatgpt-provider", "chatgpt-provider"))
}

fn tool_gate_binary() -> &'static PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| built_binary("tool-gate-plugin", "tool-gate"))
}

fn basic_tools_binary() -> &'static PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| built_binary("basic-tools-plugin", "basic-tools"))
}

fn mag_binary() -> &'static PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| built_binary("mag-plugin", "mag-plugin"))
}

async fn spawn_mag(data_dir: &Path) -> Child {
    tokio::process::Command::new(mag_binary())
        .arg("--kernel")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel/init.lua"))
        .arg("--tool-gate")
        .arg("tool-gate")
        .env("NEFOR_DATA_DIR", data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn MAG")
}

async fn spawn_provider(base_url: &str, data_dir: &Path) -> Child {
    tokio::process::Command::new(provider_binary())
        .arg("--name")
        .arg(PROVIDER)
        .arg("--base-url")
        .arg(base_url)
        .env("NEFOR_DATA_DIR", data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn provider")
}

async fn spawn_tool_gate() -> Child {
    tokio::process::Command::new(tool_gate_binary())
        .arg("--prompt")
        .arg("read_file")
        .arg("--deny")
        .arg("write_file")
        .arg("--default")
        .arg("deny")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn tool gate")
}

async fn spawn_basic_tools() -> Child {
    tokio::process::Command::new(basic_tools_binary())
        .arg("--gate")
        .arg("tool-gate")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn basic-tools")
}

async fn outgoing(reader: &mut BufReader<ChildStdout>, expecting: &str) -> PluginOutgoing {
    let mut line = String::new();
    match timeout(READ_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(0)) => panic!("plugin closed while expecting {expecting}"),
        Ok(Ok(_)) => PluginOutgoing::parse_line(line.trim_end()).expect("parse output"),
        Ok(Err(error)) => panic!("read {expecting}: {error}"),
        Err(_) => panic!("timed out waiting for {expecting}"),
    }
}

async fn expect_denial_without_source_invocation(
    reader: &mut BufReader<ChildStdout>,
    blocked_id: &str,
) -> Map<String, Value> {
    loop {
        let Body::Event(body) = outgoing(reader, "correlated gate denial").await.body else {
            continue;
        };
        let kind = body.get("kind").and_then(Value::as_str);
        if kind.is_some_and(|kind| kind.ends_with(".tool.invoke")) {
            panic!("blocked invocation reached a source: {body:?}");
        }
        if kind == Some("tool.result") && body.get("id").and_then(Value::as_str) == Some(blocked_id)
        {
            return body;
        }
    }
}

async fn write_envelope(stdin: &mut ChildStdin, envelope: Envelope) {
    stdin
        .write_all(envelope.to_line().as_bytes())
        .await
        .expect("write");
    stdin.write_all(b"\n").await.expect("newline");
    stdin.flush().await.expect("flush");
}

async fn handshake(reader: &mut BufReader<ChildStdout>, stdin: &mut ChildStdin) {
    assert!(matches!(
        outgoing(reader, "ready").await.body,
        Body::System(SystemBody::Ready { .. })
    ));
    write_envelope(
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

async fn send(stdin: &mut ChildStdin, from: &str, body: Map<String, Value>) {
    let origin = if from == "engine" {
        PluginName::engine()
    } else {
        PluginName::new(from).expect("plugin name")
    };
    write_envelope(stdin, Envelope::event(origin, Timestamp::now(), body)).await;
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().expect("object").clone()
}

async fn next_kind(reader: &mut BufReader<ChildStdout>, kind: &str) -> Map<String, Value> {
    loop {
        if let Body::Event(body) = outgoing(reader, kind).await.body {
            if body.get("kind").and_then(Value::as_str) == Some(kind) {
                return body;
            }
            if matches!(
                body.get("kind").and_then(Value::as_str),
                Some("mag.error" | "mag.run_failed")
            ) {
                panic!("MAG failed while expecting {kind}: {body:?}");
            }
        }
    }
}

async fn read_http_json(stream: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).await.expect("read request");
        assert!(count > 0, "request closed before body");
        bytes.extend_from_slice(&buffer[..count]);
        let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|v| v.parse::<usize>().ok())
            })
            .unwrap_or(0);
        let start = header_end + 4;
        if bytes.len() >= start + length {
            return (
                headers.lines().next().expect("request line").to_owned(),
                if length == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[start..start + length]).expect("JSON body")
                },
            );
        }
    }
}

async fn sse(stream: &mut TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("SSE response");
}

async fn fake_responses(listener: TcpListener) {
    let mut round = 0;
    while round < 2 {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let (line, request) = read_http_json(&mut stream).await;
        if !line.starts_with("POST /responses ") {
            let body = if line.contains(" /models") {
                r#"{"data":[]}"#
            } else {
                r#"{}"#
            };
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            stream
                .write_all(response.as_bytes())
                .await
                .expect("aux response");
            continue;
        }

        assert_eq!(request["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(request["tools"][0]["name"], "read_file");
        assert_eq!(
            request["tools"][0]["description"],
            "Read the contents of a file. Returns the file's text content or an error."
        );
        assert_eq!(
            request["tools"][0]["parameters"]["properties"]["path"]["type"],
            "string"
        );
        if round == 0 {
            sse(&mut stream, "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"fixture.txt\\\"}\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"finish_reason\":\"tool_calls\"}}\n\ndata: [DONE]\n\n").await;
        } else {
            let input = request["input"].as_array().expect("round-two input");
            assert!(input
                .iter()
                .any(|item| item["type"] == "function_call_output"
                    && item["call_id"] == "call_1"
                    && item["output"] == FIXTURE_CONTENT));
            sse(&mut stream, "data: {\"type\":\"response.output_text.delta\",\"delta\":\"{\\\"content\\\":\\\"done after read_file\\\"}\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\"}}\n\ndata: [DONE]\n\n").await;
        }
        round += 1;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chatgpt_direct_allowlist_reaches_http_and_tool_result_returns_through_gate() {
    let temp = tempfile::tempdir().expect("tempdir");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("address"));
    let server = tokio::spawn(fake_responses(listener));

    let mut mag = spawn_mag(temp.path()).await;
    let mut mag_in = mag.stdin.take().expect("MAG stdin");
    let mut mag_out = BufReader::new(mag.stdout.take().expect("MAG stdout"));
    handshake(&mut mag_out, &mut mag_in).await;

    let mut provider = spawn_provider(&base_url, temp.path()).await;
    let mut provider_in = provider.stdin.take().expect("provider stdin");
    let mut provider_out = BufReader::new(provider.stdout.take().expect("provider stdout"));
    handshake(&mut provider_out, &mut provider_in).await;
    send(
        &mut provider_in,
        "engine",
        object(json!({"kind": format!("{PROVIDER}.auth.set"), "token": "test-token"})),
    )
    .await;

    let mut gate = spawn_tool_gate().await;
    let mut gate_in = gate.stdin.take().expect("gate stdin");
    let mut gate_out = BufReader::new(gate.stdout.take().expect("gate stdout"));
    handshake(&mut gate_out, &mut gate_in).await;

    let mut basic_tools = spawn_basic_tools().await;
    let mut basic_tools_in = basic_tools.stdin.take().expect("basic-tools stdin");
    let mut basic_tools_out =
        BufReader::new(basic_tools.stdout.take().expect("basic-tools stdout"));
    handshake(&mut basic_tools_out, &mut basic_tools_in).await;
    let advertisement = next_kind(&mut basic_tools_out, "tool-gate.tools.advertise").await;
    send(&mut gate_in, "basic-tools", advertisement).await;
    let register = next_kind(&mut gate_out, "tool.register").await;
    send(&mut provider_in, "tool-gate", register).await;

    // A disallowed call is rejected by the real gate before policy or source dispatch.
    send(
        &mut gate_in,
        "mag",
        object(json!({
            "kind": "tool-gate.tool.invoke", "id": "blocked-probe", "name": "write_file",
            "args": {"path": "must-not-exist.txt", "content": "blocked"},
            "allowlist": ["read_file"],
            "invocation": {"run_id": "tool-run", "node_id": "answer"}
        })),
    )
    .await;
    let denied = expect_denial_without_source_invocation(&mut gate_out, "blocked-probe").await;
    assert_eq!(denied["id"], "blocked-probe");
    assert!(denied["error"]
        .as_str()
        .is_some_and(|error| error.contains("not in this invocation's allowlist")));

    tokio::fs::write(temp.path().join("fixture.txt"), FIXTURE_CONTENT)
        .await
        .expect("fixture content");

    let source = r#"
(require "nefor.actors")
(require "nefor.artifact")
(require "nefor.contracts")
(require "nefor.graph")
(let [start (nefor.graph.source "task" (type-tag nefor.contracts.Task) (as nefor.contracts.Task {:prompt "read fixture"}))
      answer (nefor.actors.agent
               (as nefor.actors.AgentConfig {:id "answer" :model (nefor.contracts.identifier "test-model")
                :profile (nefor.contracts.no-identifier) :provider "provider" :system "Read fixture.txt, then answer."
                :tools ["read_file"] :da-policy (nefor.contracts.no-da-policy) :max-corrections 0})
               (type-tag nefor.contracts.Task) "task" (type-tag nefor.contracts.FinalAnswer))
      output (nefor.graph.output "result" (type-tag (| nefor.contracts.FinalAnswer nefor.contracts.AgentError)))
      topology (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph
                 (nefor.graph.add-edges graph [(nefor.graph.edge start answer) (nefor.graph.edge answer output)]))]
  (nefor.artifact.compile topology))
"#;
    tokio::fs::write(temp.path().join("tool.mag"), source)
        .await
        .expect("fixture");
    send(
        &mut mag_in,
        "engine",
        object(json!({
            "kind": "mag.load", "id": "load", "source_dir": temp.path(),
            "module_roots": [repo_root().join("starter/mag/lib")], "entry": "tool.mag"
        })),
    )
    .await;
    let artifact = next_kind(&mut mag_out, "mag.loaded").await["artifact"].clone();
    send(&mut mag_in, "agentic-loop", object(json!({
        "kind": "mag.execute", "id": "execute", "run_id": "tool-run", "session_id": "tool-session",
        "principal": "lead", "artifact": artifact, "params_overlay": {"answer.llm": {"provider": PROVIDER}}
    }))).await;

    for round in 0..2 {
        let request = next_kind(&mut mag_out, &format!("{PROVIDER}.completion.request")).await;
        send(&mut provider_in, "mag", request).await;
        loop {
            let Body::Event(event) = outgoing(&mut provider_out, "completion event").await.body
            else {
                continue;
            };
            if event.get("kind").and_then(Value::as_str)
                != Some(&format!("{PROVIDER}.completion.event"))
            {
                continue;
            }
            let terminal = matches!(
                event.get("event").and_then(Value::as_str),
                Some("completed" | "error")
            );
            send(&mut mag_in, PROVIDER, event).await;
            if terminal {
                break;
            }
        }
        if round == 0 {
            let invoke = next_kind(&mut mag_out, "tool-gate.tool.invoke").await;
            assert_eq!(invoke["name"], "read_file");
            assert_eq!(invoke["args"]["path"], "fixture.txt");
            assert_eq!(invoke["invocation"]["run_id"], "tool-run");
            assert_eq!(invoke["allowlist"], json!(["read_file"]));
            send(&mut gate_in, "mag", invoke).await;

            let permission = next_kind(&mut gate_out, "chat.tool.permission_request").await;
            assert_eq!(permission["tool"], "read_file");
            assert_eq!(permission["allowlist"], json!(["read_file"]));
            send(
                &mut gate_in,
                "tool-validator",
                object(json!({
                    "kind": "tool.permission_response", "id": permission["id"],
                    "decision": "approve",
                    "args": {"path": "fixture.txt", "cwd": temp.path()}
                })),
            )
            .await;

            let source_invoke = next_kind(&mut gate_out, "basic-tools.tool.invoke").await;
            assert_eq!(source_invoke["name"], "read_file");
            assert_eq!(source_invoke["args"]["path"], "fixture.txt");
            assert_eq!(source_invoke["from"], "answer.run-tool");
            assert_eq!(source_invoke["allowlist"], json!(["read_file"]));
            assert_eq!(source_invoke["invocation"]["session_id"], "tool-session");
            assert_eq!(source_invoke["invocation"]["run_id"], "tool-run");
            assert_eq!(source_invoke["invocation"]["actor_id"], "answer.run-tool");
            assert_eq!(
                source_invoke["invocation"]["capability_id"],
                source_invoke["caller_id"]
            );
            assert_eq!(source_invoke["invocation"]["principal"], "lead");
            assert!(source_invoke["invocation"]["run_scope"]
                .as_str()
                .is_some_and(|scope| scope.starts_with('r')));
            send(&mut basic_tools_in, "tool-gate", source_invoke).await;
            let source_result = next_kind(&mut basic_tools_out, "tool.result").await;
            assert_eq!(source_result["output"], FIXTURE_CONTENT);
            send(&mut gate_in, "basic-tools", source_result).await;
            let gated_result = next_kind(&mut gate_out, "tool.result").await;
            assert_eq!(gated_result["name"], "read_file");
            assert_eq!(gated_result["output"], FIXTURE_CONTENT);
            send(&mut mag_in, "tool-gate", gated_result).await;
        }
    }

    let result = next_kind(&mut mag_out, "mag.run_result").await;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["result"]["value"]["content"], "done after read_file");

    server.await.expect("fake server");
    mag.kill().await.ok();
    provider.kill().await.ok();
    gate.kill().await.ok();
    basic_tools.kill().await.ok();
}
