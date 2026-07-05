//! Concurrent-execute e2e: two overlapping `mag.execute` requests of the SAME
//! program, with interleaved provider and tool traffic, settling
//! independently.
//!
//! Pins the run-context model (docs/ir.md, Run contexts):
//!   * a second execute arriving mid-first-run resets nothing — both
//!     constellations construct and live side by side;
//!   * wire ids never collide: each run's provider chat handles carry the
//!     run's scope prefix (`r<K>/agent.llm@r<seq>`), correlation ids likewise
//!     (`r<K>/cap-<n>`), so a strict chat-keyed provider and an id-keyed gate
//!     route interleaved (reverse-order included) replies to the right run;
//!   * every kernel lifecycle event carries the run_id it belongs to;
//!   * both `mag.run_result`s settle with their own in_reply_to/run_id and
//!     the results (inline text + persisted sink output) attributed to the
//!     right run.

use std::collections::{HashMap, HashSet};
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
const SESSION_ID: &str = "concurrent-session";
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

fn execute_body(exec_id: &str, run_id: &str, modification: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("mag.execute".into()));
    m.insert("id".into(), Value::String(exec_id.to_owned()));
    m.insert("session_id".into(), Value::String(SESSION_ID.into()));
    m.insert("run_id".into(), Value::String(run_id.to_owned()));
    m.insert("run_name".into(), Value::String("same-program".into()));
    m.insert("modification".into(), modification);
    m
}

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

fn tool_result(id: &str, output: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("tool.result".into()));
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("output".into(), output);
    m
}

/// One agent loop (`agent.llm → agent.run-tool → agent.tool-result →
/// agent.llm`) exiting to the program sink. BOTH runs execute exactly this
/// — identical actor ids, identical routes.
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
                "params": {},
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
                "messages": [ { "role": "user", "content": "go" } ]
            } }
        ],
        "kills": [],
        "rules": []
    })
}

/// Wire recorder: strict provider chat map (create-before-complete, no
/// duplicate creates — a chat_id collision between the two runs would trip
/// it), gate correlation ids seen, and per-run lifecycle events.
#[derive(Default)]
struct Wire {
    live_chats: HashSet<String>,
    created_order: Vec<String>,
    gate_ids: Vec<String>,
    /// run_id → lifecycle event kinds, in arrival order.
    events: HashMap<String, Vec<String>>,
    run_results: HashMap<String, Map<String, Value>>,
}

enum Step {
    /// A `chat.complete` for this chat_id awaits a provider answer.
    ProviderTurn(String),
    /// A gate invoke with this correlation id awaits a tool.result.
    GateInvoke(String),
    /// A `mag.run_result` settled.
    RunResult(String),
    Other,
}

impl Wire {
    fn observe(&mut self, body: &Map<String, Value>) -> Step {
        let kind = match body.get("kind").and_then(Value::as_str) {
            Some(k) => k.to_owned(),
            None => return Step::Other,
        };
        // Every kernel→control-plane lifecycle event carries its run_id
        // (docs/ir.md, Run contexts), as does the terminal reply.
        const LIFECYCLE: [&str; 11] = [
            "mag.run_started",
            "mag.actor_spawned",
            "mag.actor_ready",
            "mag.actor_busy",
            "mag.actor_idle",
            "mag.actor_killed",
            "mag.modification_applied",
            "mag.modification_rejected",
            "mag.modification_noop",
            "mag.run_complete",
            "mag.run_failed",
        ];
        if LIFECYCLE.contains(&kind.as_str()) || kind == "mag.run_result" {
            let run_id = body
                .get("run_id")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("kernel event {kind} without run_id: {body:?}"))
                .to_owned();
            self.events
                .entry(run_id.clone())
                .or_default()
                .push(kind.clone());
            if kind == "mag.run_result" {
                self.run_results.insert(run_id.clone(), body.clone());
                return Step::RunResult(run_id);
            }
            return Step::Other;
        }
        if kind == "tool.invoke" {
            panic!("bare tool.invoke reached the wire");
        }
        if kind == format!("{GATE}.tool.invoke") {
            let id = body
                .get("id")
                .and_then(Value::as_str)
                .expect("gate invoke carries the correlation id")
                .to_owned();
            self.gate_ids.push(id.clone());
            return Step::GateInvoke(id);
        }
        if let Some(suffix) = kind.strip_prefix(&format!("{PROVIDER}.")) {
            let chat_id = body
                .get("chat_id")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{kind} without chat_id"))
                .to_owned();
            match suffix {
                "chat.create" => {
                    assert!(
                        self.live_chats.insert(chat_id.clone()),
                        "chat.create for an already-live chat {chat_id} — \
                         concurrent runs collided on a chat handle"
                    );
                    self.created_order.push(chat_id);
                }
                "chat.complete" => {
                    assert!(
                        self.live_chats.contains(&chat_id),
                        "chat.complete for a chat never created: {chat_id}"
                    );
                    return Step::ProviderTurn(chat_id);
                }
                "chat.delete" => {
                    assert!(
                        self.live_chats.remove(&chat_id),
                        "chat.delete for a chat never created: {chat_id}"
                    );
                }
                _ => {}
            }
        }
        Step::Other
    }
}

/// The scope prefix of a run-scoped wire id (`r2/agent.llm@r1` → `r2/`).
fn scope_of(id: &str) -> &str {
    let idx = id
        .find('/')
        .expect("run-scoped wire ids carry a scope prefix");
    &id[..=idx]
}

#[tokio::test]
async fn two_overlapping_executes_of_the_same_program_settle_independently() {
    let data_dir = std::env::temp_dir().join(format!("mag-concurrent-{}", std::process::id()));
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

    let mut wire = Wire::default();

    // Read until the next provider turn (chat.complete) surfaces.
    macro_rules! next_provider_turn {
        () => {{
            loop {
                let out = read_outgoing(&mut reader, "provider turn").await;
                let body = match event_body(&out) {
                    Some(b) => b.clone(),
                    None => continue,
                };
                if let Step::ProviderTurn(chat) = wire.observe(&body) {
                    break chat;
                }
            }
        }};
    }
    macro_rules! next_gate_invoke {
        () => {{
            loop {
                let out = read_outgoing(&mut reader, "gate invoke").await;
                let body = match event_body(&out) {
                    Some(b) => b.clone(),
                    None => continue,
                };
                if let Step::GateInvoke(id) = wire.observe(&body) {
                    break id;
                }
            }
        }};
    }
    macro_rules! next_run_result {
        () => {{
            loop {
                let out = read_outgoing(&mut reader, "run result").await;
                let body = match event_body(&out) {
                    Some(b) => b.clone(),
                    None => continue,
                };
                if let Step::RunResult(rid) = wire.observe(&body) {
                    break rid;
                }
            }
        }};
    }

    // ── Launch run-1; its llm fires its round-1 provider turn. ──────────────
    send_event(
        &mut stdin,
        execute_body("exec-1", "run-1", agent_loop_modification()),
    )
    .await;
    let chat_1a = next_provider_turn!();

    // ── Launch run-2 mid-run-1: the SAME program, overlapping. ──────────────
    send_event(
        &mut stdin,
        execute_body("exec-2", "run-2", agent_loop_modification()),
    )
    .await;
    let chat_2a = next_provider_turn!();

    // Same actor, same round, different runs: the handles differ only by the
    // run scope — no collision on the wire.
    assert_ne!(
        chat_1a, chat_2a,
        "concurrent runs must not share a chat handle"
    );
    for chat in [&chat_1a, &chat_2a] {
        assert!(
            chat.ends_with("/agent.llm@r1"),
            "round-1 handle is the run-scoped factory chat_id; got {chat}"
        );
    }
    assert_ne!(
        scope_of(&chat_1a),
        scope_of(&chat_2a),
        "each run carries its own scope token"
    );

    // Both constellations spawned independently — run-2 starting killed
    // nothing of run-1.
    for run in ["run-1", "run-2"] {
        let events = wire
            .events
            .get(run)
            .unwrap_or_else(|| panic!("no events for {run}"));
        assert_eq!(
            events.iter().filter(|k| *k == "mag.actor_spawned").count(),
            4,
            "{run} spawned its own four actors; saw {events:?}"
        );
        assert!(
            !events.iter().any(|k| k == "mag.actor_killed"),
            "no kill crossed into {run} while both runs are live; saw {events:?}"
        );
    }

    // ── Interleave: answer run-2 FIRST (reverse of execute order) with a
    // tool call; then run-1 with its own tool call. ──────────────────────────
    send_event(
        &mut stdin,
        chat_result(
            &chat_2a,
            json!({
                "text": "",
                "finish_reason": "tool_calls",
                "tool_calls": [ { "id": "call-2", "name": "list_dir", "args": { "path": "b" } } ]
            }),
        ),
    )
    .await;
    let gate_2 = next_gate_invoke!();
    assert_eq!(
        scope_of(&gate_2),
        scope_of(&chat_2a),
        "run-2's tool correlation carries run-2's scope"
    );

    send_event(
        &mut stdin,
        chat_result(
            &chat_1a,
            json!({
                "text": "",
                "finish_reason": "tool_calls",
                "tool_calls": [ { "id": "call-1", "name": "list_dir", "args": { "path": "a" } } ]
            }),
        ),
    )
    .await;
    let gate_1 = next_gate_invoke!();
    assert_eq!(
        scope_of(&gate_1),
        scope_of(&chat_1a),
        "run-1's tool correlation carries run-1's scope"
    );
    assert_ne!(
        gate_1, gate_2,
        "concurrent runs must not share a correlation id"
    );

    // ── Tool results, again reverse order: run-1's first. Each unblocks its
    // own run's round 2 on a fresh, still-scoped chat. ───────────────────────
    send_event(&mut stdin, tool_result(&gate_1, json!("listing-one"))).await;
    let chat_1b = next_provider_turn!();
    assert!(
        chat_1b.ends_with("/agent.llm@r2"),
        "run-1 advanced to round 2; got {chat_1b}"
    );
    assert_eq!(
        scope_of(&chat_1b),
        scope_of(&chat_1a),
        "run-1's rounds stay in run-1's scope"
    );

    send_event(&mut stdin, tool_result(&gate_2, json!("listing-two"))).await;
    let chat_2b = next_provider_turn!();
    assert!(
        chat_2b.ends_with("/agent.llm@r2"),
        "run-2 advanced to round 2; got {chat_2b}"
    );
    assert_eq!(
        scope_of(&chat_2b),
        scope_of(&chat_2a),
        "run-2's rounds stay in run-2's scope"
    );

    // ── Final answers: run-1 completes first, run-2 after — each settles its
    // own execute. ───────────────────────────────────────────────────────────
    send_event(
        &mut stdin,
        chat_result(
            &chat_1b,
            json!({ "text": "final-one", "finish_reason": "stop" }),
        ),
    )
    .await;
    let settled_1 = next_run_result!();
    assert_eq!(settled_1, "run-1", "run-1's completion settles run-1");

    send_event(
        &mut stdin,
        chat_result(
            &chat_2b,
            json!({ "text": "final-two", "finish_reason": "stop" }),
        ),
    )
    .await;
    let settled_2 = next_run_result!();
    assert_eq!(settled_2, "run-2", "run-2's completion settles run-2");

    // ── Both terminal replies: independent correlation, per-run results, and
    // non-colliding persisted outputs (dirs keyed by run_id, not the shared
    // run_name). ─────────────────────────────────────────────────────────────
    let expectations = [
        ("run-1", "exec-1", "final-one"),
        ("run-2", "exec-2", "final-two"),
    ];
    let mut output_paths = Vec::new();
    for (run_id, exec_id, text) in expectations {
        let result = wire
            .run_results
            .get(run_id)
            .unwrap_or_else(|| panic!("no run_result for {run_id}"));
        assert_eq!(
            result.get("in_reply_to").and_then(Value::as_str),
            Some(exec_id),
            "{run_id} settles its own execute"
        );
        assert_eq!(
            result.get("status").and_then(Value::as_str),
            Some("completed")
        );
        assert_eq!(
            result
                .get("result")
                .and_then(|r| r.get("text"))
                .and_then(Value::as_str),
            Some(text),
            "{run_id}'s inline result is its own final answer"
        );
        let path = result
            .get("output_path")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{run_id} carries its sink output path"))
            .to_owned();
        let content = std::fs::read_to_string(&path).expect("read persisted sink output");
        assert!(
            content.contains(text),
            "{run_id}'s persisted output carries its own answer; got {content:?}"
        );
        output_paths.push(path);
    }
    assert_ne!(
        output_paths[0], output_paths[1],
        "per-run output dirs must not collide despite the shared run_name"
    );

    // Four chats total (two rounds per run), no create/complete mismatch.
    assert_eq!(
        wire.created_order.len(),
        4,
        "each round ran on its own chat"
    );

    // Teardown.
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
