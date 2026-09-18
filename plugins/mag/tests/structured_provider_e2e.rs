//! Structured-output regression through the shipped MAG actor, conversation-manager boundary,
//! and real provider dispatchers.
//!
//! These tests deliberately route NCP envelopes between separate plugin processes. The MAG
//! runtime owns canonical draft validation and typed completion, while its public provider request
//! stays thin. The harness folds MAG's canonical facts into the manager-owned read context and
//! privately delivers the expanded native request to the provider process, standing in for the
//! provider compositor's in-process `engine.deliver` seam. The providers still own their HTTP
//! request controls. The local servers only inspect the resulting wire request and return a
//! deterministic draft-edit and standalone-submission tool calls.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use nefor_mag::json::concrete_type_from_json;
use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::time::timeout;

mod support;

const READ_TIMEOUT: Duration = Duration::from_secs(90);
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

#[derive(Clone, Copy)]
enum AnswerCase {
    Text,
    Record,
    Adt,
}

impl AnswerCase {
    fn semantic_value(self) -> Value {
        match self {
            Self::Text => json!("done"),
            Self::Record => json!({"value": "done"}),
            Self::Adt => json!({"constructor": "Accepted", "value": {"value": "done"}}),
        }
    }

    fn type_name(self) -> &'static str {
        match self {
            Self::Text => "nefor.contracts.TextAnswer",
            Self::Record | Self::Adt => "main.Reply",
        }
    }

    fn assert_request(self, request: &Map<String, Value>) {
        assert!(request.get("output_schema").is_none());
        let specs = request.get("tool_specs").and_then(Value::as_array);
        if matches!(self, Self::Text) {
            assert!(specs.is_none_or(Vec::is_empty));
            return;
        }
        let specs = specs.expect("typed request carries intrinsic definitions");
        for name in ["write_output", "submit_output"] {
            let spec = specs
                .iter()
                .find(|spec| spec["name"] == name)
                .expect("intrinsic spec");
            assert_eq!(spec["owner"], "mag-runtime");
            assert_eq!(spec["execution"]["kind"], "routed");
            assert_eq!(spec["parameters"]["type"], "object");
            assert!(request["tools"].as_array().unwrap().contains(&json!(name)));
        }
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn provider_binaries() -> &'static HashMap<&'static str, PathBuf> {
    static BINARIES: OnceLock<HashMap<&'static str, PathBuf>> = OnceLock::new();
    BINARIES.get_or_init(|| {
        support::require_prepared_binaries(
            "structured_provider_e2e",
            &["mag-plugin", "openai-provider", "chatgpt-provider"],
        )
    })
}

fn kernel_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel/init.lua")
}

fn starter_dir() -> PathBuf {
    repo_root().join("examples/nefor-agent")
}

async fn spawn_mag(data_dir: &Path) -> Child {
    tokio::process::Command::new(
        provider_binaries()
            .get("mag-plugin")
            .expect("mag-plugin is in the declared prerequisite set"),
    )
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

fn assert_thin_provider_request(request: &Map<String, Value>, provider: &str) {
    assert_eq!(
        request.get("kind").and_then(Value::as_str),
        Some("conversation.provider.invoke.request")
    );
    assert_eq!(
        request.get("provider").and_then(Value::as_str),
        Some(provider)
    );
    for forbidden in ["messages", "system", "conversation_context"] {
        assert!(
            request.get(forbidden).is_none(),
            "thin provider request must not carry {forbidden}: {request:?}"
        );
    }
}

async fn next_provider_request_with_facts(
    reader: &mut BufReader<ChildStdout>,
    provider: &str,
) -> (Map<String, Value>, Vec<Value>) {
    let mut facts = Vec::new();
    loop {
        let body = next_event(reader, "conversation.provider.invoke.request").await;
        match body.get("kind").and_then(Value::as_str) {
            Some("conversation.fact.append") => {
                facts.push(body.get("fact").cloned().expect("append carries fact"));
            }
            Some("conversation.provider.invoke.request") => {
                assert_thin_provider_request(&body, provider);
                return (body, facts);
            }
            Some("mag.error" | "mag.run_failed") => {
                panic!("MAG failed while expecting provider request: {body:?}");
            }
            _ => {}
        }
    }
}

fn context_messages(facts: &[Value]) -> Vec<Value> {
    struct Message {
        role: String,
        text: String,
        structured: Vec<Value>,
        completed: bool,
        tool_calls: Vec<Value>,
        tool_call_id: Option<Value>,
    }

    let mut messages = Vec::<Message>::new();
    let mut message_indexes = HashMap::<String, usize>::new();
    let mut exchange_messages = HashMap::<String, String>::new();

    for fact in facts {
        let Some(kind) = fact.get("kind").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "message_started" => {
                let id = fact["message_id"]
                    .as_str()
                    .expect("message_started carries message_id")
                    .to_owned();
                let index = messages.len();
                message_indexes.insert(id, index);
                messages.push(Message {
                    role: fact["role"]
                        .as_str()
                        .expect("message_started carries role")
                        .to_owned(),
                    text: String::new(),
                    structured: Vec::new(),
                    completed: false,
                    tool_calls: Vec::new(),
                    tool_call_id: fact.get("tool_call_id").cloned(),
                });
            }
            "tool_exchange_started" => {
                exchange_messages.insert(
                    fact["exchange_id"].as_str().unwrap().into(),
                    fact["message_id"].as_str().unwrap().into(),
                );
            }
            "tool_call_completed" => {
                let id = &exchange_messages[fact["exchange_id"].as_str().unwrap()];
                let index = message_indexes[id];
                let call = &fact["call"];
                messages[index].tool_calls.push(json!({"id": call["tool_call_id"], "type": "function", "function": {"name": call["name"], "arguments": call["arguments"].to_string()}}));
            }
            "content_chunk_appended" => {
                let Some(index) = fact["message_id"]
                    .as_str()
                    .and_then(|id| message_indexes.get(id))
                    .copied()
                else {
                    continue;
                };
                let chunk = &fact["chunk"];
                match chunk.get("kind").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(data) = chunk.get("data").and_then(Value::as_str) {
                            messages[index].text.push_str(data);
                        }
                    }
                    Some("structured") => {
                        messages[index]
                            .structured
                            .push(chunk.get("data").cloned().unwrap_or(Value::Null));
                    }
                    _ => {}
                }
            }
            "message_completed" => {
                let Some(index) = fact["message_id"]
                    .as_str()
                    .and_then(|id| message_indexes.get(id))
                    .copied()
                else {
                    continue;
                };
                messages[index].completed = true;
            }
            _ => {}
        }
    }

    messages
        .into_iter()
        .filter_map(|message| {
            if !message.completed {
                return None;
            }
            let content = if !message.text.is_empty() {
                Value::String(message.text)
            } else if message.structured.len() == 1 {
                message.structured.into_iter().next().expect("one chunk")
            } else {
                Value::Array(message.structured)
            };
            let mut projected = json!({ "role": message.role, "content": content });
            if !message.tool_calls.is_empty() {
                projected["tool_calls"] = json!(message.tool_calls);
            }
            if let Some(id) = message.tool_call_id {
                projected["tool_call_id"] = id;
            }
            Some(projected)
        })
        .collect()
}

fn private_provider_request(
    kind: ProviderKind,
    invocation: &Map<String, Value>,
    facts: &[Value],
) -> Map<String, Value> {
    let messages = context_messages(facts);
    assert!(
        messages.iter().any(|message| message["role"] == "system"),
        "canonical context contains the authored system message: {facts:?}"
    );
    assert!(
        messages.iter().any(|message| message["role"] == "user"),
        "canonical context contains the typed task: {facts:?}"
    );

    let mut request = Map::new();
    request.insert(
        "kind".into(),
        Value::String(format!("{}.completion.request", kind.name())),
    );
    for field in [
        "request_id",
        "conversation_id",
        "model",
        "reasoning_effort",
        "tools",
        "tool_specs",
        "invocation",
    ] {
        if let Some(value) = invocation.get(field) {
            request.insert(field.into(), value.clone());
        }
    }
    request.insert("messages".into(), Value::Array(messages.clone()));
    if matches!(kind, ProviderKind::ChatGpt) {
        request.insert(
            "conversation_context".into(),
            json!({
                "messages": messages,
                "tail_messages": messages,
                "history_length": messages.len(),
                "watermark": invocation.get("watermark").cloned().unwrap_or(Value::Null)
            }),
        );
    }
    request
}

fn manager_event(kind: ProviderKind, completion: Map<String, Value>) -> Map<String, Value> {
    let mut event = completion;
    event.insert(
        "kind".into(),
        Value::String("conversation.provider.event".into()),
    );
    event.insert("provider".into(), Value::String(kind.name().into()));
    event.remove("messages");
    event.remove("history");
    event.remove("conversation_context");
    event.remove("input");
    event.remove("request");
    event.remove("system");
    event.remove("tool_specs");
    event
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

async fn fake_server(kind: ProviderKind, listener: TcpListener, answer: AnswerCase) {
    let mut round = 0;
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

        assert!(request.pointer("/response_format/json_schema").is_none());
        assert!(request.pointer("/text/format/schema").is_none());
        let typed = !matches!(answer, AnswerCase::Text);
        if typed {
            let tools = request["tools"].as_array().expect("native tools");
            for name in ["write_output", "submit_output"] {
                assert!(
                    tools
                        .iter()
                        .any(|tool| tool["name"] == name || tool["function"]["name"] == name),
                    "native definition missing: {request}"
                );
            }
        }
        let (name, args) = match round {
            0 => ("write_output", json!({"new_string": "{", "validate": true})),
            1 => (
                "write_output",
                json!({"old_string": "{", "new_string": answer.semantic_value().to_string(), "validate": true}),
            ),
            _ => ("submit_output", json!({})),
        };
        let args = args.to_string();
        let event = match (kind, typed) {
            (ProviderKind::OpenAi, true) => {
                json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": format!("call-{round}"), "type": "function", "function": {"name": name, "arguments": args}}]}}]})
            }
            (ProviderKind::ChatGpt, true) => {
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "function_call", "id": format!("fc-{round}"), "call_id": format!("call-{round}"), "name": name, "arguments": args}})
            }
            (ProviderKind::OpenAi, false) => {
                json!({"choices": [{"delta": {"content": "done"}}]})
            }
            (ProviderKind::ChatGpt, false) => {
                json!({"type": "response.output_text.delta", "delta": "done"})
            }
        };
        let terminal = match kind {
            ProviderKind::OpenAi => format!("data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{}\"}}]}}\n\n", if typed { "tool_calls" } else { "stop" }),
            ProviderKind::ChatGpt => "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n".into(),
        };
        write_sse(
            &mut stream,
            &format!("data: {event}\n\n{terminal}data: [DONE]\n\n"),
        )
        .await;
        round += 1;
        if typed && round < 3 {
            continue;
        }

        break;
    }
}

async fn load_answer_program(
    reader: &mut BufReader<ChildStdout>,
    stdin: &mut ChildStdin,
    source_dir: &Path,
    answer: AnswerCase,
) -> Value {
    let source = r#"
import core.types.{}
import nefor.actors.{}
import nefor.artifact.{}
import nefor.contracts.{}
import nefor.graph.{}
type InvestigationInput {prompt: String}

let exact_model: fn(nefor.actors.ResolvedModel) -> nefor.actors.AuthoredModel = |model| => named(nefor.actors.AuthoredModel, ResolvedModel, model)
let resolved = nefor.actors.ResolvedModel {provider: "provider", model: "test-model", reasoning_effort: nefor.actors.reasoning_effort("medium")}
let start = nefor.graph.source("task", InvestigationInput {prompt: "return done"})
let answer = nefor.actors.agent<nefor.actors.ResolvedModel, InvestigationInput, nefor.contracts.TextAnswer>("answer", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: resolved, system: "Return the requested structured answer.", tools: [], tool_approval_policy: named(nefor.contracts.ToolApprovalPolicy, Default, nil)})
let output = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("result")
let topology: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| => nefor.graph.add_edges(graph, [
  nefor.graph.edge(start, answer),
  nefor.graph.edge(answer, output),
])
nefor.artifact.compile(topology)
"#;
    let source = match answer {
        AnswerCase::Text => source.to_owned(),
        AnswerCase::Record => {
            source.replace("nefor.contracts.TextAnswer", "Reply") + "\ntype Reply {value: String}\n"
        }
        AnswerCase::Adt => source.replace("nefor.contracts.TextAnswer", "Reply")
            + "\ntype Payload {value: String}\ntype Reply = Accepted(Payload) | Rejected(String)\n",
    };
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
            "module_roots": [repo_root().join("mag/lib"), starter_dir().join("mag/lib")],
            "entry": "final-answer.mag"
        })),
    )
    .await;
    next_event_of_kind(reader, "mag.loaded").await["artifact"].clone()
}

async fn run_case(kind: ProviderKind, answer: AnswerCase) {
    let temp = tempfile::tempdir().expect("tempdir");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake server");
    let base_url = format!("http://{}", listener.local_addr().expect("server address"));
    let server = tokio::spawn(fake_server(kind, listener, answer));

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

    let artifact = load_answer_program(&mut mag_out, &mut mag_in, temp.path(), answer).await;
    let constructor_id = artifact
        .pointer("/program/initial/actors")
        .and_then(Value::as_array)
        .expect("program artifact actors")
        .iter()
        .find(|actor| actor["id"] == "answer.llm")
        .and_then(|actor| actor["params"]["value"]["output_type"].as_str())
        .expect("compiler-derived output identity")
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
            "params_overlay": {"actor:10:answer.llm": {"provider": kind.name()}}
        })),
    )
    .await;

    let mut all_facts = Vec::new();
    for round in 0..if matches!(answer, AnswerCase::Text) {
        1
    } else {
        3
    } {
        let (invocation, facts) = next_provider_request_with_facts(&mut mag_out, kind.name()).await;
        answer.assert_request(&invocation);
        if !matches!(answer, AnswerCase::Text) && round > 0 {
            let paths: Vec<_> = std::fs::read_dir(temp.path().join("output-drafts"))
                .expect("draft root")
                .map(|entry| entry.unwrap().path().join("output.json"))
                .collect();
            assert_eq!(paths.len(), 1, "one private activation draft");
            let saved = std::fs::read_to_string(&paths[0]).expect("saved draft");
            assert_eq!(
                saved,
                if round == 1 {
                    "{".into()
                } else {
                    answer.semantic_value().to_string()
                },
                "round {round}: {facts:?}"
            );
        }

        if !matches!(answer, AnswerCase::Text) && round > 0 {
            let receipt = facts
                .iter()
                .filter(|fact| fact["kind"] == "tool_result_recorded")
                .filter_map(|fact| {
                    fact["result"]
                        .as_str()
                        .and_then(|text| serde_json::from_str::<Value>(text).ok())
                })
                .find(|receipt| receipt["write"] == "saved")
                .expect("saved write receipt");
            assert_eq!(
                receipt["validation"]["status"],
                if round == 1 { "invalid" } else { "valid" }
            );
            if round == 1 {
                let diagnostic = &receipt["validation"]["error"];
                assert_eq!(diagnostic["code"], "invalid_json");
                assert!(diagnostic["line"].as_u64().unwrap() > 0);
                assert!(diagnostic["column"].as_u64().unwrap() > 0);
                assert!(diagnostic["excerpt"].as_str().unwrap().contains('{'));
            }
        }

        let request_id = invocation["request_id"]
            .as_str()
            .expect("provider request id")
            .to_owned();
        all_facts.extend(facts);
        let private_request = private_provider_request(kind, &invocation, &all_facts);
        send_event(&mut provider_in, "conversation-manager", private_request).await;

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
            assert_ne!(
                body.get("event").and_then(Value::as_str),
                Some("failed"),
                "provider failure: {body:?}"
            );
            assert_ne!(
                body.get("event").and_then(Value::as_str),
                Some("error"),
                "provider failure: {body:?}"
            );
            if body.get("event").and_then(Value::as_str) == Some("text_delta") {
                assert_eq!(body.get("text"), Some(&json!("done")));
                continue;
            }
            if body.get("event").and_then(Value::as_str) == Some("completed") {
                break body;
            }
            send_event(
                &mut mag_in,
                "conversation-manager",
                manager_event(kind, body),
            )
            .await;
        };
        if matches!(answer, AnswerCase::Text) {
            assert_eq!(
                completed
                    .get("result")
                    .and_then(|result| result.get("text"))
                    .or_else(|| completed.get("text")),
                Some(&json!("done"))
            );
        }
        assert!(completed.get("chat_id").is_none());
        send_event(
            &mut mag_in,
            "conversation-manager",
            manager_event(kind, completed),
        )
        .await;
    }
    let result = next_event_of_kind(&mut mag_out, "mag.run_result").await;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["result"]["value"]["constructor"], "Ok");
    assert_eq!(result["result"]["value"]["value"], answer.semantic_value());
    let terminal = &result["result"];
    let descriptor = &terminal["semantic_type"];
    assert_eq!(descriptor["name"], "core.types.Result");
    assert_eq!(
        descriptor["arguments"][0]["name"],
        "nefor.contracts.AgentError"
    );
    assert_eq!(descriptor["arguments"][1]["name"], answer.type_name());
    let output_type = concrete_type_from_json(&descriptor["arguments"][1]).unwrap();
    assert_eq!(output_type.stable_id().as_str(), constructor_id);
    let semantic_type = concrete_type_from_json(descriptor).expect("valid Result descriptor");
    assert_eq!(
        terminal["semantic_type_id"],
        semantic_type.stable_id().as_str()
    );
    assert_eq!(
        terminal["constructor_id"],
        semantic_type
            .constructor_id("Ok")
            .expect("Ok belongs to Result")
            .as_str()
    );
    assert_ne!(terminal["semantic_type_id"], constructor_id);
    assert_ne!(terminal["constructor_id"], constructor_id);
    assert!(result["result"].get("variant").is_none());

    server.await.expect("fake server");
    mag.kill().await.ok();
    provider.kill().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_answer_through_openai_chat_completions_dispatcher() {
    run_case(ProviderKind::OpenAi, AnswerCase::Text).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_answer_through_chatgpt_responses_dispatcher() {
    run_case(ProviderKind::ChatGpt, AnswerCase::Text).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_envelope_through_openai_dispatcher_preserves_nested_value() {
    run_case(ProviderKind::OpenAi, AnswerCase::Record).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_envelope_through_chatgpt_dispatcher_preserves_nested_value() {
    run_case(ProviderKind::ChatGpt, AnswerCase::Record).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adt_envelope_through_openai_dispatcher_preserves_nominal_value() {
    run_case(ProviderKind::OpenAi, AnswerCase::Adt).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adt_envelope_through_chatgpt_dispatcher_preserves_nominal_value() {
    run_case(ProviderKind::ChatGpt, AnswerCase::Adt).await;
}
