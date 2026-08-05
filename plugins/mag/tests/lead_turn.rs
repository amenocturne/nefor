//! Lead turn-program integration tests against the REAL shipped program
//! (`starter/agentic-loop/lead-turn.mag`), driven the way the turn spawner
//! (starter/agentic-loop) drives it:
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
//! `<provider>.completion.cancel` reaches the wire (provider cancel observed) —
//! and settles the pending execute as `mag.run_result status:"killed"`.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::time::timeout;

const PROVIDER: &str = "test-provider";
const GATE: &str = "tool-gate";
const SESSION_ID: &str = "lead-turn-session";
const CONVERSATION_ID: &str = "lead-conversation";
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The turn spawner appends a `## MAG workspace` block to the system
/// overlay (starter/agentic-loop): the session workspace dir plus the
/// inlined canonical patterns document. Here we build a representative overlay the
/// same way and assert it reaches the provider intact on completion.request.
const LEAD_SYSTEM: &str = "you are the lead\n\n\
## MAG workspace\n\n\
workspace dir: /tmp/nefor/sessions/lead-turn-session/mag\n\n\
### lib/patterns.md\n# MAG patterns — the shapes to reach for\n";

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mag-plugin"))
}

fn starter_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../starter")
}

fn kernel_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel/init.lua")
}

fn assert_typed_result(result: &Map<String, Value>) {
    let semantic = result
        .pointer_str("/result/semantic_type_id")
        .expect("terminal result carries semantic type identity");
    assert!(semantic.starts_with("sha256:"), "{result:?}");
    assert_eq!(
        result.pointer_str("/result/constructor_id"),
        Some(semantic),
        "{result:?}"
    );
    assert!(
        result
            .get("result")
            .and_then(|value| value.get("variant"))
            .is_none(),
        "{result:?}"
    );
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
    let source = if body.get("kind").and_then(Value::as_str) == Some("mag.execute") {
        PluginName::new("agentic-loop").expect("agentic-loop plugin name")
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
            "module_roots": [starter_dir().join("mag/lib").to_string_lossy()],
            "entry": "agentic-loop/lead-turn.mag",
        })),
    )
    .await;
    let loaded = next_event_of_kind(reader, "mag.loaded").await;
    assert_eq!(
        loaded.get("in_reply_to").and_then(Value::as_str),
        Some("lead-turn-load")
    );
    loaded
        .get("artifact")
        .cloned()
        .expect("mag.loaded carries the compiled artifact")
}

/// The spawner's per-turn clone: point the initial task at the user
/// message.
fn turn_artifact(program: &Value, user_text: &str) -> Value {
    let mut m = program.clone();
    let actors = m
        .get_mut("data")
        .and_then(|data| data.get_mut("actors"))
        .and_then(Value::as_array_mut)
        .expect("program has actors");
    for actor in actors {
        if actor.get("foreign").and_then(Value::as_str) == Some("nefor.factory.source") {
            let value = actor
                .get_mut("params")
                .and_then(|params| params.get_mut("value"))
                .and_then(Value::as_object_mut)
                .expect("source actor carries the typed task value");
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
            "lead.llm": {
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

fn completed(provider: &str, request_id: &str, fields: Value) -> Map<String, Value> {
    let mut body = fields.as_object().expect("completion fields").clone();
    body.insert(
        "kind".into(),
        Value::String(format!("{provider}.completion.event")),
    );
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
            "module_roots": [starter_dir().join("mag/lib").to_string_lossy()],
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
    let actors = artifact
        .pointer("/data/actors")
        .and_then(Value::as_array)
        .unwrap();
    let structured = actors
        .iter()
        .find(|actor| actor.get("id").and_then(Value::as_str) == Some("typed-task.llm"))
        .expect("structured actor lowered");
    assert_eq!(
        structured.get("foreign").and_then(Value::as_str),
        Some("nefor.factory.structured-output")
    );
    assert_eq!(
        structured.pointer("/params/schema/version"),
        Some(&json!(1))
    );
    assert_eq!(
        structured
            .pointer("/routes/nefor.agent.Result/0")
            .and_then(Value::as_str),
        None,
        "terminal validated output is not routed onward"
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
                "typed-task.llm": {
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
                "typed-task.llm": { "provider": MOCK, "model": "mock-model" }
            }
        })),
    )
    .await;

    let create_kind = format!("{MOCK}.completion.request");
    let create = next_event_of_kind(&mut reader, &create_kind).await;
    let first_chat = create["request_id"].as_str().unwrap().to_owned();
    assert_eq!(create.pointer_str("/output_schema/type"), Some("object"));
    assert_eq!(
        create.pointer_str("/output_schema/properties/task/type"),
        Some("string")
    );
    assert_eq!(
        create
            .get("output_schema")
            .and_then(|schema| schema.get("additionalProperties")),
        Some(&Value::Bool(false))
    );
    send_event(
        &mut stdin,
        completed(MOCK, &first_chat, json!({ "text": "```json\n{}\n```" })),
    )
    .await;

    let create2 = next_event_of_kind(&mut reader, &create_kind).await;
    let second_chat = create2
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap()
        .to_owned();
    let correction_history = serde_json::to_string(&create2["messages"]).unwrap();
    assert!(correction_history.contains("malformed_json"));
    send_event(
        &mut stdin,
        completed(MOCK, &second_chat, json!({ "text": "{\"task\":\"build\",\"description\":\"Implement it\",\"dependent-tasks\":[]}" })),
    )
    .await;
    let result = next_event_of_kind(&mut reader, "mag.run_result").await;
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("completed")
    );
    assert_typed_result(&result);
    assert_eq!(result.pointer_str("/result/value/task"), Some("build"));
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
            "module_roots": [starter_dir().join("mag/lib").to_string_lossy()],
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

    let builder_create =
        next_event_of_kind(&mut reader, &format!("{MOCK}.completion.request")).await;
    let builder_id = builder_create["request_id"].as_str().unwrap().to_owned();
    send_event(
        &mut stdin,
        completed(MOCK, &builder_id, json!({"text": "partial builder notes"})),
    )
    .await;

    let reviewer_create =
        next_event_of_kind(&mut reader, &format!("{MOCK}.completion.request")).await;
    let reviewer_id = reviewer_create["request_id"].as_str().unwrap().to_owned();
    let reviewer_history = serde_json::to_string(&reviewer_create["messages"]).unwrap();
    assert!(
        reviewer_history.contains("partial builder notes"),
        "reviewer did not receive builder last_output"
    );
    send_event(
        &mut stdin,
        completed(
            MOCK,
            &reviewer_id,
            json!({"text": "{\"assessment\":\"continue from partial work\"}"}),
        ),
    )
    .await;
    let result = next_event_of_kind(&mut reader, "mag.run_result").await;
    assert_typed_result(&result);
    assert_eq!(
        result.pointer_str("/result/value/assessment"),
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
    send_event(
        stdin,
        completed("mock-provider", request_id, json!({"text": text})),
    )
    .await;
}

#[tokio::test]
async fn dynamic_tasks_real_agents_complete_out_of_order_and_preserve_planner_order() {
    let data_dir = std::env::temp_dir().join(format!("mag-dynamic-tasks-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut child = spawn_mag(&data_dir).await;
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let stderr = child.stderr.take().unwrap();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[dynamic mag stderr] {line}");
        }
    });
    handshake(&mut reader, &mut stdin).await;
    send_event(
        &mut stdin,
        obj(json!({"kind":"mag.load","id":"dynamic-load",
      "source_dir":starter_dir().to_string_lossy(),
      "module_roots":[starter_dir().join("mag/lib").to_string_lossy()],
      "entry":"agentic-loop/dynamic-tasks.mag"})),
    )
    .await;
    let loaded = next_event_of_kind(&mut reader, "mag.loaded").await;
    let artifact = &loaded["artifact"];
    assert_eq!(
        artifact
            .pointer("/data/messages/0/to")
            .and_then(Value::as_str),
        Some("planner.entry")
    );
    assert_eq!(
        artifact
            .pointer("/data/messages/0/content/kind")
            .and_then(Value::as_str),
        Some("task")
    );
    send_event(
        &mut stdin,
        obj(json!({"kind":"mag.execute","id":"dynamic-exec",
      "run_id":"dynamic-run","run_name":"dynamic-tasks","session_id":SESSION_ID,
      "principal":"lead","conversation_id":CONVERSATION_ID})),
    )
    .await;

    let planner = loop {
        let event = next_event(&mut reader, "planner completion.request").await;
        if event.get("kind").and_then(Value::as_str) == Some("mock-provider.completion.request") {
            break event;
        }
        if event.get("kind").and_then(Value::as_str) == Some("mag.error") {
            panic!("dynamic execute: {event:?}");
        }
    };
    let planner_id = planner["request_id"].as_str().unwrap().to_owned();
    complete_chat(&mut reader, &mut stdin, &planner_id,
      r#"{"value":[{"task":"a","description":"first","dependent_tasks":[]},{"task":"a.collect","description":"second","dependent_tasks":["a"]}]}"#).await;

    let first = loop {
        let event = next_event(&mut reader, "first worker completion.request").await;
        if event.get("kind").and_then(Value::as_str) == Some("mock-provider.completion.request") {
            break event;
        }
        if matches!(
            event.get("kind").and_then(Value::as_str),
            Some("mag.error" | "mag.run_result")
        ) {
            panic!("planner expansion failed: {event:?}");
        }
    };
    let second = next_event_of_kind(&mut reader, "mock-provider.completion.request").await;
    let first_id = first["request_id"].as_str().unwrap().to_owned();
    let second_id = second["request_id"].as_str().unwrap().to_owned();
    assert_ne!(
        first_id, second_id,
        "parallel workers have distinct request ids"
    );
    // Planner order is one,two; completion order is two,one.
    send_event(
        &mut stdin,
        completed(
            "mock-provider",
            &first_id,
            json!({"text":r#"{"task":"a.collect","description":"done second"}"#}),
        ),
    )
    .await;
    send_event(
        &mut stdin,
        completed(
            "mock-provider",
            &second_id,
            json!({"text":r#"{"task":"a","description":"done first"}"#}),
        ),
    )
    .await;

    let summary_create = next_event_of_kind(&mut reader, "mock-provider.completion.request").await;
    assert_eq!(
        summary_create.pointer_str("/output_schema/type"),
        Some("object")
    );
    assert_eq!(
        summary_create.pointer_str("/output_schema/title"),
        Some("main.Summary")
    );
    assert_eq!(
        summary_create.pointer_str("/output_schema/properties/content/type"),
        Some("string")
    );
    let output_schema = summary_create["output_schema"]
        .as_object()
        .expect("provider JSON Schema object");
    assert!(!output_schema.contains_key("version"));
    assert!(!output_schema.contains_key("root"));
    let summary_id = summary_create["request_id"].as_str().unwrap().to_owned();
    let ordered = summary_create["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .find_map(|message| message.get("content")?.as_array())
        })
        .expect("typed worker list reached summarizer");
    assert_eq!(ordered[0]["task"], "a");
    assert_eq!(ordered[1]["task"], "a.collect");
    send_event(
        &mut stdin,
        completed(
            "mock-provider",
            &summary_id,
            json!({"text":"{\"content\":\"done\"}"}),
        ),
    )
    .await;
    let result = next_event_of_kind(&mut reader, "mag.run_result").await;
    assert_eq!(result["status"], "completed", "{result:?}");
    assert_typed_result(&result);
    assert_eq!(result["result"]["value"]["content"], "done");
    shutdown(stdin, child).await;
}

#[tokio::test]
async fn dynamic_tasks_zero_bypasses_collector_and_reaches_static_summarizer() {
    let data_dir = std::env::temp_dir().join(format!("mag-dynamic-zero-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut child = spawn_mag(&data_dir).await;
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    handshake(&mut reader, &mut stdin).await;
    send_event(&mut stdin,obj(json!({"kind":"mag.load","id":"zero-load",
      "source_dir":starter_dir().to_string_lossy(),"module_roots":[starter_dir().join("mag/lib").to_string_lossy()],
      "entry":"agentic-loop/dynamic-tasks.mag"}))).await;
    next_event_of_kind(&mut reader, "mag.loaded").await;
    send_event(
        &mut stdin,
        obj(
            json!({"kind":"mag.execute","id":"zero-exec","run_id":"zero-run",
      "run_name":"zero","session_id":SESSION_ID,"principal":"lead","conversation_id":CONVERSATION_ID}),
        ),
    )
    .await;
    let planner = next_event_of_kind(&mut reader, "mock-provider.completion.request").await;
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
                !id.starts_with("expand.worker") && id != "expand.collector",
                "zero branch spawned dynamic actor {id}"
            );
        }
        if event.get("kind").and_then(Value::as_str) == Some("mock-provider.completion.request") {
            break event;
        }
    };
    let summary_id = summary["request_id"].as_str().unwrap().to_owned();
    send_event(
        &mut stdin,
        completed(
            "mock-provider",
            &summary_id,
            json!({"text":"{\"content\":\"empty\"}"}),
        ),
    )
    .await;
    let result = loop {
        let event = next_event(&mut reader, "zero terminal result").await;
        if let Some(id) = event.get("id").and_then(Value::as_str) {
            assert!(
                !id.starts_with("expand.worker") && id != "expand.collector",
                "zero branch spawned dynamic actor {id}"
            );
        }
        if event.get("kind").and_then(Value::as_str) == Some("mag.run_result") {
            break event;
        }
    };
    assert_typed_result(&result);
    assert_eq!(result["result"]["value"]["content"], "empty");
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
    send_event(&mut stdin,obj(json!({"kind":"mag.load","id":"one-load",
      "source_dir":starter_dir().to_string_lossy(),"module_roots":[starter_dir().join("mag/lib").to_string_lossy()],
      "entry":"agentic-loop/dynamic-tasks.mag"}))).await;
    next_event_of_kind(&mut reader, "mag.loaded").await;
    send_event(
        &mut stdin,
        obj(
            json!({"kind":"mag.execute","id":"one-exec","run_id":"one-run",
      "run_name":"one","session_id":SESSION_ID,"principal":"lead","conversation_id":CONVERSATION_ID}),
        ),
    )
    .await;
    let planner = next_event_of_kind(&mut reader, "mock-provider.completion.request").await;
    complete_chat(
        &mut reader,
        &mut stdin,
        planner["request_id"].as_str().unwrap(),
        r#"{"value":[{"task":"duplicate","description":"only","dependent_tasks":[]}]}"#,
    )
    .await;
    let worker = next_event_of_kind(&mut reader, "mock-provider.completion.request").await;
    assert!(worker["request_id"].as_str().is_some());
    complete_chat(
        &mut reader,
        &mut stdin,
        worker["request_id"].as_str().unwrap(),
        r#"{"task":"duplicate","description":"done"}"#,
    )
    .await;
    let summary = next_event_of_kind(&mut reader, "mock-provider.completion.request").await;
    complete_chat(
        &mut reader,
        &mut stdin,
        summary["request_id"].as_str().unwrap(),
        r#"{"content":"one done"}"#,
    )
    .await;
    let result = next_event_of_kind(&mut reader, "mag.run_result").await;
    assert_typed_result(&result);
    assert_eq!(result["result"]["value"]["content"], "one done");
    shutdown(stdin, child).await;
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
    send_event(&mut stdin,obj(json!({"kind":"mag.load","id":"invalid-load",
      "source_dir":starter_dir().to_string_lossy(),"module_roots":[starter_dir().join("mag/lib").to_string_lossy()],
      "entry":"agentic-loop/dynamic-tasks.mag"}))).await;
    next_event_of_kind(&mut reader, "mag.loaded").await;
    send_event(
        &mut stdin,
        obj(
            json!({"kind":"mag.execute","id":"invalid-exec","run_id":"invalid-run",
      "run_name":"invalid","session_id":SESSION_ID,"principal":"lead","conversation_id":CONVERSATION_ID}),
        ),
    )
    .await;
    for _ in 0..3 {
        let request = next_event_of_kind(&mut reader, "mock-provider.completion.request").await;
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
                !id.starts_with("expand.worker") && id != "expand.collector",
                "invalid branch spawned dynamic actor {id}"
            );
        }
        if event.get("kind").and_then(Value::as_str) == Some("mag.run_result") {
            break event;
        }
    };
    assert_eq!(result["status"], "completed", "{result:?}");
    assert_typed_result(&result);
    assert_eq!(
        result["result"]["value"]["last_output"]["text"], "not json",
        "{result:?}"
    );
    assert!(result["result"]["value"]["reason"]["type"]
        .as_str()
        .is_some_and(|tag| tag.starts_with("sha256:")));
    assert!(result["result"]["value"]["reason"]["value"]["violations"].is_array());
    assert!(result["result"]["value"]["reason"]["value"]
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
    let stderr = child.stderr.take().expect("stderr");
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[mag stderr] {line}");
        }
    });

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

    // The bridge drives the provider round: completion.request carries the
    // overlaid system + the program-authored tool surface, keyed by a
    // scope-prefixed chat handle.
    let create_kind = format!("{PROVIDER}.completion.request");
    let create = next_event_of_kind(&mut reader, &create_kind).await;
    let request_id = create
        .get("request_id")
        .and_then(Value::as_str)
        .expect("completion.request carries request_id")
        .to_owned();
    assert!(
        request_id.starts_with(&format!("{scope}/")),
        "request id {request_id:?} carries run scope {scope:?}"
    );
    let conversation_id = create
        .get("conversation_id")
        .and_then(Value::as_str)
        .expect("completion.request carries conversation identity")
        .to_owned();
    assert_eq!(
        conversation_id, CONVERSATION_ID,
        "provider routing is owned by the durable conversation"
    );
    let system = create
        .get("system")
        .and_then(Value::as_str)
        .expect("completion.request carries the spawner's system overlay");
    assert!(
        system.contains("## MAG workspace"),
        "the ambient MAG workspace block reaches the provider; got {system:?}"
    );
    assert!(
        system.contains("workspace dir: /tmp/nefor/sessions/lead-turn-session/mag"),
        "the block carries the session workspace dir; got {system:?}"
    );
    assert!(
        system.contains("MAG patterns"),
        "the block inlines the patterns doc; got {system:?}"
    );
    assert_eq!(
        create.get("model").and_then(Value::as_str),
        Some("test-model")
    );
    let tools = create
        .get("tools")
        .and_then(Value::as_array)
        .expect("completion.request advertises the program-authored tool surface");
    let tool_names: Vec<&str> = tools.iter().filter_map(Value::as_str).collect();
    for expected in ["read_file", "write-review", "mag", "mag-eval"] {
        assert!(
            tool_names.contains(&expected),
            "lead tool surface carries {expected}; got {tool_names:?}"
        );
    }
    // World queries ride mag-eval expressions; the plain query tools are
    // deliberately off the lead's surface. mag-env is gone entirely — the
    // workspace context is ambient in the system prompt now.
    for absent in ["list_dir", "search_text", "bash", "mag-env"] {
        assert!(
            !tool_names.contains(&absent),
            "lead tool surface must not carry {absent}; got {tool_names:?}"
        );
    }

    // Turn 1, round 1: the request carries the task as its only message.
    assert_eq!(
        create.pointer_str("/messages/0/content/value/prompt"),
        Some("what is in the repo?"),
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
    let create2 = next_event_of_kind(&mut reader, &create_kind).await;
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
    let continuation_messages = create2["messages"].as_array().expect("round 2 messages");
    let tool_results: Vec<&Value> = continuation_messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        .collect();
    assert_eq!(
        tool_results.len(),
        1,
        "the original gate result reaches provider history exactly once"
    );
    assert_eq!(
        tool_results[0].pointer("/content").and_then(Value::as_str),
        Some("# nefor")
    );
    let provider_history = serde_json::to_string(continuation_messages).unwrap();
    assert!(
        !provider_history.contains(notice_text),
        "instruction notices are absent from provider history"
    );
    send_event(
        &mut stdin,
        completed(
            PROVIDER,
            &request_id2,
            json!({ "text": "{\"content\":\"the repo holds nefor\"}" }),
        ),
    )
    .await;

    let result = next_event_of_kind(&mut reader, "mag.run_result").await;
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("completed")
    );
    assert_eq!(
        result.get("in_reply_to").and_then(Value::as_str),
        Some("exec-turn-1")
    );
    assert_eq!(
        result.pointer_str("/result/value/content"),
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

    let turn2 = next_event_of_kind(&mut reader, &create_kind).await;
    assert_eq!(
        turn2.get("conversation_id").and_then(Value::as_str),
        Some(conversation_id.as_str()),
        "a later MAG run for the same conversation keeps provider cache affinity"
    );
    let messages = turn2["messages"].as_array().expect("turn 2 messages");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "what is in the repo?");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "the repo holds nefor");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(
        messages[2]
            .pointer("/content/value/prompt")
            .and_then(Value::as_str),
        Some("and what else?")
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
    let stderr = child.stderr.take().expect("stderr");
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[mag stderr] {line}");
        }
    });

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

    // Wait until the provider round is in flight (completion.request on the
    // wire, no reply sent) — the llm actor now holds live external work.
    let request = next_event_of_kind(&mut reader, &format!("{PROVIDER}.completion.request")).await;
    let request_id = request
        .get("request_id")
        .and_then(Value::as_str)
        .expect("completion.request carries request_id")
        .to_owned();

    // Esc: the control plane kills the run.
    send_event(
        &mut stdin,
        obj(json!({ "kind": "mag.kill_run", "run_id": "lead-run-killed" })),
    )
    .await;

    // The reap runs kill handlers through the fold: the dying llm's
    // provider-cancel envelope reaches the wire BEFORE the terminal reply.
    let cancel_kind = format!("{PROVIDER}.completion.cancel");
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
    let stderr = child.stderr.take().expect("stderr");
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[mag stderr] {line}");
        }
    });

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
    let create_kind = format!("{PROVIDER}.completion.request");
    let create = next_event_of_kind(&mut reader, &create_kind).await;
    let request_id = create
        .get("request_id")
        .and_then(Value::as_str)
        .expect("completion.request carries request_id")
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
    // The lead llm re-fires: a fresh completion.request whose transcript carries the
    // interrupted tool result as a readable `[tool error] interrupted by user`.
    let cancel_kind = format!("{GATE}.tool.cancel");
    let mut saw_cancel = false;
    let request_id2 = loop {
        let body = next_event(&mut reader, "interrupt aftermath").await;
        match body.get("kind").and_then(Value::as_str) {
            Some(k) if k == cancel_kind => {
                assert_eq!(
                    body.get("id").and_then(Value::as_str),
                    Some(cap_id.as_str()),
                    "the cancel targets the in-flight tool correlation"
                );
                saw_cancel = true;
            }
            Some(k) if k == create_kind => {
                let c2 = body
                    .get("request_id")
                    .and_then(Value::as_str)
                    .expect("round 2 request_id")
                    .to_owned();
                assert_ne!(
                    c2, request_id,
                    "the re-fire runs on a fresh provider request"
                );
                let history = serde_json::to_string(&body["messages"]).unwrap();
                assert!(
                    history.contains("[tool error] interrupted by user"),
                    "the lead re-fired with the interrupted result as a readable tool turn"
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

    // The re-fired round produces the real final answer; the run completes
    // (NOT killed) so the terminal reply settles the ORIGINAL execute.
    send_event(
        &mut stdin,
        completed(
            PROVIDER,
            &request_id2,
            json!({ "text": "{\"content\":\"I stopped the read as you asked.\"}" }),
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
        result.pointer_str("/result/value/content"),
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
/// by user"`, and the llm NEVER re-fires (no round-2 `completion.request`). This pins
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
    let stderr = child.stderr.take().expect("stderr");
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[mag stderr] {line}");
        }
    });

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
    let create_kind = format!("{PROVIDER}.completion.request");
    let create = next_event_of_kind(&mut reader, &create_kind).await;
    let request_id = create
        .get("request_id")
        .and_then(Value::as_str)
        .expect("completion.request carries request_id")
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

    // The run ends failed with NO re-fire: a tool.cancel for the in-flight call
    // reaches the wire, and the terminal reply is `status:"failed"`. Crucially,
    // NO round-2 completion.request appears — the llm never gets to answer "Completed".
    let cancel_kind = format!("{GATE}.tool.cancel");
    let mut saw_cancel = false;
    let result = loop {
        let body = next_event(&mut reader, "terminate aftermath").await;
        match body.get("kind").and_then(Value::as_str) {
            Some(k) if k == cancel_kind => {
                assert_eq!(
                    body.get("id").and_then(Value::as_str),
                    Some(cap_id.as_str()),
                    "the cancel targets the in-flight tool correlation"
                );
                saw_cancel = true;
            }
            Some(k) if k == create_kind => {
                panic!(
                    "the terminated run's llm must NOT re-fire — saw a round-2 completion.request"
                );
            }
            Some("mag.run_result") => break body,
            _ => {}
        }
    };
    assert!(
        saw_cancel,
        "a tool.cancel for the in-flight call reached the wire before the run ended"
    );
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("failed"),
        "the terminated dispatched run settles FAILED, not completed"
    );
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
