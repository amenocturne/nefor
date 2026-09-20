//! Lead turn-program integration tests against the REAL shipped program
//! (`examples/nefor-agent/agentic-loop/lead-turn.mag`), driven the way the turn spawner
//! (examples/nefor-agent/agentic-loop) drives it:
//!
//!   * `mag.load` the shipped program from the starter tree, take the
//!     compiled artifact off `mag.loaded`;
//!   * per turn, clone it — point the initial `mag.Task` at the user
//!     message — and overlay `{ system, provider, model, history }` onto
//!     the lead llm actor via `params_overlay`;
//!   * `mag.execute` with the artifact inline.
//!
//! Test 1 (full turn + seeded history): a turn runs user message → kernel
//! run (scope-carrying `mag.run_started`) → gated tool round-trip
//! (correlation id scope-prefixed) → final answer inline on
//! `mag.run_result status:"completed"`. A second execute seeded with the
//! first turn's `{ user, answer }` pair replays that history to the
//! provider ahead of the new task — turn-as-function over a persistent
//! chat.
//!
//! Test 2 (interrupt = kill): `mag.kill_run` mid-provider-round reaps the
//! constellation through the fold — the dying llm's
//! `conversation.provider.cancel.request` reaches the wire —
//! and settles the pending execute as `mag.run_result status:"killed"`.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nefor_mag::json::concrete_type_from_json;
use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::time::timeout;

const PROVIDER: &str = "test-provider";
const GATE: &str = "tool-gate";
const SESSION_ID: &str = "lead-turn-session";
const CONVERSATION_ID: &str = "lead-conversation";
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// The turn spawner composes the MAG guides and references after the authored
/// system prompt, followed by the writable workspace. Here we build a representative overlay the
/// same way and assert it is recorded once as canonical conversation facts.
const LEAD_SYSTEM: &str = "you are the lead\n\n\
# MAG in Five Minutes\n\ncore guide\n\n---\n\n\
# Nefor MAG in Five Minutes\n\nnefor guide\n\n---\n\n\
# MAG references\n\nFull MAG Book: `/runtime/mag/book/README.md`\n\n---\n\n\
# MAG workspace\n\nWritable source directory: `/tmp/nefor/sessions/lead-turn-session/mag`\n";

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mag-plugin"))
}

fn starter_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/nefor-agent")
}

fn module_roots() -> [PathBuf; 2] {
    [
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mag/lib"),
        starter_dir().join("mag/lib"),
    ]
}

fn kernel_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel/init.lua")
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("create copied module directory");
    for entry in std::fs::read_dir(from).expect("read module directory") {
        let entry = entry.expect("read module entry");
        let destination = to.join(entry.file_name());
        if entry.file_type().expect("read module entry type").is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).expect("copy module file");
        }
    }
}

fn assert_typed_result(result: &Map<String, Value>) {
    let terminal = result.get("result").expect("terminal result");
    let descriptor = terminal
        .get("semantic_type")
        .expect("terminal result carries semantic type descriptor");
    let semantic_type = concrete_type_from_json(descriptor).expect("valid terminal descriptor");
    assert_eq!(descriptor["name"], "core.types.Result", "{result:?}");
    assert_eq!(
        descriptor["arguments"][0]["name"], "nefor.contracts.AgentError",
        "{result:?}"
    );
    assert_eq!(
        terminal["semantic_type_id"],
        semantic_type.stable_id().as_str(),
        "{result:?}"
    );
    let selected = terminal["value"]["constructor"]
        .as_str()
        .expect("terminal result selects a constructor");
    assert_eq!(
        terminal["constructor_id"],
        semantic_type
            .constructor_id(selected)
            .expect("selected constructor belongs to Result")
            .as_str(),
        "{result:?}"
    );
    assert!(terminal.get("variant").is_none(), "{result:?}");
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
    let mut child = cmd.spawn().expect("spawn mag-plugin");
    let stderr = child.stderr.take().expect("stderr");
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[mag stderr] {line}");
        }
    });
    child
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

async fn next_event<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    expecting: &str,
) -> Map<String, Value> {
    loop {
        let out = read_outgoing(reader, expecting).await;
        if let Body::Event(map) = out.body {
            return map;
        }
    }
}

/// Read events until one of kind `kind` arrives; returns it. Other events
/// (lifecycle noise) are skipped.
async fn next_event_of_kind<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    kind: &str,
) -> Map<String, Value> {
    loop {
        let body = next_event(reader, kind).await;
        if body.get("kind").and_then(Value::as_str) == Some(kind) {
            return body;
        }
        if matches!(
            body.get("kind").and_then(Value::as_str),
            Some("mag.error" | "mag.run_failed")
        ) {
            panic!("MAG failure while expecting {kind}: {body:?}");
        }
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
    let kind = body.get("kind").and_then(Value::as_str);
    let source = if kind == Some("mag.execute") {
        PluginName::new("agentic-loop").expect("agentic-loop plugin name")
    } else if kind == Some("conversation.provider.event") {
        PluginName::new("conversation-manager").expect("conversation-manager plugin name")
    } else {
        PluginName::engine()
    };
    write_env(stdin, Envelope::event(source, Timestamp::now(), body)).await;
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

async fn shutdown(mut stdin: ChildStdin, mut child: Child) {
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

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().expect("object").clone()
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
    for forbidden in ["messages", "system", "tool_specs"] {
        assert!(
            request.get(forbidden).is_none(),
            "thin provider request must not carry {forbidden}: {request:?}"
        );
    }
}

async fn next_provider_request_with_facts<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
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
                panic!("MAG failure while expecting provider request: {body:?}");
            }
            Some("mag.run_result")
                if body.get("status").and_then(Value::as_str) != Some("completed") =>
            {
                panic!("MAG run settled while expecting provider request: {body:?}");
            }
            _ => {}
        }
    }
}

async fn next_provider_request<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    provider: &str,
) -> Map<String, Value> {
    next_provider_request_with_facts(reader, provider).await.0
}

fn facts_json(facts: &[Value]) -> String {
    serde_json::to_string(facts).expect("serialize canonical facts")
}

/// Load the shipped lead turn-program and return its compiled artifact.
async fn load_lead_program<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    stdin: &mut ChildStdin,
) -> Value {
    send_event(
        stdin,
        obj(json!({
            "kind": "mag.load",
            "id": "lead-turn-load",
            "source_dir": starter_dir().to_string_lossy(),
            "module_roots": module_roots(),
            "entry": "agentic-loop/lead-turn.mag",
        })),
    )
    .await;
    let loaded = next_event_of_kind(reader, "mag.loaded").await;
    assert_eq!(
        loaded.get("in_reply_to").and_then(Value::as_str),
        Some("lead-turn-load")
    );
    let artifact = loaded
        .get("artifact")
        .cloned()
        .expect("mag.loaded carries the compiled artifact");
    assert_eq!(artifact.get("format"), Some(&json!("nefor.mag")));
    assert_eq!(artifact.get("version"), Some(&json!(4)));
    assert_eq!(artifact.get("kind"), Some(&json!("program")));
    assert!(artifact.pointer("/program/initial").is_some());
    assert!(artifact.pointer("/program/operations").is_some());
    assert!(artifact.get("program_id").is_none());
    artifact
}

fn program_initial(artifact: &Value) -> &Value {
    artifact
        .pointer("/program/initial")
        .expect("nefor.mag v3 program.initial")
}

fn program_initial_mut(artifact: &mut Value) -> &mut Value {
    artifact
        .pointer_mut("/program/initial")
        .expect("nefor.mag v3 program.initial")
}

/// The spawner's per-turn clone: point the initial task at the user
/// message.
fn turn_artifact(program: &Value, user_text: &str) -> Value {
    let mut m = program.clone();
    let actors = program_initial_mut(&mut m)
        .get_mut("actors")
        .and_then(Value::as_array_mut)
        .expect("program has actors");
    for actor in actors {
        if actor.get("factory").and_then(Value::as_str) == Some("nefor.factory.source") {
            let value = actor
                .pointer_mut("/params/value/value")
                .and_then(Value::as_object_mut)
                .expect("source actor carries the packed typed task value");
            value.insert("prompt".to_owned(), Value::String(user_text.to_owned()));
        }
    }
    m
}

/// The spawner's per-turn execute: artifact inline + the config/history
/// overlay on the lead llm actor.
fn execute_body(
    exec_id: &str,
    run_id: &str,
    artifact: Value,
    history: Value,
) -> Map<String, Value> {
    obj(json!({
        "kind": "mag.execute",
        "id": exec_id,
        "run_id": run_id,
        "run_name": "lead",
        "session_id": SESSION_ID,
        "principal": "lead",
        "conversation_id": CONVERSATION_ID,
        "artifact": artifact,
        "params_overlay": {
            "actor:8:lead.llm": {
                "system": LEAD_SYSTEM,
                "provider": PROVIDER,
                "model": "test-model",
                "reasoning_effort": "high",
                "history": history,
                "conversation_id": CONVERSATION_ID,
            }
        }
    }))
}

fn assert_duration_ms(result: &Map<String, Value>) -> u64 {
    result
        .get("duration_ms")
        .and_then(Value::as_u64)
        .expect("terminal run result carries a nonnegative integer duration_ms")
}

fn completed(provider: &str, request_id: &str, fields: Value) -> Map<String, Value> {
    let mut body = fields.as_object().expect("completion fields").clone();
    body.insert(
        "kind".into(),
        Value::String("conversation.provider.event".into()),
    );
    body.insert("provider".into(), Value::String(provider.to_owned()));
    body.insert("request_id".into(), Value::String(request_id.to_owned()));
    body.insert("event".into(), Value::String("completed".into()));
    body
}

async fn load_typed_task_program<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    stdin: &mut ChildStdin,
) -> Value {
    send_event(
        stdin,
        obj(json!({
            "kind": "mag.load",
            "id": "typed-task-load",
            "source_dir": starter_dir().to_string_lossy(),
            "module_roots": module_roots(),
            "entry": "agentic-loop/typed-task.mag",
        })),
    )
    .await;
    let loaded = next_event_of_kind(reader, "mag.loaded").await;
    loaded
        .get("artifact")
        .cloned()
        .expect("typed task artifact")
}

#[tokio::test]
async fn typed_task_contract_lowers_and_corrects_mock_provider_json() {
    const MOCK: &str = "mock-provider";
    let data_dir = std::env::temp_dir().join(format!("mag-typed-task-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).expect("mkdir data dir");
    let mut child = spawn_mag(&data_dir).await;
    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    handshake(&mut reader, &mut stdin).await;

    let artifact = load_typed_task_program(&mut reader, &mut stdin).await;
    let actors = program_initial(&artifact)
        .pointer("/actors")
        .and_then(Value::as_array)
        .unwrap();
    let structured = actors
        .iter()
        .find(|actor| actor.get("id").and_then(Value::as_str) == Some("typed-task.llm"))
        .expect("structured actor lowered");
    assert_eq!(
        structured.get("factory").and_then(Value::as_str),
        Some("nefor.factory.structured-output")
    );
    assert_eq!(
        structured.pointer("/params/value/schema/version"),
        Some(&json!(2))
    );
    assert!(
        structured.get("routes").is_none(),
        "artifact-v4 actors do not own routes"
    );

    send_event(
        &mut stdin,
        obj(json!({
            "kind": "mag.execute",
            "id": "typed-schema-tamper",
            "run_id": "typed-schema-tamper-run",
            "session_id": SESSION_ID,
            "principal": "lead",
            "conversation_id": CONVERSATION_ID,
            "artifact": artifact.clone(),
            "params_overlay": {
                "actor:14:typed-task.llm": {
                    "schema": {"version": 1, "root": {"kind": "data"}}
                }
            }
        })),
    )
    .await;
    let rejected = next_event_of_kind(&mut reader, "mag.error").await;
    assert_eq!(
        rejected.get("in_reply_to").and_then(Value::as_str),
        Some("typed-schema-tamper")
    );
    assert!(rejected
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("protected compiler-derived param \"schema\"")));

    send_event(
        &mut stdin,
        obj(json!({
            "kind": "mag.execute",
            "id": "typed-exec",
            "run_id": "typed-run",
            "run_name": "typed-task",
            "session_id": SESSION_ID,
            "principal": "lead",
            "conversation_id": CONVERSATION_ID,
            "artifact": artifact,
            "params_overlay": {
                "actor:14:typed-task.llm": { "provider": MOCK, "model": "mock-model" }
            }
        })),
    )
    .await;

    let create = next_provider_request(&mut reader, MOCK).await;
    let first_chat = create["request_id"].as_str().unwrap().to_owned();
    assert_eq!(create.pointer_str("/output_schema/type"), Some("object"));
    assert_eq!(
        create.pointer_str("/output_schema/properties/value/properties/task/type"),
        Some("string")
    );
    assert_eq!(
        create
            .get("output_schema")
            .and_then(|schema| schema.pointer("/properties/value/additionalProperties")),
        Some(&Value::Bool(false))
    );
    send_event(
        &mut stdin,
        completed(MOCK, &first_chat, json!({ "text": "```json\n{}\n```" })),
    )
    .await;

    let (create2, correction_facts) = next_provider_request_with_facts(&mut reader, MOCK).await;
    let second_chat = create2
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap()
        .to_owned();
    let correction_history = facts_json(&correction_facts);
    assert!(
        correction_history.contains("malformed_json")
            || correction_history.contains("invalid_json"),
        "correction diagnostics are retained as canonical facts: {correction_facts:?}"
    );
    send_event(
        &mut stdin,
        completed(MOCK, &second_chat, json!({ "text": "{\"value\":{\"task\":\"build\",\"description\":\"Implement it\",\"dependent_tasks\":[]}}" })),
    )
    .await;
    let result = next_event_of_kind(&mut reader, "mag.run_result").await;
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("completed")
    );
    assert_typed_result(&result);
    assert_eq!(
        result.pointer_str("/result/value/value/task"),
        Some("build")
    );
    shutdown(stdin, child).await;
}

#[tokio::test]
async fn whole_agent_error_union_can_drive_a_recovery_agent() {
    const MOCK: &str = "mock-provider";
    let data_dir = std::env::temp_dir().join(format!("mag-recovery-chain-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).expect("mkdir data dir");
    let mut child = spawn_mag(&data_dir).await;
    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    handshake(&mut reader, &mut stdin).await;

    send_event(
        &mut stdin,
        obj(json!({
            "kind": "mag.load",
            "id": "recovery-chain-load",
            "source_dir": starter_dir().to_string_lossy(),
            "module_roots": module_roots(),
            "entry": "agentic-loop/recovery-chain.mag",
        })),
    )
    .await;
    let loaded = next_event_of_kind(&mut reader, "mag.loaded").await;
    let artifact = loaded.get("artifact").cloned().expect("recovery artifact");
    send_event(
        &mut stdin,
        obj(json!({
            "kind": "mag.execute",
            "id": "recovery-chain-exec",
            "run_id": "recovery-chain-run",
            "session_id": SESSION_ID,
            "principal": "lead",
            "conversation_id": CONVERSATION_ID,
            "artifact": artifact,
        })),
    )
    .await;

    let builder_create = next_provider_request(&mut reader, MOCK).await;
    let builder_id = builder_create["request_id"].as_str().unwrap().to_owned();
    send_event(
        &mut stdin,
        completed(MOCK, &builder_id, json!({"text": "partial builder notes"})),
    )
    .await;

    let (reviewer_create, reviewer_facts) =
        next_provider_request_with_facts(&mut reader, MOCK).await;
    let reviewer_id = reviewer_create["request_id"].as_str().unwrap().to_owned();
    let reviewer_history = facts_json(&reviewer_facts);
    assert!(
        reviewer_history.contains("partial builder notes"),
        "builder output was not recorded before the reviewer invocation"
    );
    send_event(
        &mut stdin,
        completed(
            MOCK,
            &reviewer_id,
            json!({"text": "{\"value\":{\"assessment\":\"continue from partial work\"}}"}),
        ),
    )
    .await;
    let result = next_event_of_kind(&mut reader, "mag.run_result").await;
    assert_typed_result(&result);
    assert_eq!(
        result.pointer_str("/result/value/value/assessment"),
        Some("continue from partial work")
    );
    shutdown(stdin, child).await;
}

async fn complete_chat<R: AsyncBufReadExt + Unpin>(
    _reader: &mut R,
    stdin: &mut ChildStdin,
    request_id: &str,
    text: &str,
) {
    let text = serde_json::from_str::<Value>(text)
        .ok()
        .map(|value| {
            if value.get("value").is_some() {
                value
            } else {
                json!({"value": value})
            }
        })
        .map_or_else(|| text.to_owned(), |value| value.to_string());
    send_event(
        stdin,
        completed("mock-provider", request_id, json!({"text": text})),
    )
    .await;
}

fn dynamic_behavior_fixture() -> Value {
    serde_json::from_str(include_str!(
        "fixtures/dynamic-operations-current-behavior.json"
    ))
    .expect("dynamic operations behavior fixture is valid JSON")
}

fn assert_dynamic_program_envelope(artifact: &Value) {
    assert_eq!(artifact["format"], "nefor.mag");
    assert_eq!(artifact["version"], 4);
    assert_eq!(artifact["kind"], "program");
    let program = artifact["program"].as_object().expect("program payload");
    let operations = program["operations"]
        .as_array()
        .expect("ordered operations");
    assert_eq!(operations.len(), 1);
    let operation = &operations[0];
    assert_eq!(operation["id"], "expand.expand");
    assert_eq!(operation["on"]["endpoint"]["value"]["id"], "expand.input");
    assert_eq!(operation["on"]["endpoint"]["constructor"], "ActorEndpoint");
    assert_eq!(operation["on"]["wire"], "nefor.dynamic.Indexed");
    assert!(operation["on"]["type_id"]
        .as_str()
        .is_some_and(|value| value.starts_with("sha256:")));
    let expressions = operation["expressions"]
        .as_array()
        .expect("flat expressions");
    let shapes = expressions
        .iter()
        .filter_map(|expression| expression.get("constructor").and_then(Value::as_str))
        .map(|constructor| match constructor {
            "Trigger" => "trigger",
            "Capture" => "capture",
            "Field" => "field",
            "IntToDecimalString" => "int_to_decimal_string",
            "ConcatStrings" => "concat",
            other => panic!("unknown dynamic expression constructor {other}"),
        })
        .collect::<Vec<_>>();
    for required in [
        "trigger",
        "capture",
        "field",
        "int_to_decimal_string",
        "concat",
    ] {
        assert!(
            shapes.contains(&required),
            "missing {required} expression: {shapes:?}"
        );
    }
    let payloads = expressions
        .iter()
        .map(|expression| expression.get("value").expect("expression payload"))
        .collect::<Vec<_>>();
    let ids = payloads
        .iter()
        .map(|expression| expression["id"].as_str().expect("expression id"))
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), expressions.len());
    assert!(ids.iter().all(|id| !id.is_empty()));
    let unique_ids = ids
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(unique_ids.len(), ids.len(), "expression ids are unique");
    for (index, expression) in payloads.iter().enumerate() {
        let references = expression
            .get("record")
            .and_then(Value::as_str)
            .into_iter()
            .chain(expression.get("value").and_then(Value::as_str))
            .chain(
                expression
                    .get("values")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            );
        for reference in references {
            let position = ids
                .iter()
                .position(|candidate| *candidate == reference)
                .expect("expression reference resolves");
            assert!(position < index, "expressions are topologically ordered");
        }
    }
    let template_actors = operation["template"]["actors"]
        .as_array()
        .expect("template actors");
    let slots = template_actors
        .iter()
        .map(|actor| actor["slot"].as_str().expect("actor slot"))
        .collect::<Vec<_>>();
    assert_eq!(
        slots,
        [
            "worker.entry",
            "worker.llm",
            "worker.run-tool",
            "worker.tool-result",
            "traverse.result",
        ]
    );
    let llm = template_actors
        .iter()
        .find(|actor| actor["slot"] == "worker.llm")
        .expect("llm template actor");
    let run_tool = template_actors
        .iter()
        .find(|actor| actor["slot"] == "worker.run-tool")
        .expect("run-tool template actor");
    assert_eq!(
        run_tool.pointer("/params/value/conversation_peer"),
        Some(&json!("worker.llm")),
        "the authored parameter names the original local actor slot"
    );
    assert_eq!(run_tool["parameter_bindings"], json!([]));
    assert_ne!(run_tool["id"], llm["id"]);
    let template_routes = operation["template"]["routes"]
        .as_array()
        .expect("template routes");
    assert_eq!(template_routes.len(), 6);
    assert!(template_routes.iter().all(|route| {
        route.pointer("/from/endpoint/constructor").is_some()
            && route.pointer("/to/endpoint/constructor").is_some()
            && route.get("transforms").and_then(Value::as_array).is_some()
    }));
    let template_messages = operation["template"]["messages"]
        .as_array()
        .expect("template messages");
    assert_eq!(template_messages.len(), 1);
    let input_message = &template_messages[0];
    assert_eq!(
        input_message.pointer("/to/endpoint/value/slot"),
        Some(&json!("worker.entry"))
    );
    assert_eq!(input_message["content"]["constructor"], "Expression");
    let content_expression = input_message["content"]["value"]
        .as_str()
        .expect("message expression id");
    let payload_expression = expressions
        .iter()
        .find(|expression| expression["value"]["id"] == content_expression)
        .expect("message references a declared expression");
    assert_eq!(payload_expression["constructor"], "Field");
    assert_eq!(payload_expression["value"]["field"], "value");
    assert!(operation.get("fn").is_none());
    assert!(operation.get("source").is_none());
    assert!(operation.get("bytecode").is_none());
}

async fn load_dynamic_program<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    stdin: &mut ChildStdin,
    load_id: &str,
) -> Map<String, Value> {
    send_event(
        stdin,
        obj(json!({
            "kind": "mag.load",
            "id": load_id,
            "source_dir": starter_dir().to_string_lossy(),
            "module_roots": module_roots(),
            "entry": "agentic-loop/dynamic-tasks.mag",
        })),
    )
    .await;
    let loaded = next_event_of_kind(reader, "mag.loaded").await;
    assert_eq!(loaded["in_reply_to"], load_id);
    assert_dynamic_program_envelope(&loaded["artifact"]);
    assert!(
        loaded["hash"]
            .as_str()
            .is_some_and(|hash| hash.starts_with("sha256:")),
        "compiled artifacts carry a content identity: {loaded:?}"
    );
    loaded
}

fn request_actor(request: &Map<String, Value>) -> &str {
    request
        .pointer_str("/invocation/actor_id")
        .expect("provider request carries the authoritative actor id")
}

fn assert_occurrence_actor(actor_id: &str, original_actor: &str, index: usize) {
    assert!(
        actor_id.starts_with("traverse:6:expand"),
        "runtime id is traversal-qualified: {actor_id}"
    );
    assert!(
        actor_id.contains(original_actor),
        "runtime id preserves original actor identity {original_actor}: {actor_id}"
    );
    assert!(
        actor_id.ends_with(&format!(":{index}")),
        "runtime id preserves occurrence index {index}: {actor_id}"
    );
}

fn structured_input<'a>(facts: &'a [Value], actor_id: &str) -> &'a Value {
    facts
        .iter()
        .find_map(|fact| {
            (fact.get("actor_id").and_then(Value::as_str) == Some(actor_id)
                && fact.get("kind").and_then(Value::as_str) == Some("content_chunk_appended")
                && fact.pointer("/chunk/kind") == Some(&json!("structured")))
            .then(|| {
                let data = fact.pointer("/chunk/data")?;
                Some(data.get("value").unwrap_or(data))
            })
            .flatten()
        })
        .expect("actor receives one structured input record")
}

async fn next_provider_request_recording<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    provider: &str,
    trace: &mut Vec<String>,
    events: &mut Vec<Value>,
) -> (Map<String, Value>, Vec<Value>) {
    let mut facts = Vec::new();
    loop {
        let body = next_event(reader, "conversation.provider.invoke.request").await;
        events.push(Value::Object(body.clone()));
        let kind = body
            .get("kind")
            .and_then(Value::as_str)
            .expect("event kind")
            .to_owned();
        trace.push(kind.clone());
        match kind.as_str() {
            "conversation.fact.append" => {
                facts.push(body.get("fact").cloned().expect("append carries fact"));
            }
            "conversation.provider.invoke.request" => {
                assert_thin_provider_request(&body, provider);
                return (body, facts);
            }
            "mag.error" | "mag.run_failed" => {
                panic!("MAG failure while expecting provider request: {body:?}");
            }
            "mag.run_result" if body.get("status").and_then(Value::as_str) != Some("completed") => {
                panic!("MAG run settled while expecting provider request: {body:?}");
            }
            _ => {}
        }
    }
}

async fn next_event_of_kind_recording<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    kind: &str,
    trace: &mut Vec<String>,
    events: &mut Vec<Value>,
) -> Map<String, Value> {
    loop {
        let body = next_event(reader, kind).await;
        events.push(Value::Object(body.clone()));
        let actual = body
            .get("kind")
            .and_then(Value::as_str)
            .expect("event kind")
            .to_owned();
        trace.push(actual.clone());
        if actual == kind {
            return body;
        }
        if matches!(actual.as_str(), "mag.error" | "mag.run_failed") {
            panic!("MAG failure while expecting {kind}: {body:?}");
        }
    }
}

fn assert_materialized_item(events: &[Value], expected: &Value, index: usize) {
    let spawned = events
        .iter()
        .filter(|event| event["kind"] == "mag.actor_spawned")
        .collect::<Vec<_>>();
    let actor_for_slot = |slot: &str| {
        spawned
            .iter()
            .copied()
            .find(|event| {
                event["id"].as_str().is_some_and(|id| {
                    id.starts_with("traverse:6:expand")
                        && id.contains(slot)
                        && id.ends_with(&format!(":{index}"))
                })
            })
            .unwrap_or_else(|| panic!("missing occurrence {index} actor for slot {slot}"))
    };

    for actor in expected["actors"].as_array().expect("fixture actors") {
        let slot = actor["slot"].as_str().expect("fixture actor slot");
        let materialized = actor_for_slot(slot);
        assert_occurrence_actor(materialized["id"].as_str().unwrap(), slot, index);
        assert_eq!(materialized["factory"], actor["factory"]);
        assert_eq!(materialized["spec"]["params"]["system"], actor["system"]);
    }

    let llm = actor_for_slot("worker.llm");
    let run_tool = actor_for_slot("worker.run-tool");
    assert_eq!(
        run_tool["spec"]["params"]["conversation_peer"], llm["id"],
        "conversation_peer is relocated to the occurrence's opaque llm id"
    );
    let result = actor_for_slot("traverse.result");
    assert_eq!(result["spec"]["params"]["index"], index);
    assert!(result["spec"]["params"]["collection"]
        .as_str()
        .is_some_and(|collection| !collection.is_empty()));

    let actors_with_routes = spawned
        .iter()
        .filter(|event| event["spec"].get("routes").is_some())
        .map(|event| json!({"id": event["id"], "routes": event["spec"]["routes"]}))
        .collect::<Vec<_>>();
    assert!(
        actors_with_routes.is_empty(),
        "artifact-v3 actors do not own routes: {actors_with_routes:?}"
    );

    let entry = actor_for_slot("worker.entry");
    let message = &expected["message"];
    assert!(events
        .iter()
        .any(|event| { event["kind"] == "mag.firing" && event["id"] == entry["id"] }));
    assert_eq!(entry["spec"]["input"]["wire"], message["kind"]);

    let occurrence_ids = expected["actors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|actor| actor_for_slot(actor["slot"].as_str().unwrap())["id"].clone())
        .collect::<std::collections::HashSet<_>>();
    let node_paths = events
        .iter()
        .filter(|event| event["kind"] == "mag.nodes_declared")
        .flat_map(|event| event["nodes"].as_array().into_iter().flatten())
        .filter(|node| {
            node["members"]
                .as_array()
                .is_some_and(|members| members.iter().any(|id| occurrence_ids.contains(id)))
        })
        .map(|node| node["path"].clone())
        .collect::<Vec<_>>();
    assert_eq!(node_paths.len(), occurrence_ids.len());
    let trigger_path = events
        .iter()
        .filter(|event| event["kind"] == "mag.nodes_declared")
        .flat_map(|event| event["nodes"].as_array().into_iter().flatten())
        .find(|node| {
            node["members"]
                .as_array()
                .is_some_and(|members| members.contains(&json!("expand.input")))
        })
        .and_then(|node| node["path"].as_array())
        .expect("traversal trigger has a declared logical owner");
    assert!(
        node_paths.iter().all(|path| {
            path.as_array().is_some_and(|segments| {
                segments.starts_with(trigger_path)
                    && segments
                        .get(trigger_path.len())
                        .and_then(Value::as_str)
                        .is_some_and(|segment| segment.ends_with(&format!(":{index}")))
            })
        }),
        "occurrence nodes use traversal-qualified logical paths: {node_paths:?}"
    );
}

#[tokio::test]
async fn dynamic_tasks_real_agents_complete_out_of_order_and_preserve_planner_order() {
    let data_dir = std::env::temp_dir().join(format!("mag-dynamic-tasks-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut child = spawn_mag(&data_dir).await;
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    handshake(&mut reader, &mut stdin).await;
    let assert_snapshot = |request: &Map<String, Value>| {
        assert_eq!(
            request.get("model").and_then(Value::as_str),
            Some("snapshot-dynamic")
        );
        assert_eq!(
            request.get("reasoning_effort").and_then(Value::as_str),
            Some("high")
        );
    };
    let loaded = load_dynamic_program(&mut reader, &mut stdin, "dynamic-load").await;
    let artifact = &loaded["artifact"];
    let initial = artifact
        .pointer("/program/initial")
        .expect("program envelope initial");
    assert_eq!(
        initial
            .pointer("/messages/0/to/endpoint/value/id")
            .and_then(Value::as_str),
        Some("task")
    );
    assert_eq!(
        initial
            .pointer("/messages/0/content/value/kind")
            .and_then(Value::as_str),
        Some("mag.Unit")
    );
    let fixture = dynamic_behavior_fixture();
    let mut lifecycle_trace = Vec::new();
    let mut observed_events = Vec::new();
    send_event(
        &mut stdin,
        obj(json!({"kind":"mag.execute","id":"dynamic-exec",
      "run_id":"dynamic-run","run_name":"dynamic-tasks","session_id":SESSION_ID,
      "principal":"lead","conversation_id":CONVERSATION_ID,"artifact":loaded["artifact"].clone(),
      "model_snapshot":{"provider":"mock-provider","model":"snapshot-dynamic","reasoning_effort":"high"}})),
    )
    .await;

    let (planner, _) = next_provider_request_recording(
        &mut reader,
        "mock-provider",
        &mut lifecycle_trace,
        &mut observed_events,
    )
    .await;
    assert_eq!(request_actor(&planner), "planner.llm");
    assert_snapshot(&planner);
    let planner_id = planner["request_id"].as_str().unwrap().to_owned();
    complete_chat(&mut reader, &mut stdin, &planner_id,
      r#"{"value":[{"task":"same","description":"repeated","dependent_tasks":[]},{"task":"same","description":"repeated","dependent_tasks":[] }]}"#).await;

    let (first, first_facts) = next_provider_request_recording(
        &mut reader,
        "mock-provider",
        &mut lifecycle_trace,
        &mut observed_events,
    )
    .await;
    let (second, second_facts) = next_provider_request_recording(
        &mut reader,
        "mock-provider",
        &mut lifecycle_trace,
        &mut observed_events,
    )
    .await;
    let first_actor = request_actor(&first);
    let second_actor = request_actor(&second);
    assert_occurrence_actor(first_actor, "worker.llm", 0);
    assert_occurrence_actor(second_actor, "worker.llm", 1);
    assert_ne!(
        first_actor, second_actor,
        "equal values retain occurrence identity"
    );
    let expected_system = fixture["scenarios"]["multiple"]["worker_system"]
        .as_str()
        .expect("worker system fixture");
    for (facts, actor, index) in [
        (&first_facts, first_actor, 0usize),
        (&second_facts, second_actor, 1usize),
    ] {
        assert!(
            facts_json(facts).contains(expected_system),
            "each occurrence keeps the ordinary agent's authored system prompt: {facts:?}"
        );
        assert_eq!(
            structured_input(facts, actor),
            &json!({"task":"same","description":"repeated","dependent_tasks":[]}),
            "occurrence {index} receives the intact Task record"
        );
    }
    assert_snapshot(&first);
    assert_snapshot(&second);
    let first_id = first["request_id"].as_str().unwrap().to_owned();
    let second_id = second["request_id"].as_str().unwrap().to_owned();
    assert_ne!(
        first_id, second_id,
        "parallel workers have distinct request ids"
    );
    // Equal planner values retain occurrence identity; completion order is two,one.
    assert_eq!(
        json!([1, 0]),
        fixture["scenarios"]["multiple"]["completion_order"]
    );
    send_event(
        &mut stdin,
        completed(
            "mock-provider",
            &second_id,
            json!({"text":r#"{"value":{"task":"same","description":"done second"}}"#}),
        ),
    )
    .await;
    send_event(
        &mut stdin,
        completed(
            "mock-provider",
            &first_id,
            json!({"text":r#"{"value":{"task":"same","description":"done first"}}"#}),
        ),
    )
    .await;

    let (summary_create, summary_facts) = next_provider_request_recording(
        &mut reader,
        "mock-provider",
        &mut lifecycle_trace,
        &mut observed_events,
    )
    .await;
    assert_eq!(request_actor(&summary_create), "summarizer.llm");
    assert_snapshot(&summary_create);
    assert_eq!(
        summary_create.pointer_str("/output_schema/type"),
        Some("object")
    );
    assert_eq!(
        summary_create.pointer_str("/output_schema/properties/value/title"),
        Some("main.Summary")
    );
    assert_eq!(
        summary_create.pointer_str("/output_schema/properties/value/properties/content/type"),
        Some("string")
    );
    let output_schema = summary_create["output_schema"]
        .as_object()
        .expect("provider JSON Schema object");
    assert!(!output_schema.contains_key("version"));
    assert!(!output_schema.contains_key("root"));
    let summary_id = summary_create["request_id"].as_str().unwrap().to_owned();
    let ordered = summary_facts
        .iter()
        .find_map(|fact| {
            (fact.get("actor_id").and_then(Value::as_str) == Some("summarizer.llm")
                && fact.get("kind").and_then(Value::as_str) == Some("content_chunk_appended"))
            .then(|| {
                let data = fact.pointer("/chunk/data")?;
                data.as_array()
                    .or_else(|| data.get("value").and_then(Value::as_array))
            })
            .flatten()
        })
        .expect("ordered worker results are canonical summarizer input");
    // The dynamic item type is a nominal ADT, so each ordered value retains
    // its constructor evidence instead of erasing WorkerResult vs AgentError.
    assert_eq!(ordered[0]["constructor"], "Ok");
    assert_eq!(ordered[1]["constructor"], "Ok");
    assert_eq!(ordered[0]["value"]["task"], "same");
    assert_eq!(ordered[1]["value"]["task"], "same");
    assert_eq!(
        json!([
            ordered[0]["value"]["description"],
            ordered[1]["value"]["description"]
        ]),
        fixture["scenarios"]["multiple"]["summary_order"]
    );
    send_event(
        &mut stdin,
        completed(
            "mock-provider",
            &summary_id,
            json!({"text":"{\"value\":{\"content\":\"done\"}}"}),
        ),
    )
    .await;
    let result = next_event_of_kind_recording(
        &mut reader,
        "mag.run_result",
        &mut lifecycle_trace,
        &mut observed_events,
    )
    .await;
    assert_eq!(
        result["status"], fixture["scenarios"]["multiple"]["terminal_status"],
        "{result:?}"
    );
    assert_typed_result(&result);
    assert_eq!(
        result["result"]["value"]["value"]["content"],
        fixture["scenarios"]["multiple"]["terminal_content"]
    );
    assert_materialized_item(&observed_events, &fixture["item_delta"], 0);
    assert_materialized_item(&observed_events, &fixture["item_delta"], 1);
    let ids_for = |index: usize| {
        observed_events
            .iter()
            .filter_map(|event| {
                let id = event["id"].as_str()?;
                (event["kind"] == "mag.actor_spawned"
                    && id.starts_with("traverse:6:expand")
                    && id.ends_with(&format!(":{index}")))
                .then_some(id)
            })
            .collect::<std::collections::HashSet<_>>()
    };
    let first_occurrence = ids_for(0);
    let second_occurrence = ids_for(1);
    assert_eq!(first_occurrence.len(), 5);
    assert_eq!(second_occurrence.len(), 5);
    assert!(
        first_occurrence.is_disjoint(&second_occurrence),
        "equal values materialize disjoint actor constellations"
    );
    let applied = lifecycle_trace
        .iter()
        .filter(|kind| kind.as_str() == "mag.modification_applied")
        .count();
    assert_eq!(
        applied, 3,
        "two occurrences plus collection completion apply"
    );
    assert_eq!(
        lifecycle_trace.last().map(String::as_str),
        Some("mag.run_result"),
        "the terminal result closes the observed lifecycle"
    );
    assert_eq!(
        lifecycle_trace
            .iter()
            .any(|kind| kind == "mag.actor_killed"),
        fixture["scenarios"]["actor_killed_before_terminal_result"]
            .as_bool()
            .expect("actor-killed fixture"),
    );
    shutdown(stdin, child).await;
}

#[tokio::test]
async fn dynamic_tasks_zero_uses_empty_collection_identity_and_reaches_summarizer() {
    let data_dir = std::env::temp_dir().join(format!("mag-dynamic-zero-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut child = spawn_mag(&data_dir).await;
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    handshake(&mut reader, &mut stdin).await;
    let loaded = load_dynamic_program(&mut reader, &mut stdin, "zero-load").await;
    send_event(
        &mut stdin,
        obj(
            json!({"kind":"mag.execute","id":"zero-exec","run_id":"zero-run",
      "run_name":"zero","session_id":SESSION_ID,"principal":"lead","conversation_id":CONVERSATION_ID,
      "artifact":loaded["artifact"].clone(),}),
        ),
    )
    .await;
    let planner = next_provider_request(&mut reader, "mock-provider").await;
    assert_eq!(request_actor(&planner), "planner.llm");
    complete_chat(
        &mut reader,
        &mut stdin,
        planner["request_id"].as_str().unwrap(),
        r#"{"value":[]}"#,
    )
    .await;
    let summary = loop {
        let event = next_event(&mut reader, "zero summary create").await;
        if let Some(id) = event.get("id").and_then(Value::as_str) {
            assert!(
                !id.starts_with("traverse:6:expand"),
                "zero branch spawned worker actor {id}"
            );
        }
        if event.get("kind").and_then(Value::as_str) == Some("conversation.provider.invoke.request")
        {
            assert_thin_provider_request(&event, "mock-provider");
            assert_eq!(request_actor(&event), "summarizer.llm");
            break event;
        }
    };
    let summary_id = summary["request_id"].as_str().unwrap().to_owned();
    send_event(
        &mut stdin,
        completed(
            "mock-provider",
            &summary_id,
            json!({"text":"{\"value\":{\"content\":\"empty\"}}"}),
        ),
    )
    .await;
    let result = loop {
        let event = next_event(&mut reader, "zero terminal result").await;
        if let Some(id) = event.get("id").and_then(Value::as_str) {
            assert!(
                !id.starts_with("traverse:6:expand"),
                "zero branch spawned worker actor {id}"
            );
        }
        if event.get("kind").and_then(Value::as_str) == Some("mag.run_result") {
            break event;
        }
    };
    let fixture = dynamic_behavior_fixture();
    assert_eq!(
        result["status"],
        fixture["scenarios"]["zero"]["terminal_status"]
    );
    assert_typed_result(&result);
    assert_eq!(
        result["result"]["value"]["value"]["content"],
        fixture["scenarios"]["zero"]["terminal_content"]
    );
    shutdown(stdin, child).await;
}

#[tokio::test]
async fn dynamic_tasks_one_runs_one_real_worker_and_static_summarizer() {
    let data_dir = std::env::temp_dir().join(format!("mag-dynamic-one-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut child = spawn_mag(&data_dir).await;
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    handshake(&mut reader, &mut stdin).await;
    let loaded = load_dynamic_program(&mut reader, &mut stdin, "one-load").await;
    let artifact = loaded["artifact"].clone();
    send_event(
        &mut stdin,
        obj(
            json!({"kind":"mag.execute","id":"one-exec","run_id":"one-run",
      "run_name":"one","session_id":SESSION_ID,"principal":"lead","conversation_id":CONVERSATION_ID,
      "artifact":artifact}),
        ),
    )
    .await;
    let planner = next_provider_request(&mut reader, "mock-provider").await;
    assert_eq!(request_actor(&planner), "planner.llm");
    complete_chat(
        &mut reader,
        &mut stdin,
        planner["request_id"].as_str().unwrap(),
        r#"{"value":[{"task":"duplicate","description":"only","dependent_tasks":[]}]}"#,
    )
    .await;
    let (worker, worker_facts) =
        next_provider_request_with_facts(&mut reader, "mock-provider").await;
    let fixture = dynamic_behavior_fixture();
    let worker_actor = request_actor(&worker);
    assert_occurrence_actor(worker_actor, "worker.llm", 0);
    assert_eq!(
        structured_input(&worker_facts, worker_actor),
        &json!({"task":"duplicate","description":"only","dependent_tasks":[]}),
        "the ordinary worker receives the planner's Task record intact"
    );
    assert!(worker["request_id"].as_str().is_some());
    complete_chat(
        &mut reader,
        &mut stdin,
        worker["request_id"].as_str().unwrap(),
        r#"{"task":"duplicate","description":"done"}"#,
    )
    .await;
    let summary = next_provider_request(&mut reader, "mock-provider").await;
    assert_eq!(request_actor(&summary), "summarizer.llm");
    complete_chat(
        &mut reader,
        &mut stdin,
        summary["request_id"].as_str().unwrap(),
        r#"{"content":"one done"}"#,
    )
    .await;
    let result = next_event_of_kind(&mut reader, "mag.run_result").await;
    assert_eq!(
        result["status"],
        fixture["scenarios"]["one"]["terminal_status"]
    );
    assert_typed_result(&result);
    assert_eq!(
        result["result"]["value"]["value"]["content"],
        fixture["scenarios"]["one"]["terminal_content"]
    );
    shutdown(stdin, child).await;
}

#[tokio::test]
async fn retained_dynamic_program_survives_source_disposal_and_process_restart() {
    let root = std::env::temp_dir().join(format!("mag-retained-program-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    let source = root.join("source/agentic-loop");
    std::fs::create_dir_all(&source).expect("create temporary source");
    std::fs::copy(
        starter_dir().join("agentic-loop/dynamic-tasks.mag"),
        source.join("dynamic-tasks.mag"),
    )
    .expect("copy operation-bearing source");
    let core_modules = root.join("modules/core");
    let config_modules = root.join("modules/config");
    copy_tree(&module_roots()[0], &core_modules);
    copy_tree(&module_roots()[1], &config_modules);
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let mut compiler_process = spawn_mag(&data_dir).await;
    let mut compiler_stdin = compiler_process.stdin.take().expect("compiler stdin");
    let mut compiler_reader =
        BufReader::new(compiler_process.stdout.take().expect("compiler stdout"));
    handshake(&mut compiler_reader, &mut compiler_stdin).await;
    std::fs::write(root.join("source/mag.toml"), "version = 1\n").unwrap();
    let mut retained = None;
    for status in ["miss", "hit", "bypass"] {
        send_event(
            &mut compiler_stdin,
            obj(json!({
                "kind":"mag.build", "id":"retained-load",
                "project_root":root.join("source"),
                "cache_dir":root.join("cache"), "no_cache":status == "bypass",
                "module_roots":[core_modules, config_modules],
                "entry":"agentic-loop/dynamic-tasks.mag"
            })),
        )
        .await;
        let loaded = next_event_of_kind(&mut compiler_reader, "mag.loaded").await;
        assert_eq!(loaded["build"]["status"], status);
        let result = (loaded["artifact"].clone(), loaded["hash"].clone());
        if let Some(previous) = &retained {
            assert_eq!(&result, previous);
        }
        retained = Some(result);
    }
    let artifact = retained.unwrap().0;
    assert_dynamic_program_envelope(&artifact);
    shutdown(compiler_stdin, compiler_process).await;

    std::fs::remove_dir_all(root.join("source")).expect("dispose source tree");
    std::fs::remove_dir_all(root.join("modules")).expect("dispose module roots");
    std::fs::remove_dir_all(root.join("cache")).expect("dispose build cache");

    let mut runtime_process = spawn_mag(&data_dir).await;
    let mut stdin = runtime_process.stdin.take().expect("runtime stdin");
    let mut reader = BufReader::new(runtime_process.stdout.take().expect("runtime stdout"));
    handshake(&mut reader, &mut stdin).await;
    for run_id in ["retained-a", "retained-b"] {
        send_event(
            &mut stdin,
            obj(json!({
                "kind":"mag.execute", "id":format!("{run_id}-exec"), "run_id":run_id,
                "run_name":"retained", "session_id":SESSION_ID, "principal":"lead",
                "conversation_id":CONVERSATION_ID, "artifact":artifact.clone()
            })),
        )
        .await;
    }
    let planner_a = next_provider_request(&mut reader, "mock-provider").await;
    let planner_b = next_provider_request(&mut reader, "mock-provider").await;
    assert_eq!(request_actor(&planner_a), "planner.llm");
    assert_eq!(request_actor(&planner_b), "planner.llm");
    for planner in [&planner_a, &planner_b] {
        let run = planner["invocation"]["run_id"].as_str().unwrap();
        complete_chat(
            &mut reader,
            &mut stdin,
            planner["request_id"].as_str().expect("planner request id"),
            &json!({"value":[{"task":run,"description":run,"dependent_tasks":[]}]}).to_string(),
        )
        .await;
    }
    let worker_a = next_provider_request(&mut reader, "mock-provider").await;
    let worker_b = next_provider_request(&mut reader, "mock-provider").await;
    assert_ne!(
        worker_a["invocation"]["run_id"],
        worker_b["invocation"]["run_id"]
    );
    for worker in [&worker_b, &worker_a] {
        assert_occurrence_actor(request_actor(worker), "worker.llm", 0);
        let run = worker["invocation"]["run_id"].as_str().unwrap();
        complete_chat(
            &mut reader,
            &mut stdin,
            worker["request_id"].as_str().unwrap(),
            &json!({"task":run,"description":format!("done-{run}")}).to_string(),
        )
        .await;
    }
    let mut trace = Vec::new();
    let mut events = Vec::new();
    let mut summaries = Vec::new();
    for _ in 0..2 {
        let (summary, facts) =
            next_provider_request_recording(&mut reader, "mock-provider", &mut trace, &mut events)
                .await;
        assert_eq!(request_actor(&summary), "summarizer.llm");
        let run = summary["invocation"]["run_id"].as_str().unwrap();
        let input = facts
            .iter()
            .find_map(|fact| {
                (fact["actor_id"] == "summarizer.llm"
                    && fact["run_id"] == run
                    && fact["kind"] == "content_chunk_appended"
                    && fact.pointer("/chunk/kind") == Some(&json!("structured")))
                .then(|| fact.pointer("/chunk/data/value").unwrap())
            })
            .expect("summary receives structured worker results");
        assert_eq!(input.as_array().unwrap().len(), 1);
        assert_eq!(
            input[0]["value"],
            json!({"task":run,"description":format!("done-{run}")})
        );
        summaries.push(summary);
    }
    for summary in summaries {
        let run = summary["invocation"]["run_id"].as_str().unwrap();
        complete_chat(
            &mut reader,
            &mut stdin,
            summary["request_id"].as_str().unwrap(),
            &json!({"content":format!("summary-{run}")}).to_string(),
        )
        .await;
    }
    let first = next_event_of_kind(&mut reader, "mag.run_result").await;
    let second = next_event_of_kind(&mut reader, "mag.run_result").await;
    let mut run_ids = Vec::new();
    for result in [&first, &second] {
        let run = result["run_id"].as_str().unwrap();
        run_ids.push(run);
        assert_eq!(result["status"], "completed");
        assert_typed_result(result);
        assert_eq!(
            result["result"]["value"]["value"]["content"],
            format!("summary-{run}")
        );
    }
    run_ids.sort();
    assert_eq!(run_ids, ["retained-a", "retained-b"]);
    shutdown(stdin, runtime_process).await;
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn dynamic_tasks_invalid_planner_spawns_nothing_and_returns_typed_error() {
    let data_dir = std::env::temp_dir().join(format!("mag-dynamic-invalid-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut child = spawn_mag(&data_dir).await;
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    handshake(&mut reader, &mut stdin).await;
    let loaded = load_dynamic_program(&mut reader, &mut stdin, "invalid-load").await;
    send_event(
        &mut stdin,
        obj(
            json!({"kind":"mag.execute","id":"invalid-exec","run_id":"invalid-run",
      "run_name":"invalid","session_id":SESSION_ID,"principal":"lead","conversation_id":CONVERSATION_ID,
      "artifact":loaded["artifact"].clone(),}),
        ),
    )
    .await;
    for _ in 0..3 {
        let request = next_provider_request(&mut reader, "mock-provider").await;
        assert_eq!(request_actor(&request), "planner.llm");
        complete_chat(
            &mut reader,
            &mut stdin,
            request["request_id"].as_str().unwrap(),
            "not json",
        )
        .await;
    }
    let result = loop {
        let event = next_event(&mut reader, "invalid terminal result").await;
        if let Some(id) = event.get("id").and_then(Value::as_str) {
            assert!(
                !id.starts_with("traverse:6:expand"),
                "invalid branch spawned dynamic actor {id}"
            );
        }
        if event.get("kind").and_then(Value::as_str) == Some("mag.run_result") {
            break event;
        }
    };
    let fixture = dynamic_behavior_fixture();
    assert_eq!(
        result["status"], fixture["scenarios"]["invalid"]["terminal_status"],
        "{result:?}"
    );
    assert_typed_result(&result);
    assert_eq!(
        result["result"]["value"]["value"]["last_output"]["text"], "not json",
        "{result:?}"
    );
    assert_eq!(
        result["result"]["value"]["value"]["reason"]["constructor"],
        "OutputValidationError"
    );
    assert!(result["result"]["value"]["value"]["reason"]["value"]["violations"].is_array());
    assert!(result["result"]["value"]["value"]["reason"]["value"]
        .get("attempts")
        .is_none());
    shutdown(stdin, child).await;
}

#[tokio::test]
async fn lead_turn_runs_through_gate_and_second_turn_replays_seeded_history() {
    let data_dir = std::env::temp_dir().join(format!("mag-lead-turn-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).expect("mkdir data dir");

    let mut child = spawn_mag(&data_dir).await;
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    handshake(&mut reader, &mut stdin).await;
    let program = load_lead_program(&mut reader, &mut stdin).await;

    // ── turn 1: user message → tool round-trip → final answer ──────────
    send_event(
        &mut stdin,
        execute_body(
            "exec-turn-1",
            "lead-run-1",
            turn_artifact(&program, "what is in the repo?"),
            json!([]),
        ),
    )
    .await;

    let started = next_event_of_kind(&mut reader, "mag.run_started").await;
    assert_eq!(
        started.get("run_id").and_then(Value::as_str),
        Some("lead-run-1")
    );
    let scope = started
        .get("scope")
        .and_then(Value::as_str)
        .expect("mag.run_started carries the run's wire-id scope token")
        .to_owned();

    // The bridge drives a thin manager request keyed by a scope-prefixed
    // correlation handle. Full conversation content exists only as canonical
    // facts emitted before the request.
    let (create, initial_facts) = next_provider_request_with_facts(&mut reader, PROVIDER).await;
    let request_id = create
        .get("request_id")
        .and_then(Value::as_str)
        .expect("provider request carries request_id")
        .to_owned();
    assert!(
        request_id.starts_with(&format!("{scope}/")),
        "request id {request_id:?} carries run scope {scope:?}"
    );
    let conversation_id = create
        .get("conversation_id")
        .and_then(Value::as_str)
        .expect("provider request carries conversation identity")
        .to_owned();
    assert_eq!(
        conversation_id, CONVERSATION_ID,
        "provider routing is owned by the durable conversation"
    );
    let canonical_initial = facts_json(&initial_facts);
    assert!(
        canonical_initial.contains("# MAG workspace"),
        "the ambient MAG workspace block is canonical: {initial_facts:?}"
    );
    assert!(
        canonical_initial
            .contains("Writable source directory: `/tmp/nefor/sessions/lead-turn-session/mag`"),
        "the canonical block carries the session workspace dir: {initial_facts:?}"
    );
    assert!(
        canonical_initial.contains("Nefor MAG in Five Minutes"),
        "the canonical block inlines the Nefor MAG guide: {initial_facts:?}"
    );
    assert_eq!(
        create.get("model").and_then(Value::as_str),
        Some("test-model")
    );
    let tools = create
        .get("tools")
        .and_then(Value::as_array)
        .expect("thin request names the program-authored tool surface");
    let tool_names: Vec<&str> = tools.iter().filter_map(Value::as_str).collect();
    for expected in [
        "read_file",
        "write-review",
        "mag-write-file",
        "mag-preview",
        "mag-apply",
    ] {
        assert!(
            tool_names.contains(&expected),
            "lead tool surface carries {expected}; got {tool_names:?}"
        );
    }
    // World queries use direct process tools; the deprecated plain query tools are
    // deliberately off the lead's surface. mag-env is gone entirely — the
    // workspace context is ambient in the system prompt now.
    for absent in ["list_dir", "search_text", "bash", "mag-env"] {
        assert!(
            !tool_names.contains(&absent),
            "lead tool surface must not carry {absent}; got {tool_names:?}"
        );
    }

    assert!(
        canonical_initial.contains("what is in the repo?"),
        "the task is recorded before invoking the provider: {initial_facts:?}"
    );

    // The model calls a tool → the gate invoke rides a scope-prefixed
    // correlation id (the seam the spawner's transcript tool events key on).
    send_event(
        &mut stdin,
        completed(
            PROVIDER,
            &request_id,
            json!({ "tool_calls": [
                { "id": "call-1", "name": "read_file", "args": { "path": "README.md" } }
            ] }),
        ),
    )
    .await;
    let gate_invoke_kind = format!("{GATE}.tool.invoke");
    let invoke = next_event_of_kind(&mut reader, &gate_invoke_kind).await;
    let cap_id = invoke
        .get("id")
        .and_then(Value::as_str)
        .expect("gate invoke carries the kernel correlation id")
        .to_owned();
    assert!(
        cap_id.starts_with(&format!("{scope}/")),
        "gated tool invocation id {cap_id:?} is scoped to the run ({scope:?})"
    );
    assert_eq!(
        invoke.get("name").and_then(Value::as_str),
        Some("read_file")
    );
    let provenance = invoke
        .get("invocation")
        .and_then(Value::as_object)
        .expect("gate invoke carries authoritative run provenance");
    assert_eq!(provenance.get("session_id"), Some(&json!(SESSION_ID)));
    assert_eq!(provenance.get("run_id"), Some(&json!("lead-run-1")));
    assert_eq!(provenance.get("run_scope"), Some(&json!(scope)));
    assert_eq!(provenance.get("principal"), Some(&json!("lead")));
    assert_eq!(provenance.get("capability_id"), Some(&json!(cap_id)));
    assert_eq!(
        provenance.get("actor_id"),
        invoke.get("from"),
        "the run binding signs the invoking actor"
    );
    let notice_text = "Local instruction files available for /private-agent-worktree";
    // A dedicated notice is an orthogonal bus event. Feeding it between the
    // capability invoke and gate result must not create another continuation.
    send_event(
        &mut stdin,
        obj(json!({
            "kind": "chat.instruction.notice",
            "notice_id": "lead:session:private",
            "text": notice_text,
            "path": "/private-agent-worktree",
            "invocation": provenance,
        })),
    )
    .await;
    send_event(
        &mut stdin,
        obj(json!({ "kind": "tool.result", "id": cap_id, "output": "# nefor" })),
    )
    .await;

    // Round 2: the tool result feeds exactly one fresh provider round; answer final.
    let (create2, continuation_facts) =
        next_provider_request_with_facts(&mut reader, PROVIDER).await;
    let request_id2 = create2
        .get("request_id")
        .and_then(Value::as_str)
        .expect("round 2 request_id")
        .to_owned();
    assert_ne!(
        request_id2, request_id,
        "provider request handles stay per-round"
    );
    assert_eq!(
        create2.get("conversation_id").and_then(Value::as_str),
        Some(conversation_id.as_str()),
        "round 2 reuses the durable conversation identity"
    );
    let continuation_history = facts_json(&continuation_facts);
    let recorded_tool_results: Vec<&Value> = continuation_facts
        .iter()
        .filter(|fact| fact.get("kind").and_then(Value::as_str) == Some("tool_result_recorded"))
        .collect();
    assert_eq!(
        recorded_tool_results.len(),
        1,
        "the original gate result has one canonical semantic fact: {continuation_facts:?}"
    );
    assert_eq!(
        recorded_tool_results[0].get("result"),
        Some(&json!("# nefor"))
    );
    assert!(
        !continuation_history.contains(notice_text),
        "instruction notices are absent from canonical conversation history"
    );
    assert!(
        create2.get("output_schema").is_none(),
        "TextAnswer requests direct terminal text without a provider schema"
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    send_event(
        &mut stdin,
        completed(
            PROVIDER,
            &request_id2,
            json!({ "text": "the repo holds nefor" }),
        ),
    )
    .await;

    let result = next_event_of_kind(&mut reader, "mag.run_result").await;
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("completed")
    );
    assert!(
        assert_duration_ms(&result) >= 20,
        "the producer-owned duration includes time spent awaiting the provider"
    );
    assert_eq!(
        result.get("in_reply_to").and_then(Value::as_str),
        Some("exec-turn-1")
    );
    assert_eq!(
        result.pointer_str("/result/value/value"),
        Some("the repo holds nefor"),
        "the sink's final answer rides the terminal reply inline"
    );
    assert!(
        result
            .get("result")
            .and_then(|value| value.get("transcript_delta"))
            .is_none(),
        "terminal graph data does not carry a parallel conversation history"
    );

    // ── turn 2: the spawner seeds {user, answer} from turn 1 ───────────
    send_event(
        &mut stdin,
        execute_body(
            "exec-turn-2",
            "lead-run-2",
            turn_artifact(&program, "and what else?"),
            json!([
                { "role": "user", "content": "what is in the repo?" },
                { "role": "assistant", "content": "the repo holds nefor" }
            ]),
        ),
    )
    .await;

    let (turn2, turn2_facts) = next_provider_request_with_facts(&mut reader, PROVIDER).await;
    assert_eq!(
        turn2.get("conversation_id").and_then(Value::as_str),
        Some(conversation_id.as_str()),
        "a later MAG run for the same conversation keeps provider cache affinity"
    );
    let turn2_history = facts_json(&turn2_facts);
    let first_user = turn2_history
        .find("what is in the repo?")
        .expect("seeded user message is canonical");
    let first_answer = turn2_history
        .find("the repo holds nefor")
        .expect("seeded assistant message is canonical");
    let second_user = turn2_history
        .find("and what else?")
        .expect("second user message is canonical");
    assert!(
        first_user < first_answer && first_answer < second_user,
        "seeded history precedes the new task in canonical fact order: {turn2_facts:?}"
    );

    shutdown(stdin, child).await;
    std::fs::remove_dir_all(&data_dir).ok();
}

#[tokio::test]
async fn kill_run_cancels_the_provider_round_and_settles_killed() {
    let data_dir = std::env::temp_dir().join(format!("mag-lead-kill-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).expect("mkdir data dir");

    let mut child = spawn_mag(&data_dir).await;
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    handshake(&mut reader, &mut stdin).await;
    let program = load_lead_program(&mut reader, &mut stdin).await;

    send_event(
        &mut stdin,
        execute_body(
            "exec-killed",
            "lead-run-killed",
            turn_artifact(&program, "long-running question"),
            json!([]),
        ),
    )
    .await;

    // Wait until the thin provider round is in flight (no reply sent) — the
    // llm actor now holds live external work.
    let request = next_provider_request(&mut reader, PROVIDER).await;
    let request_id = request
        .get("request_id")
        .and_then(Value::as_str)
        .expect("provider request carries request_id")
        .to_owned();

    // Esc: the control plane kills the run.
    send_event(
        &mut stdin,
        obj(json!({ "kind": "mag.kill_run", "run_id": "lead-run-killed" })),
    )
    .await;

    // The reap runs kill handlers through the fold: the dying llm's
    // provider-cancel request reaches the wire BEFORE the terminal reply.
    let cancel_kind = "conversation.provider.cancel.request";
    let mut saw_cancel = false;
    let result = loop {
        let body = next_event(&mut reader, "kill aftermath").await;
        match body.get("kind").and_then(Value::as_str) {
            Some(k) if k == cancel_kind => {
                assert_eq!(
                    body.get("request_id").and_then(Value::as_str),
                    Some(request_id.as_str()),
                    "the cancel targets the in-flight provider chat"
                );
                saw_cancel = true;
            }
            Some("mag.run_result") => break body,
            _ => {}
        }
    };
    assert!(
        saw_cancel,
        "provider cancel observed before the terminal reply"
    );
    assert_eq!(result.get("status").and_then(Value::as_str), Some("killed"));
    assert_duration_ms(&result);
    assert_eq!(
        result.get("run_id").and_then(Value::as_str),
        Some("lead-run-killed")
    );
    assert_eq!(
        result.get("in_reply_to").and_then(Value::as_str),
        Some("exec-killed"),
        "the kill settles the pending execute reply"
    );

    // A duplicate kill is a no-op: no further terminal reply for the run.
    send_event(
        &mut stdin,
        obj(json!({ "kind": "mag.kill_run", "run_id": "lead-run-killed" })),
    )
    .await;
    // Ping to prove liveness and that nothing else was emitted in between.
    send_event(
        &mut stdin,
        obj(json!({ "kind": "mag.ping", "id": "ping-after-kill" })),
    )
    .await;
    let pong = next_event(&mut reader, "pong after duplicate kill").await;
    assert_eq!(pong.get("kind").and_then(Value::as_str), Some("mag.pong"));

    shutdown(stdin, child).await;
    std::fs::remove_dir_all(&data_dir).ok();
}

/// The graceful double-Esc path (`mag.interrupt_run`): interrupting a lead run
/// blocked on an in-flight tool call cancels the real work (a
/// `tool-gate.tool.cancel` for the open correlation reaches the wire), settles
/// that correlation as a failed "interrupted by user" tool result, re-fires the
/// lead llm with that result in its transcript, and lets the run WIND DOWN to a
/// real final answer — `mag.run_result status:"completed"`, NOT killed. This is
/// the incident's tool leg end-to-end through the real plugin + bridge + kernel;
/// the run is never killed, so the turn records itself (no amnesia).
#[tokio::test]
async fn interrupt_run_settles_inflight_tool_and_lead_winds_down_completed() {
    let data_dir = std::env::temp_dir().join(format!("mag-lead-interrupt-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).expect("mkdir data dir");

    let mut child = spawn_mag(&data_dir).await;
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    handshake(&mut reader, &mut stdin).await;
    let program = load_lead_program(&mut reader, &mut stdin).await;

    send_event(
        &mut stdin,
        execute_body(
            "exec-interrupt",
            "lead-run-interrupt",
            turn_artifact(&program, "read a big file for me"),
            json!([]),
        ),
    )
    .await;

    // Round 1: drive the provider round to a tool call.
    let create_kind = "conversation.provider.invoke.request";
    let create = next_provider_request(&mut reader, PROVIDER).await;
    let request_id = create
        .get("request_id")
        .and_then(Value::as_str)
        .expect("provider request carries request_id")
        .to_owned();
    send_event(
        &mut stdin,
        completed(
            PROVIDER,
            &request_id,
            json!({ "tool_calls": [
                { "id": "call-1", "name": "read_file", "args": { "path": "HUGE" } }
            ] }),
        ),
    )
    .await;

    // The gate invoke is now in flight (run-tool blocked awaiting the result).
    let gate_invoke_kind = format!("{GATE}.tool.invoke");
    let invoke = next_event_of_kind(&mut reader, &gate_invoke_kind).await;
    let cap_id = invoke
        .get("id")
        .and_then(Value::as_str)
        .expect("gate invoke carries the kernel correlation id")
        .to_owned();

    // Double-Esc: gracefully interrupt the run instead of killing it. Do NOT
    // reply to the tool call — the interrupt is what settles it.
    send_event(
        &mut stdin,
        obj(json!({ "kind": "mag.interrupt_run", "run_id": "lead-run-interrupt" })),
    )
    .await;

    // Real termination: a tool.cancel for the open correlation reaches the wire
    // (→ the gate would forward it to the owning source and kill the child).
    // The lead llm re-fires with the interrupted tool result recorded as
    // canonical conversation facts before the next thin provider request.
    let cancel_kind = format!("{GATE}.tool.cancel");
    let mut saw_cancel = false;
    let mut interruption_facts = Vec::new();
    let request_id2 = loop {
        let body = next_event(&mut reader, "interrupt aftermath").await;
        match body.get("kind").and_then(Value::as_str) {
            Some("conversation.fact.append") => {
                interruption_facts.push(
                    body.get("fact")
                        .cloned()
                        .expect("append carries interrupted tool fact"),
                );
            }
            Some(k) if k == cancel_kind => {
                assert_eq!(
                    body.get("id").and_then(Value::as_str),
                    Some(cap_id.as_str()),
                    "the cancel targets the in-flight tool correlation"
                );
                saw_cancel = true;
            }
            Some(k) if k == create_kind => {
                assert_thin_provider_request(&body, PROVIDER);
                let c2 = body
                    .get("request_id")
                    .and_then(Value::as_str)
                    .expect("round 2 request_id")
                    .to_owned();
                assert_ne!(
                    c2, request_id,
                    "the re-fire runs on a fresh provider request"
                );
                break c2;
            }
            _ => {}
        }
    };
    assert!(
        saw_cancel,
        "a tool.cancel for the in-flight call reached the wire"
    );
    assert!(
        facts_json(&interruption_facts).contains("[tool error] interrupted by user"),
        "the readable interrupted tool result is canonical before re-fire: {interruption_facts:?}"
    );

    // The re-fired round produces the real final answer; the run completes
    // (NOT killed) so the terminal reply settles the ORIGINAL execute.
    send_event(
        &mut stdin,
        completed(
            PROVIDER,
            &request_id2,
            json!({ "text": "I stopped the read as you asked." }),
        ),
    )
    .await;
    let result = next_event_of_kind(&mut reader, "mag.run_result").await;
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("completed"),
        "the interrupted turn winds down completed, not killed"
    );
    assert_eq!(
        result.get("in_reply_to").and_then(Value::as_str),
        Some("exec-interrupt"),
        "the surviving run settles its original execute reply"
    );
    assert_eq!(
        result.pointer_str("/result/value/value"),
        Some("I stopped the read as you asked."),
        "the lead's post-interrupt final answer rides the terminal reply"
    );

    shutdown(stdin, child).await;
    std::fs::remove_dir_all(&data_dir).ok();
}

/// The TERMINATING interrupt path (`mag.interrupt_run { terminate: true }`) — a
/// dispatched sub-run. Contrast the graceful test above: a dispatched run is
/// ephemeral, so an interrupt must STOP it, not gracefully cancel one tool and
/// let its llm re-fire to a phantom "Completed". Here the interrupt cancels the
/// in-flight tool (a `tool-gate.tool.cancel` reaches the wire → the bash dies)
/// AND ends the run FAILED — `mag.run_result status:"failed" error:"interrupted
/// by user"`, and the llm NEVER re-fires (no round-2 provider request). This pins
/// the incident's fix through the real plugin + bridge + kernel.
#[tokio::test]
async fn terminating_interrupt_cancels_inflight_tool_and_settles_failed_without_refire() {
    let data_dir = std::env::temp_dir().join(format!("mag-lead-terminate-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).expect("mkdir data dir");

    let mut child = spawn_mag(&data_dir).await;
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    handshake(&mut reader, &mut stdin).await;
    let program = load_lead_program(&mut reader, &mut stdin).await;

    send_event(
        &mut stdin,
        execute_body(
            "exec-terminate",
            "sub-run-terminate",
            turn_artifact(&program, "read a big file for me"),
            json!([]),
        ),
    )
    .await;

    // Round 1: drive the provider round to a tool call.
    let create_kind = "conversation.provider.invoke.request";
    let create = next_provider_request(&mut reader, PROVIDER).await;
    let request_id = create
        .get("request_id")
        .and_then(Value::as_str)
        .expect("provider request carries request_id")
        .to_owned();
    send_event(
        &mut stdin,
        completed(
            PROVIDER,
            &request_id,
            json!({ "tool_calls": [
                { "id": "call-1", "name": "read_file", "args": { "path": "HUGE" } }
            ] }),
        ),
    )
    .await;

    // The gate invoke is now in flight (run-tool blocked awaiting the result).
    let gate_invoke_kind = format!("{GATE}.tool.invoke");
    let invoke = next_event_of_kind(&mut reader, &gate_invoke_kind).await;
    let cap_id = invoke
        .get("id")
        .and_then(Value::as_str)
        .expect("gate invoke carries the kernel correlation id")
        .to_owned();

    // Double-Esc on the DISPATCHED run: terminate it. Do NOT reply to the tool
    // call. The terminate cancels it and ends the run — no synthetic settle.
    send_event(
        &mut stdin,
        obj(json!({
            "kind": "mag.interrupt_run",
            "run_id": "sub-run-terminate",
            "terminate": true
        })),
    )
    .await;

    // The run ends failed with NO re-fire: exactly one tool.cancel for the
    // in-flight call reaches the wire before the terminal failed result.
    // NO round-2 provider request appears — the llm never gets to answer "Completed".
    let cancel_kind = format!("{GATE}.tool.cancel");
    let mut cancel_count = 0;
    let result = loop {
        let body = next_event(&mut reader, "terminate aftermath").await;
        match body.get("kind").and_then(Value::as_str) {
            Some(k) if k == cancel_kind => {
                assert_eq!(
                    body.get("id").and_then(Value::as_str),
                    Some(cap_id.as_str()),
                    "the cancel targets the in-flight tool correlation"
                );
                cancel_count += 1;
            }
            Some(k) if k == create_kind => {
                assert_thin_provider_request(&body, PROVIDER);
                panic!(
                    "the terminated run's llm must NOT re-fire — saw a round-2 provider request"
                );
            }
            Some("mag.run_result") => break body,
            _ => {}
        }
    };
    assert_eq!(
        cancel_count, 1,
        "termination emits exactly one tool.cancel for the in-flight call"
    );
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("failed"),
        "the terminated dispatched run settles FAILED, not completed"
    );
    assert_duration_ms(&result);
    assert_eq!(
        result.get("error").and_then(Value::as_str),
        Some("interrupted by user"),
        "the failure carries the interruption reason for the relay to the lead"
    );
    assert_eq!(
        result.get("in_reply_to").and_then(Value::as_str),
        Some("exec-terminate"),
        "the terminated run settles its original execute reply"
    );

    shutdown(stdin, child).await;
    std::fs::remove_dir_all(&data_dir).ok();
}

/// Tiny JSON-pointer helper for `Map<String, Value>` roots.
trait PointerStr {
    fn pointer_str(&self, pointer: &str) -> Option<&str>;
}

impl PointerStr for Map<String, Value> {
    fn pointer_str(&self, pointer: &str) -> Option<&str> {
        let mut parts = pointer.trim_start_matches('/').splitn(2, '/');
        let first = parts.next()?;
        let v = self.get(first)?;
        match parts.next() {
            Some(rest) => v.pointer(&format!("/{rest}"))?.as_str(),
            None => v.as_str(),
        }
    }
}
