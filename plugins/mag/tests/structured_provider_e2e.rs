//! Structured-output regression through the shipped MAG actor and real provider dispatchers.
//!
//! These tests deliberately route NCP envelopes between separate plugin processes. The MAG
//! compiler/factory owns schema projection and result decoding; the providers own their HTTP
//! request controls. The local servers only inspect the resulting wire request and return a
//! deterministic structured response.

use std::collections::HashMap;
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

const READ_TIMEOUT: Duration = Duration::from_secs(30);
const SESSION_ID: &str = "structured-provider-e2e";

#[derive(Clone, Copy)]
enum ProviderKind {
    OpenAi,
    ChatGpt,
}

impl ProviderKind {
    fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai-e2e",
            Self::ChatGpt => "chatgpt-e2e",
        }
    }

    fn binary(self) -> &'static str {
        match self {
            Self::OpenAi => "openai-provider",
            Self::ChatGpt => "chatgpt-provider",
        }
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn mag_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mag-plugin"))
}

fn provider_binaries() -> &'static HashMap<&'static str, PathBuf> {
    static BINARIES: OnceLock<HashMap<&'static str, PathBuf>> = OnceLock::new();
    BINARIES.get_or_init(|| {
        let root = repo_root();
        let target_dir = root.join("target/structured-provider-e2e");
        let status = Command::new(env!("CARGO"))
            .current_dir(&root)
            .env("CARGO_TARGET_DIR", &target_dir)
            .args([
                "build",
                "--locked",
                "-p",
                "openai-provider",
                "-p",
                "chatgpt-provider",
            ])
            .status()
            .expect("run isolated Cargo provider build");
        assert!(status.success(), "Cargo provider build failed: {status}");

        ["openai-provider", "chatgpt-provider"]
            .into_iter()
            .map(|name| {
                let binary = target_dir
                    .join("debug")
                    .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
                assert!(
                    binary.is_file(),
                    "Cargo did not produce expected provider binary {}",
                    binary.display()
                );
                (name, binary)
            })
            .collect()
    })
}

fn kernel_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel/init.lua")
}

fn starter_dir() -> PathBuf {
    repo_root().join("starter")
}

async fn spawn_mag(data_dir: &Path) -> Child {
    tokio::process::Command::new(mag_binary())
        .arg("--kernel")
        .arg(kernel_path())
        .arg("--tool-gate")
        .arg("tool-gate")
        .env("NEFOR_DATA_DIR", data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn mag-plugin")
}

async fn spawn_provider(kind: ProviderKind, base_url: &str, data_dir: &Path) -> Child {
    let binary = provider_binaries()
        .get(kind.binary())
        .expect("Cargo-built provider binary");
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("--name")
        .arg(kind.name())
        .arg("--base-url")
        .arg(base_url)
        .env("NEFOR_DATA_DIR", data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if matches!(kind, ProviderKind::OpenAi) {
        command.arg("--model").arg("test-model");
    }
    command.spawn().expect("spawn provider")
}

async fn read_outgoing(reader: &mut BufReader<ChildStdout>, expecting: &str) -> PluginOutgoing {
    let mut line = String::new();
    match timeout(READ_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(0)) => panic!("plugin stdout closed while expecting {expecting}"),
        Ok(Ok(_)) => PluginOutgoing::parse_line(line.trim_end()).expect("parse plugin output"),
        Ok(Err(error)) => panic!("read plugin output while expecting {expecting}: {error}"),
        Err(_) => panic!("timed out waiting for {expecting}"),
    }
}

async fn handshake(reader: &mut BufReader<ChildStdout>, stdin: &mut ChildStdin) {
    let ready = read_outgoing(reader, "system ready").await;
    assert!(matches!(ready.body, Body::System(SystemBody::Ready { .. })));
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

async fn write_envelope(stdin: &mut ChildStdin, envelope: Envelope) {
    stdin
        .write_all(envelope.to_line().as_bytes())
        .await
        .expect("write envelope");
    stdin.write_all(b"\n").await.expect("write newline");
    stdin.flush().await.expect("flush envelope");
}

async fn send_event(stdin: &mut ChildStdin, from: &str, body: Map<String, Value>) {
    let origin = if from == "engine" {
        PluginName::engine()
    } else {
        PluginName::new(from).expect("valid plugin name")
    };
    write_envelope(stdin, Envelope::event(origin, Timestamp::now(), body)).await;
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().expect("JSON object").clone()
}

async fn next_event(reader: &mut BufReader<ChildStdout>, expecting: &str) -> Map<String, Value> {
    loop {
        if let Body::Event(body) = read_outgoing(reader, expecting).await.body {
            return body;
        }
    }
}

async fn next_event_of_kind(reader: &mut BufReader<ChildStdout>, kind: &str) -> Map<String, Value> {
    loop {
        let body = next_event(reader, kind).await;
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

async fn read_http_json(stream: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).await.expect("read HTTP request");
        assert!(count > 0, "HTTP request closed before its body arrived");
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
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or(0);
        let body_start = header_end + 4;
        if bytes.len() >= body_start + length {
            let request_line = headers.lines().next().expect("request line").to_owned();
            let body = if length == 0 {
                Value::Null
            } else {
                serde_json::from_slice(&bytes[body_start..body_start + length])
                    .expect("HTTP JSON body")
            };
            return (request_line, body);
        }
    }
}

async fn write_sse(stream: &mut TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("write SSE response");
}

fn assert_strict_final_answer_schema(schema: &Value) {
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["content"]["type"], "string");
    assert_eq!(schema["required"], json!(["content"]));
    assert_eq!(schema["additionalProperties"], false);
    let object = schema.as_object().expect("provider schema object");
    for descriptor in ["version", "root", "kind"] {
        assert!(
            !object.contains_key(descriptor),
            "provider schema must omit MAG descriptor field `{descriptor}`: {schema}"
        );
    }
}

async fn fake_server(kind: ProviderKind, listener: TcpListener) {
    loop {
        let (mut stream, _) = listener.accept().await.expect("accept HTTP request");
        let (request_line, request) = read_http_json(&mut stream).await;
        if matches!(kind, ProviderKind::ChatGpt) && !request_line.starts_with("POST /responses ") {
            let body = if request_line.contains(" /models") {
                r#"{"data":[]}"#
            } else if request_line.contains(" /usage") {
                r#"{}"#
            } else {
                r#"{"output":[]}"#
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("models response");
            continue;
        }

        match kind {
            ProviderKind::OpenAi => {
                assert!(request_line.contains(" /v1/chat/completions "));
                assert_eq!(request["response_format"]["type"], "json_schema");
                assert_eq!(
                    request["response_format"]["json_schema"]["name"],
                    "mag_output"
                );
                assert_eq!(request["response_format"]["json_schema"]["strict"], true);
                assert_strict_final_answer_schema(
                    &request["response_format"]["json_schema"]["schema"],
                );
                write_sse(
                    &mut stream,
                    "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"content\\\":\\\"done\\\"}\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                )
                .await;
            }
            ProviderKind::ChatGpt => {
                assert!(request_line.contains(" /responses "));
                assert_eq!(request["tools"], json!([]));
                assert_eq!(request["text"]["format"]["type"], "json_schema");
                assert_eq!(request["text"]["format"]["name"], "mag_output");
                assert_eq!(request["text"]["format"]["strict"], true);
                assert_strict_final_answer_schema(&request["text"]["format"]["schema"]);
                write_sse(
                    &mut stream,
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"{\\\"content\\\":\\\"done\\\"}\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\ndata: [DONE]\n\n",
                )
                .await;
            }
        }
        break;
    }
}

async fn load_final_answer_program(
    reader: &mut BufReader<ChildStdout>,
    stdin: &mut ChildStdin,
    source_dir: &Path,
) -> Value {
    let source = r#"
(require "nefor.actors")
(require "nefor.artifact")
(require "nefor.contracts")
(require "nefor.graph")

(let [start (nefor.graph.source "task"
              (type-tag nefor.contracts.Task)
              (as nefor.contracts.Task {:prompt "return done"}))
      answer (nefor.actors.agent
               (as nefor.actors.AgentConfig {:id "answer"
                :model (nefor.contracts.identifier "test-model")
                :profile (nefor.contracts.no-identifier)
                :provider "provider"
                :system "Return the requested structured answer."
                :tools []
                :da-policy (nefor.contracts.no-da-policy)
                :max-corrections 0})
               (type-tag nefor.contracts.Task)
               "task"
               (type-tag nefor.contracts.FinalAnswer))
      output (nefor.graph.output "result"
               (type-tag (| nefor.contracts.FinalAnswer nefor.contracts.AgentError)))
      topology (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph
                 (nefor.graph.add-edges graph
                   [(nefor.graph.edge start answer)
                    (nefor.graph.edge answer output)]))]
  (nefor.artifact.compile topology))
"#;
    tokio::fs::write(source_dir.join("final-answer.mag"), source)
        .await
        .expect("write MAG fixture");
    send_event(
        stdin,
        "engine",
        object(json!({
            "kind": "mag.load",
            "id": "structured-load",
            "source_dir": source_dir,
            "module_roots": [starter_dir().join("mag/lib")],
            "entry": "final-answer.mag"
        })),
    )
    .await;
    next_event_of_kind(reader, "mag.loaded").await["artifact"].clone()
}

async fn run_case(kind: ProviderKind) {
    let temp = tempfile::tempdir().expect("tempdir");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake server");
    let base_url = format!("http://{}", listener.local_addr().expect("server address"));
    let server = tokio::spawn(fake_server(kind, listener));

    let mut mag = spawn_mag(temp.path()).await;
    let mut mag_in = mag.stdin.take().expect("mag stdin");
    let mut mag_out = BufReader::new(mag.stdout.take().expect("mag stdout"));
    handshake(&mut mag_out, &mut mag_in).await;

    let mut provider = spawn_provider(kind, &base_url, temp.path()).await;
    let mut provider_in = provider.stdin.take().expect("provider stdin");
    let mut provider_out = BufReader::new(provider.stdout.take().expect("provider stdout"));
    handshake(&mut provider_out, &mut provider_in).await;
    if matches!(kind, ProviderKind::ChatGpt) {
        send_event(
            &mut provider_in,
            "engine",
            object(json!({"kind": format!("{}.auth.set", kind.name()), "token": "test-token"})),
        )
        .await;
    }

    let artifact = load_final_answer_program(&mut mag_out, &mut mag_in, temp.path()).await;
    let constructor_id = artifact["data"]["actors"]
        .as_array()
        .expect("artifact actors")
        .iter()
        .find(|actor| actor["foreign"] == "nefor.factory.structured-output")
        .and_then(|actor| actor["params"]["output_type"].as_str())
        .expect("compiler-derived FinalAnswer constructor identity")
        .to_owned();

    send_event(
        &mut mag_in,
        "agentic-loop",
        object(json!({
            "kind": "mag.execute",
            "id": "structured-exec",
            "run_id": "structured-run",
            "session_id": SESSION_ID,
            "principal": "lead",
            "conversation_id": "structured-provider-conversation",
            "artifact": artifact,
            "params_overlay": {"answer.llm": {"provider": kind.name()}}
        })),
    )
    .await;

    let request_kind = format!("{}.completion.request", kind.name());
    let request = next_event_of_kind(&mut mag_out, &request_kind).await;
    assert_strict_final_answer_schema(&request["output_schema"]);
    let request_id = request["request_id"]
        .as_str()
        .expect("completion request id")
        .to_owned();
    send_event(&mut provider_in, "mag", request).await;

    let completion_kind = format!("{}.completion.event", kind.name());
    let completed = loop {
        let outgoing = read_outgoing(&mut provider_out, "structured provider completion").await;
        let Body::Event(body) = outgoing.body else {
            continue;
        };
        assert_ne!(
            body.get("kind").and_then(Value::as_str),
            Some("completion.event"),
            "provider process must publish its configured canonical kind"
        );
        if body.get("kind").and_then(Value::as_str) != Some(&completion_kind) {
            continue;
        }
        assert_eq!(
            body.get("request_id").and_then(Value::as_str),
            Some(request_id.as_str())
        );
        if body.get("event").and_then(Value::as_str) == Some("text_delta") {
            panic!("structured JSON deltas must stay suppressed: {body:?}");
        }
        if body.get("event").and_then(Value::as_str) == Some("completed") {
            break body;
        }
    };
    assert_eq!(
        completed
            .get("result")
            .and_then(|result| result.get("text"))
            .or_else(|| completed.get("text")),
        Some(&json!(r#"{"content":"done"}"#))
    );
    assert!(
        completed.get("chat_id").is_none(),
        "direct completion preserves request envelope semantics"
    );
    send_event(&mut mag_in, kind.name(), completed).await;

    let result = next_event_of_kind(&mut mag_out, "mag.run_result").await;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["result"]["value"]["content"], "done");
    assert_eq!(result["result"]["semantic_type_id"], constructor_id);
    assert_eq!(result["result"]["constructor_id"], constructor_id);
    assert!(result["result"].get("variant").is_none());

    server.await.expect("fake server");
    mag.kill().await.ok();
    provider.kill().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_answer_through_openai_chat_completions_dispatcher() {
    run_case(ProviderKind::OpenAi).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_answer_through_chatgpt_responses_dispatcher() {
    run_case(ProviderKind::ChatGpt).await;
}
