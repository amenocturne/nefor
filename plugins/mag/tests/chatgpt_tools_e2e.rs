//! Canonical MAG → conversation-manager → tool-gate regression.
//!
//! This routes real MAG, tool-gate, and basic-tools processes over NCP. The
//! harness plays the provider behind conversation-manager's generic relay: the
//! first response calls the shipped `read_file` tool and the second returns the
//! structured final answer after observing the real file contents. Provider
//! native request/HTTP lowering is covered by the provider-owned suites.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::time::timeout;

mod support;

const READ_TIMEOUT: Duration = Duration::from_secs(90);
const PROVIDER: &str = "chatgpt-tools-e2e";
const FIXTURE_CONTENT: &str = "content from shipped basic-tools";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn prepared_binaries() -> &'static HashMap<&'static str, PathBuf> {
    static BINARIES: OnceLock<HashMap<&'static str, PathBuf>> = OnceLock::new();
    BINARIES.get_or_init(|| {
        support::require_prepared_binaries(
            "chatgpt_tools_e2e",
            &["mag-plugin", "tool-gate", "basic-tools"],
        )
    })
}

fn prepared_binary(name: &str) -> &PathBuf {
    prepared_binaries()
        .get(name)
        .unwrap_or_else(|| panic!("{name} is in the declared prerequisite set"))
}

async fn spawn_mag(data_dir: &Path) -> Child {
    tokio::process::Command::new(prepared_binary("mag-plugin"))
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

async fn spawn_tool_gate() -> Child {
    tokio::process::Command::new(prepared_binary("tool-gate"))
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
    tokio::process::Command::new(prepared_binary("basic-tools"))
        .arg("--gate")
        .arg("tool-gate")
        .arg("--read-file-max-bytes")
        .arg("32768")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chatgpt_projects_stale_allowlist_and_returns_tool_result_through_gate() {
    let temp = tempfile::tempdir().expect("tempdir");

    let mut mag = spawn_mag(temp.path()).await;
    let mut mag_in = mag.stdin.take().expect("MAG stdin");
    let mut mag_out = BufReader::new(mag.stdout.take().expect("MAG stdout"));
    handshake(&mut mag_out, &mut mag_in).await;

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
    send(&mut mag_in, "tool-gate", register.clone()).await;

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
import core.types.{}
import nefor.actors.{}
import nefor.artifact.{}
import nefor.contracts.{}
import nefor.graph.{}
type InvestigationInput {prompt: String}

let exact_model: fn(nefor.actors.ResolvedModel) -> nefor.actors.AuthoredModel = |model| => named(nefor.actors.AuthoredModel, ResolvedModel, model)
let resolved = nefor.actors.ResolvedModel {provider: "provider", model: "test-model", reasoning_effort: nefor.actors.reasoning_effort("medium")}
let start = nefor.graph.source("task", InvestigationInput {prompt: "read fixture"})
let answer = nefor.actors.agent<nefor.actors.ResolvedModel, InvestigationInput, nefor.contracts.TextAnswer>("answer", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: resolved, system: "Read fixture.txt, then answer.", tools: ["read_file", "unavailable_fixture_tool"], tool_approval_policy: named(nefor.contracts.ToolApprovalPolicy, Default, nil)})
let output = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("result")
let topology: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| => nefor.graph.add_edges(graph, [
  nefor.graph.edge(start, answer),
  nefor.graph.edge(answer, output),
])
nefor.artifact.compile(topology)
"#;
    tokio::fs::write(temp.path().join("tool.mag"), source)
        .await
        .expect("fixture");
    send(
        &mut mag_in,
        "engine",
        object(json!({
            "kind": "mag.load", "id": "load", "source_dir": temp.path(),
            "module_roots": [
                repo_root().join("mag/lib"),
                repo_root().join("examples/nefor-agent/mag/lib")
            ], "entry": "tool.mag"
        })),
    )
    .await;
    let artifact = next_kind(&mut mag_out, "mag.loaded").await["artifact"].clone();
    send(&mut mag_in, "agentic-loop", object(json!({
        "kind": "mag.execute", "id": "execute", "run_id": "tool-run", "session_id": "tool-session",
        "principal": "lead", "conversation_id": "chatgpt-tools-conversation",
        "artifact": artifact, "params_overlay": {"actor:10:answer.llm": {"provider": PROVIDER}}
    }))).await;

    let mut companion_conversation_id = None;
    for round in 0..2 {
        let request = next_kind(&mut mag_out, "conversation.provider.invoke.request").await;
        assert_eq!(request["provider"], PROVIDER);
        let request_conversation_id = request["conversation_id"]
            .as_str()
            .expect("companion LLM invocation conversation_id")
            .to_owned();
        if let Some(expected) = &companion_conversation_id {
            assert_eq!(&request_conversation_id, expected);
        } else {
            companion_conversation_id = Some(request_conversation_id.clone());
        }
        assert!(
            request.get("messages").is_none(),
            "thin invoke has no history"
        );
        assert!(
            request.get("system").is_none(),
            "thin invoke has no system prompt"
        );
        assert!(
            request.get("tool_specs").is_none(),
            "tool schemas stay provider-owned"
        );
        let request_id = request["request_id"]
            .as_str()
            .expect("provider request id")
            .to_owned();
        if round == 0 {
            send(
                &mut mag_in,
                "conversation-manager",
                object(json!({
                    "kind": "conversation.provider.event",
                    "provider": PROVIDER,
                    "request_id": request_id,
                    "event": "tool_call",
                    "id": "call_1",
                    "name": "read_file",
                    "arguments": {"path": "fixture.txt"}
                })),
            )
            .await;
            send(
                &mut mag_in,
                "conversation-manager",
                object(json!({
                    "kind": "conversation.provider.event",
                    "provider": PROVIDER,
                    "request_id": request_id,
                    "event": "completed",
                    "text": "",
                    "finish_reason": "tool_calls"
                })),
            )
            .await;
        } else {
            let answer = "done after read_file";
            send(
                &mut mag_in,
                "conversation-manager",
                object(json!({
                    "kind": "conversation.provider.event",
                    "provider": PROVIDER,
                    "request_id": request_id,
                    "event": "text_delta",
                    "text": answer
                })),
            )
            .await;
            send(
                &mut mag_in,
                "conversation-manager",
                object(json!({
                    "kind": "conversation.provider.event",
                    "provider": PROVIDER,
                    "request_id": request_id,
                    "event": "completed",
                    "text": answer,
                    "finish_reason": "stop"
                })),
            )
            .await;
        }
        if round == 0 {
            let invoke = next_kind(&mut mag_out, "tool-gate.tool.invoke").await;
            assert_eq!(invoke["name"], "read_file");
            assert_eq!(invoke["args"]["path"], "fixture.txt");
            assert_eq!(invoke["invocation"]["run_id"], "tool-run");
            assert_eq!(
                invoke["allowlist"],
                json!(["read_file", "unavailable_fixture_tool"])
            );
            let upstream_invoke_id = invoke
                .get("id")
                .and_then(Value::as_str)
                .expect("tool-gate.tool.invoke.id must be a string")
                .to_owned();
            send(&mut gate_in, "mag", invoke).await;

            let permission = next_kind(&mut gate_out, "chat.tool.permission_request").await;
            assert_eq!(permission["tool"], "read_file");
            assert_eq!(
                permission["allowlist"],
                json!(["read_file", "unavailable_fixture_tool"])
            );
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
            assert_eq!(
                source_invoke["allowlist"],
                json!(["read_file", "unavailable_fixture_tool"])
            );
            assert_eq!(source_invoke["invocation"]["session_id"], "tool-session");
            assert_eq!(source_invoke["invocation"]["run_id"], "tool-run");
            assert_eq!(source_invoke["invocation"]["actor_id"], "answer.run-tool");
            assert_eq!(
                source_invoke["invocation"]["conversation_id"],
                companion_conversation_id
                    .as_deref()
                    .expect("companion LLM conversation identity captured"),
                "tool invocation must use the exact companion LLM conversation identity"
            );
            assert_eq!(
                source_invoke["invocation"]["root_conversation_id"],
                "chatgpt-tools-conversation"
            );
            assert_eq!(source_invoke["invocation"]["provider"], "provider");
            assert_eq!(source_invoke["invocation"]["model"], "test-model");
            assert_eq!(
                source_invoke.get("caller_id").and_then(Value::as_str),
                Some(upstream_invoke_id.as_str()),
                "source caller_id must preserve the upstream tool invocation ID"
            );
            assert_eq!(
                source_invoke["invocation"]
                    .get("capability_id")
                    .and_then(Value::as_str),
                Some(upstream_invoke_id.as_str()),
                "source invocation.capability_id must preserve the upstream tool invocation ID"
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
    assert_eq!(result["result"]["value"]["value"], "done after read_file");

    mag.kill().await.ok();
    gate.kill().await.ok();
    basic_tools.kill().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_executed_lifecycle_records_facts_without_activating_run_tool() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut mag = spawn_mag(temp.path()).await;
    let mut mag_in = mag.stdin.take().expect("MAG stdin");
    let mut mag_out = BufReader::new(mag.stdout.take().expect("MAG stdout"));
    handshake(&mut mag_out, &mut mag_in).await;

    let source = r#"
import core.types.{}
import nefor.actors.{}
import nefor.artifact.{}
import nefor.contracts.{}
import nefor.graph.{}
type InvestigationInput {prompt: String}

let exact_model: fn(nefor.actors.ResolvedModel) -> nefor.actors.AuthoredModel = |model| => named(nefor.actors.AuthoredModel, ResolvedModel, model)
let resolved = nefor.actors.ResolvedModel {provider: "provider", model: "test-model", reasoning_effort: nefor.actors.reasoning_effort("medium")}
let start = nefor.graph.source("task", InvestigationInput {prompt: "research"})
let answer = nefor.actors.agent<nefor.actors.ResolvedModel, InvestigationInput, nefor.contracts.TextAnswer>("answer", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: resolved, system: "Research, then answer.", tools: ["web_search"], tool_approval_policy: named(nefor.contracts.ToolApprovalPolicy, Default, nil)})
let output = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("result")
let topology: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| => nefor.graph.add_edges(graph, [
  nefor.graph.edge(start, answer),
  nefor.graph.edge(answer, output),
])
nefor.artifact.compile(topology)
"#;
    tokio::fs::write(temp.path().join("native.mag"), source)
        .await
        .expect("fixture");
    send(
        &mut mag_in,
        "engine",
        object(json!({
            "kind": "mag.load", "id": "load-native", "source_dir": temp.path(),
            "module_roots": [
                repo_root().join("mag/lib"),
                repo_root().join("examples/nefor-agent/mag/lib")
            ], "entry": "native.mag"
        })),
    )
    .await;
    let artifact = next_kind(&mut mag_out, "mag.loaded").await["artifact"].clone();
    send(
        &mut mag_in,
        "agentic-loop",
        object(json!({
            "kind": "mag.execute", "id": "execute-native", "run_id": "native-run",
            "session_id": "native-session", "principal": "lead",
            "conversation_id": "native-conversation", "artifact": artifact,
            "params_overlay": {"actor:10:answer.llm": {"provider": PROVIDER}}
        })),
    )
    .await;

    let request = next_kind(&mut mag_out, "conversation.provider.invoke.request").await;
    let request_id = request["request_id"]
        .as_str()
        .expect("provider request id")
        .to_owned();
    for body in [
        object(json!({
            "kind": "conversation.provider.event", "provider": PROVIDER,
            "request_id": request_id, "event": "tool_execution_started",
            "tool_call_id": "web-native-1", "name": "web_search",
            "arguments": {"action": "search", "query": "rust native tools"},
            "provider_context": {"encrypted_content": "must-not-be-public"}
        })),
        object(json!({
            "kind": "conversation.provider.event", "provider": PROVIDER,
            "request_id": request_id, "event": "tool_execution_completed",
            "tool_call_id": "web-native-1", "name": "web_search",
            "arguments": {"action": "search", "query": "rust provider tools"},
            "result": {"status": "completed"},
            "extra": {"response_body": "must-not-be-public"}
        })),
        object(json!({
            "kind": "conversation.provider.event", "provider": PROVIDER,
            "request_id": request_id, "event": "completed",
            "text": "native answer", "finish_reason": "stop"
        })),
    ] {
        send(&mut mag_in, "conversation-manager", body).await;
    }

    let mut facts = Vec::new();
    let result = loop {
        let Body::Event(body) = outgoing(&mut mag_out, "native lifecycle result").await.body else {
            continue;
        };
        let kind = body.get("kind").and_then(Value::as_str);
        assert!(
            !kind.is_some_and(|value| value == "tool.invoke" || value.ends_with(".tool.invoke")),
            "provider-executed observation reached tool execution: {body:?}"
        );
        if matches!(
            kind,
            Some("mag.actor_ready" | "mag.actor_busy" | "mag.actor_idle" | "mag.firing")
        ) && body
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.contains("run-tool"))
        {
            panic!("provider-executed observation activated run-tool: {body:?}");
        }
        if kind == Some("conversation.fact.append") {
            facts.push(body["fact"].clone());
        }
        if kind == Some("mag.run_result") {
            break body;
        }
    };

    let count = |kind: &str| {
        facts
            .iter()
            .filter(|fact| fact.get("kind").and_then(Value::as_str) == Some(kind))
            .count()
    };
    assert_eq!(count("tool_exchange_started"), 1);
    assert_eq!(count("tool_call_fragment_appended"), 1);
    assert_eq!(count("tool_call_completed"), 1);
    assert_eq!(count("tool_result_recorded"), 1);
    let tool_facts: Vec<_> = facts
        .iter()
        .filter(|fact| {
            fact.get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.starts_with("tool_"))
        })
        .collect();
    let tool_wire = serde_json::to_string(&tool_facts).expect("tool facts serialize");
    assert!(tool_wire.contains("rust provider tools"));
    assert!(!tool_wire.contains("must-not-be-public"));
    assert!(!tool_wire.contains("provider_context"));
    assert!(!tool_wire.contains("response_body"));
    assert_eq!(result["status"], "completed");
    assert_eq!(result["result"]["value"]["value"], "native answer");

    mag.kill().await.ok();
}
