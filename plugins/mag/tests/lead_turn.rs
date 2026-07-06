//! Lead turn-program integration tests against the REAL shipped program
//! (`starter/agentic-loop/lead-turn.mag`), driven the way the turn spawner
//! (starter/agentic-loop) drives it:
//!
//!   * `mag.load` the shipped program from the starter tree, take the
//!     compiled modification off `mag.loaded`;
//!   * per turn, clone it — point the initial `mag.Task` at the user
//!     message — and overlay `{ system, provider, model, history }` onto
//!     the lead llm actor via `params_overlay`;
//!   * `mag.execute` with the modification inline.
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
//! `<provider>.chat.cancel` reaches the wire (provider cancel observed) —
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
const READ_TIMEOUT: Duration = Duration::from_secs(30);

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mag-plugin"))
}

fn starter_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../starter")
}

fn kernel_path() -> PathBuf {
    starter_dir().join("mag-kernel/init.lua")
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

/// Load the shipped lead turn-program and return its compiled modification.
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
        .get("modification")
        .cloned()
        .expect("mag.loaded carries the compiled modification")
}

/// The spawner's per-turn clone: point the initial mag.Task at the user
/// message.
fn turn_modification(program: &Value, user_text: &str) -> Value {
    let mut m = program.clone();
    let messages = m
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .expect("program has messages");
    for msg in messages {
        if let Some(content) = msg.get_mut("content") {
            content["prompt"] = Value::String(user_text.to_owned());
        }
    }
    m
}

/// The spawner's per-turn execute: modification inline + the config/history
/// overlay on the lead llm actor.
fn execute_body(
    exec_id: &str,
    run_id: &str,
    modification: Value,
    history: Value,
) -> Map<String, Value> {
    obj(json!({
        "kind": "mag.execute",
        "id": exec_id,
        "run_id": run_id,
        "run_name": "lead",
        "session_id": SESSION_ID,
        "modification": modification,
        "params_overlay": {
            "lead.llm": {
                "system": "you are the lead",
                "provider": PROVIDER,
                "model": "test-model",
                "reasoning_effort": "high",
                "history": history,
            }
        }
    }))
}

fn chat_result(chat_id: &str, output: Value) -> Map<String, Value> {
    obj(json!({
        "kind": format!("{PROVIDER}.chat.complete.result"),
        "chat_id": chat_id,
        "output": output,
    }))
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
            turn_modification(&program, "what is in the repo?"),
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

    // The bridge drives the provider round: chat.create carries the
    // overlaid system + the program-authored tool surface, keyed by a
    // scope-prefixed chat handle.
    let create_kind = format!("{PROVIDER}.chat.create");
    let create = next_event_of_kind(&mut reader, &create_kind).await;
    let chat_id = create
        .get("chat_id")
        .and_then(Value::as_str)
        .expect("chat.create carries chat_id")
        .to_owned();
    assert!(
        chat_id.starts_with(&format!("{scope}/lead.llm@")),
        "chat handle {chat_id:?} carries the run scope {scope:?} + the llm actor id"
    );
    assert_eq!(
        create.get("system").and_then(Value::as_str),
        Some("you are the lead"),
        "the spawner's system overlay reaches the provider"
    );
    assert_eq!(
        create.get("model").and_then(Value::as_str),
        Some("test-model")
    );
    let tools = create
        .get("tools")
        .and_then(Value::as_array)
        .expect("chat.create advertises the program-authored tool surface");
    let tool_names: Vec<&str> = tools.iter().filter_map(Value::as_str).collect();
    for expected in ["read_file", "write-review", "mag", "mag-eval", "mag-env"] {
        assert!(
            tool_names.contains(&expected),
            "lead tool surface carries {expected}; got {tool_names:?}"
        );
    }
    // World queries ride mag-eval expressions; the plain query tools are
    // deliberately off the lead's surface.
    for absent in ["list_dir", "search_text", "bash"] {
        assert!(
            !tool_names.contains(&absent),
            "lead tool surface must not carry {absent}; got {tool_names:?}"
        );
    }

    // Turn 1, round 1: the only appended message is the task (empty seed).
    let append_kind = format!("{PROVIDER}.chat.append");
    let append = next_event_of_kind(&mut reader, &append_kind).await;
    assert_eq!(
        append.pointer_str("/message/content"),
        Some("what is in the repo?"),
    );
    let complete_kind = format!("{PROVIDER}.chat.complete");
    next_event_of_kind(&mut reader, &complete_kind).await;

    // The model calls a tool → the gate invoke rides a scope-prefixed
    // correlation id (the seam the spawner's transcript tool events key on).
    send_event(
        &mut stdin,
        chat_result(
            &chat_id,
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
    send_event(
        &mut stdin,
        obj(json!({ "kind": "tool.result", "id": cap_id, "output": "# nefor" })),
    )
    .await;

    // Round 2: the tool result feeds a fresh provider round; answer final.
    let create2 = next_event_of_kind(&mut reader, &create_kind).await;
    let chat_id2 = create2
        .get("chat_id")
        .and_then(Value::as_str)
        .expect("round 2 chat_id")
        .to_owned();
    next_event_of_kind(&mut reader, &complete_kind).await;
    send_event(
        &mut stdin,
        chat_result(&chat_id2, json!({ "text": "the repo holds nefor" })),
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
        result.pointer_str("/result/text"),
        Some("the repo holds nefor"),
        "the sink's final answer rides the terminal reply inline"
    );

    // ── turn 2: the spawner seeds {user, answer} from turn 1 ───────────
    send_event(
        &mut stdin,
        execute_body(
            "exec-turn-2",
            "lead-run-2",
            turn_modification(&program, "and what else?"),
            json!([
                { "role": "user", "content": "what is in the repo?" },
                { "role": "assistant", "content": "the repo holds nefor" }
            ]),
        ),
    )
    .await;

    next_event_of_kind(&mut reader, &create_kind).await;
    // The llm replays the seeded history ahead of the new task: three
    // appends, in order.
    let a1 = next_event_of_kind(&mut reader, &append_kind).await;
    assert_eq!(a1.pointer_str("/message/role"), Some("user"));
    assert_eq!(
        a1.pointer_str("/message/content"),
        Some("what is in the repo?")
    );
    let a2 = next_event_of_kind(&mut reader, &append_kind).await;
    assert_eq!(a2.pointer_str("/message/role"), Some("assistant"));
    assert_eq!(
        a2.pointer_str("/message/content"),
        Some("the repo holds nefor")
    );
    let a3 = next_event_of_kind(&mut reader, &append_kind).await;
    assert_eq!(a3.pointer_str("/message/role"), Some("user"));
    assert_eq!(a3.pointer_str("/message/content"), Some("and what else?"));

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
            turn_modification(&program, "long-running question"),
            json!([]),
        ),
    )
    .await;

    // Wait until the provider round is in flight (chat.complete on the
    // wire, no reply sent) — the llm actor now holds live external work.
    let complete_kind = format!("{PROVIDER}.chat.complete");
    let complete = next_event_of_kind(&mut reader, &complete_kind).await;
    let chat_id = complete
        .get("chat_id")
        .and_then(Value::as_str)
        .expect("chat.complete carries chat_id")
        .to_owned();

    // Esc: the control plane kills the run.
    send_event(
        &mut stdin,
        obj(json!({ "kind": "mag.kill_run", "run_id": "lead-run-killed" })),
    )
    .await;

    // The reap runs kill handlers through the fold: the dying llm's
    // provider-cancel envelope reaches the wire BEFORE the terminal reply.
    let cancel_kind = format!("{PROVIDER}.chat.cancel");
    let mut saw_cancel = false;
    let result = loop {
        let body = next_event(&mut reader, "kill aftermath").await;
        match body.get("kind").and_then(Value::as_str) {
            Some(k) if k == cancel_kind => {
                assert_eq!(
                    body.get("chat_id").and_then(Value::as_str),
                    Some(chat_id.as_str()),
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
