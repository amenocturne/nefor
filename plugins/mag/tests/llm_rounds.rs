//! Round/chat-lifecycle protocol tests for the llm ⇄ provider-bridge leg,
//! driven against a scripted provider that ENFORCES the real provider's
//! chat-id-keyed semantics (plugins/chatgpt-provider holds a `Chats` map;
//! append/complete on a chat it never created is an upstream error). The
//! earlier harnesses answered any chat_id — exactly the lenience that let two
//! real-session regressions pass CI:
//!
//!   * Round 2 of a tool round-trip created a FRESH provider chat but appended
//!     only the newest tool message — no user turn, no assistant tool_calls —
//!     which a real OpenAI-dialect endpoint rejects ("tool message without
//!     preceding tool_calls") → `finish_reason: "error"` (session 27c60892,
//!     chat explorer.llm@r4).
//!   * That error result then classified as a FinalAnswer and routed to the
//!     sink: the run "completed" with an empty answer — error masking.
//!   * A re-executed program found its actor ids still alive in the resident
//!     kernel: every spawn degraded to a duplicate-alive no-op — no
//!     `mag.actor_spawned` / `mag.actor_ready` on the bus, and the stale llm
//!     instance's round counter kept counting (@r3 on a fresh run).
//!
//! Three tests pin the fixes: full-transcript replay per round over strict
//! chats; a provider error failing the run (`mag.run_failed` + terminal
//! `mag.run_result status:"failed"` carrying the detail, sink never fires);
//! and a re-execute respawning a fresh constellation (spawn/ready events again,
//! rounds restarting at @r1).

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
const SESSION_ID: &str = "llm-rounds-session";
const READ_TIMEOUT: Duration = Duration::from_secs(30);

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mag-plugin"))
}

fn kernel_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel/init.lua")
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

/// The gate's reply shape: a broadcast `tool.result { id, output }` keyed by
/// the caller's outer id (the kernel correlation id the bridge kept).
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

fn execute_body(exec_id: &str, run_id: &str, modification: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("mag.execute".into()));
    m.insert("id".into(), Value::String(exec_id.to_owned()));
    m.insert("session_id".into(), Value::String(SESSION_ID.into()));
    m.insert("run_id".into(), Value::String(run_id.to_owned()));
    m.insert("run_name".into(), Value::String(run_id.to_owned()));
    m.insert("modification".into(), modification);
    m
}

/// The strict provider counterpart: a chat-id-keyed `Chats` map exactly like
/// the real provider's. Any `append`/`complete`/`delete` for a chat that was
/// never created (or is already deleted) is a hard failure — real-provider
/// semantics: "every completed chat_id must have been created".
#[derive(Default)]
struct StrictChats {
    live: HashSet<String>,
    /// Appended messages per chat, in arrival order.
    appended: HashMap<String, Vec<Value>>,
    /// Chat ids in creation order (round sequencing assertions).
    created_order: Vec<String>,
}

impl StrictChats {
    /// Route one wire event into the chat map; returns the chat_id when the
    /// event was a `chat.complete` (the harness answers those).
    fn observe(&mut self, kind: &str, body: &Map<String, Value>) -> Option<String> {
        let suffix = kind.strip_prefix(&format!("{PROVIDER}."))?;
        let chat_id = body
            .get("chat_id")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{kind} without chat_id"))
            .to_owned();
        match suffix {
            "chat.create" => {
                assert!(
                    self.live.insert(chat_id.clone()),
                    "chat.create for an already-live chat {chat_id}"
                );
                self.created_order.push(chat_id);
                None
            }
            "chat.append" => {
                assert!(
                    self.live.contains(&chat_id),
                    "chat.append for a chat never created (or deleted): {chat_id}"
                );
                let message = body.get("message").cloned().unwrap_or(Value::Null);
                self.appended.entry(chat_id).or_default().push(message);
                None
            }
            "chat.complete" => {
                assert!(
                    self.live.contains(&chat_id),
                    "chat.complete for a chat never created (or deleted): {chat_id}"
                );
                Some(chat_id)
            }
            "chat.delete" => {
                assert!(
                    self.live.remove(&chat_id),
                    "chat.delete for a chat never created: {chat_id}"
                );
                None
            }
            _ => None,
        }
    }
}

/// One agent loop (`llm → run-tool → tool-result → llm`) exiting to the
/// program sink.
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
                "messages": [ { "role": "user", "content": "list the repo" } ]
            } }
        ],
        "kills": [],
        "rules": []
    })
}

/// One llm exiting straight to the sink.
fn single_llm_modification() -> Value {
    json!({
        "actors": [
            {
                "id": "agent",
                "factory": "llm",
                "params": { "model": "opus", "provider": PROVIDER, "system": "answer" },
                "routes": { "generic-provider.FinalAnswer": ["sink"] }
            },
            { "id": "sink", "factory": "sink", "params": {}, "routes": {} }
        ],
        "messages": [
            { "to": "agent", "content": {
                "kind": "generic-provider.ProviderOut",
                "messages": [ { "role": "user", "content": "what is nefor" } ]
            } }
        ],
        "kills": [],
        "rules": []
    })
}

struct Plugin {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    data_dir: PathBuf,
}

async fn start_plugin(tag: &str) -> Plugin {
    let data_dir = std::env::temp_dir().join(format!("mag-{tag}-{}", std::process::id()));
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
    Plugin {
        child,
        stdin,
        reader,
        data_dir,
    }
}

async fn shutdown_plugin(mut plugin: Plugin) {
    write_env(
        &mut plugin.stdin,
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
    drop(plugin.stdin);
    let _ = timeout(Duration::from_secs(10), plugin.child.wait()).await;
    std::fs::remove_dir_all(&plugin.data_dir).ok();
}

/// Bug 1 pin: each activation is one round on a FRESH provider chat, so every
/// round must create its chat before completing it (strict chats enforce it)
/// and round 2's request must replay the WHOLE conversation — user turn,
/// assistant tool_calls (wire shape, arguments as a JSON string), tool result —
/// not just the newest tool message.
#[tokio::test]
async fn multi_round_replays_the_full_transcript_on_a_fresh_chat_per_round() {
    let mut plugin = start_plugin("llm-rounds-multi").await;

    send_event(
        &mut plugin.stdin,
        execute_body("exec-rounds", "rounds-run", agent_loop_modification()),
    )
    .await;

    let gate_invoke_kind = format!("{GATE}.tool.invoke");
    let mut chats = StrictChats::default();
    let mut round = 0u32;
    let run_result;

    loop {
        let out = read_outgoing(&mut plugin.reader, "round event").await;
        let body = match event_body(&out) {
            Some(b) => b.clone(),
            None => continue,
        };
        let kind = match body_kind(&body) {
            Some(k) => k.to_owned(),
            None => continue,
        };

        if let Some(chat_id) = chats.observe(&kind, &body) {
            round += 1;
            match round {
                1 => {
                    assert!(
                        chat_id.ends_with("@r1"),
                        "round 1 must run on @r1; got {chat_id}"
                    );
                    send_event(
                        &mut plugin.stdin,
                        chat_result(
                            &chat_id,
                            json!({
                                "text": "",
                                "finish_reason": "tool_calls",
                                "tool_calls": [
                                    { "id": "call-1", "name": "list_dir",
                                      "args": { "path": "." } }
                                ]
                            }),
                        ),
                    )
                    .await;
                }
                2 => {
                    assert!(
                        chat_id.ends_with("@r2"),
                        "round 2 must run on @r2 — one increment per activation; got {chat_id}"
                    );
                    // The replayed transcript, message by message. This is the
                    // request a real provider accepts; the pre-fix bridge sent
                    // only the tool message and the provider rejected it.
                    let msgs = chats
                        .appended
                        .get(&chat_id)
                        .unwrap_or_else(|| panic!("round 2 appended nothing to {chat_id}"));
                    assert_eq!(
                        msgs.len(),
                        3,
                        "round 2 replays [user, assistant(tool_calls), tool]; got {msgs:?}"
                    );
                    assert_eq!(msgs[0].get("role").and_then(Value::as_str), Some("user"));
                    assert_eq!(
                        msgs[0].get("content").and_then(Value::as_str),
                        Some("list the repo")
                    );
                    assert_eq!(
                        msgs[1].get("role").and_then(Value::as_str),
                        Some("assistant"),
                        "the model's tool-call turn is replayed"
                    );
                    let call = &msgs[1]["tool_calls"][0];
                    assert_eq!(call.get("id").and_then(Value::as_str), Some("call-1"));
                    assert_eq!(
                        call["function"].get("name").and_then(Value::as_str),
                        Some("list_dir")
                    );
                    assert_eq!(
                        call["function"].get("arguments").and_then(Value::as_str),
                        Some(r#"{"path":"."}"#),
                        "wire-shape arguments: a JSON string, not a decoded object"
                    );
                    assert_eq!(msgs[2].get("role").and_then(Value::as_str), Some("tool"));
                    assert_eq!(
                        msgs[2].get("tool_call_id").and_then(Value::as_str),
                        Some("call-1")
                    );
                    assert_eq!(
                        msgs[2].get("content").and_then(Value::as_str),
                        Some("dir-listing")
                    );
                    send_event(
                        &mut plugin.stdin,
                        chat_result(
                            &chat_id,
                            json!({ "text": "explored-and-answered", "finish_reason": "stop" }),
                        ),
                    )
                    .await;
                }
                n => panic!("unexpected provider round {n} (chat_id {chat_id})"),
            }
        } else if kind == gate_invoke_kind {
            let id = body
                .get("id")
                .and_then(Value::as_str)
                .expect("gate invoke keeps the correlation id")
                .to_owned();
            assert_eq!(body.get("name").and_then(Value::as_str), Some("list_dir"));
            send_event(&mut plugin.stdin, tool_result(&id, json!("dir-listing"))).await;
        } else if kind == "tool.invoke" {
            panic!("bare tool.invoke reached the wire");
        } else if kind == "mag.run_result" {
            run_result = body;
            break;
        }
    }

    assert_eq!(round, 2, "the run made exactly two provider rounds");
    assert_eq!(
        chats.created_order.len(),
        2,
        "each round created its own chat"
    );
    assert_eq!(
        run_result.get("status").and_then(Value::as_str),
        Some("completed")
    );
    let sink_path = run_result
        .get("output_path")
        .and_then(Value::as_str)
        .expect("run_result carries the sink output PATH");
    let sink_output = std::fs::read_to_string(sink_path).expect("read persisted sink output");
    assert!(
        sink_output.contains("explored-and-answered"),
        "the sink persisted the post-tool final answer; got {sink_output:?}"
    );

    shutdown_plugin(plugin).await;
}

/// Bug 2 pin: a provider round that dies (`finish_reason: "error"`) must FAIL
/// the run — `mag.run_failed` on the bus and a terminal `mag.run_result
/// status:"failed"` carrying the provider's detail — never route an empty
/// FinalAnswer to the sink and "complete".
#[tokio::test]
async fn provider_error_fails_the_run_with_the_detail_surfaced() {
    let mut plugin = start_plugin("llm-rounds-error").await;

    send_event(
        &mut plugin.stdin,
        execute_body("exec-error", "error-run", single_llm_modification()),
    )
    .await;

    let mut chats = StrictChats::default();
    let mut saw_run_failed_event: Option<Map<String, Value>> = None;
    let mut saw_run_complete_event = false;
    let run_result;

    loop {
        let out = read_outgoing(&mut plugin.reader, "error-run event").await;
        let body = match event_body(&out) {
            Some(b) => b.clone(),
            None => continue,
        };
        let kind = match body_kind(&body) {
            Some(k) => k.to_owned(),
            None => continue,
        };

        if let Some(chat_id) = chats.observe(&kind, &body) {
            // The provider's terminal result for a dead round: finish_reason
            // "error" plus the detail the provider surfaced (the chatgpt
            // provider threads its turn failure into complete.result).
            send_event(
                &mut plugin.stdin,
                chat_result(
                    &chat_id,
                    json!({
                        "text": "",
                        "finish_reason": "error",
                        "error": "HTTP 400: tool message without preceding tool_calls"
                    }),
                ),
            )
            .await;
        } else if kind == "mag.run_failed" {
            saw_run_failed_event = Some(body);
        } else if kind == "mag.run_complete" {
            saw_run_complete_event = true;
        } else if kind == "mag.run_result" {
            run_result = body;
            break;
        }
    }

    let failed_event = saw_run_failed_event.expect("a mag.run_failed event reached the bus");
    assert_eq!(
        failed_event.get("from").and_then(Value::as_str),
        Some("agent"),
        "the event names the failing actor"
    );
    let event_error = failed_event
        .get("error")
        .and_then(Value::as_str)
        .expect("the event carries the failure detail");
    assert!(
        event_error.contains("HTTP 400"),
        "the provider's detail is threaded through; got {event_error:?}"
    );

    assert!(
        !saw_run_complete_event,
        "the sink must never fire on a provider error"
    );
    assert_eq!(
        run_result.get("status").and_then(Value::as_str),
        Some("failed"),
        "the run fails instead of completing with an empty answer; got {run_result:?}"
    );
    assert_eq!(
        run_result.get("in_reply_to").and_then(Value::as_str),
        Some("exec-error")
    );
    let result_error = run_result
        .get("error")
        .and_then(Value::as_str)
        .expect("the terminal reply carries the failure detail");
    assert!(result_error.contains("HTTP 400"), "got {result_error:?}");
    assert!(
        run_result.get("output_path").is_none(),
        "a failed run has no sink output"
    );

    shutdown_plugin(plugin).await;
}

/// Bug 3 pin: each execute owns a fresh constellation. The first run's
/// leftovers are reaped at its terminal settle (run-context teardown —
/// `mag.actor_killed` for its still-live actors, stamped with ITS run_id),
/// and a second execute of the same program spawns fresh in its own run
/// context — `mag.actor_spawned` and `mag.actor_ready` reach the bus AGAIN —
/// with the fresh llm instance's rounds restarting at @r1. (Regression:
/// session 27c60892 re-executed a program whose ids were still alive in a
/// single global constellation: zero spawn/ready events, rounds continuing
/// at @r3. The global kill-sweep that first fixed it is now the per-run
/// context teardown.)
#[tokio::test]
async fn re_execute_respawns_a_fresh_constellation_with_lifecycle_events() {
    let mut plugin = start_plugin("llm-rounds-reexec").await;

    for (i, exec_id) in ["exec-first", "exec-second"].iter().enumerate() {
        send_event(
            &mut plugin.stdin,
            execute_body(exec_id, &format!("run-{i}"), single_llm_modification()),
        )
        .await;

        let mut chats = StrictChats::default();
        let mut spawned: HashSet<String> = HashSet::new();
        let mut ready: HashSet<String> = HashSet::new();
        let mut killed: HashSet<String> = HashSet::new();
        let run_result;

        loop {
            let out = read_outgoing(&mut plugin.reader, "re-execute event").await;
            let body = match event_body(&out) {
                Some(b) => b.clone(),
                None => continue,
            };
            let kind = match body_kind(&body) {
                Some(k) => k.to_owned(),
                None => continue,
            };

            if let Some(chat_id) = chats.observe(&kind, &body) {
                assert!(
                    chat_id.ends_with("@r1"),
                    "run {i}: a fresh llm instance restarts its rounds at @r1; got {chat_id}"
                );
                send_event(
                    &mut plugin.stdin,
                    chat_result(&chat_id, json!({ "text": format!("answer-{i}") })),
                )
                .await;
            } else if kind == "mag.actor_spawned" {
                if let Some(id) = body.get("id").and_then(Value::as_str) {
                    spawned.insert(id.to_owned());
                }
            } else if kind == "mag.actor_ready" {
                if let Some(id) = body.get("id").and_then(Value::as_str) {
                    ready.insert(id.to_owned());
                }
            } else if kind == "mag.actor_killed" {
                if let Some(id) = body.get("id").and_then(Value::as_str) {
                    killed.insert(id.to_owned());
                }
            } else if kind == "mag.run_result" {
                run_result = body;
                break;
            }
        }

        for id in ["agent", "sink"] {
            assert!(
                spawned.contains(id),
                "run {i}: mag.actor_spawned for '{id}' must reach the bus; saw {spawned:?}"
            );
            assert!(
                ready.contains(id),
                "run {i}: mag.actor_ready for '{id}' must reach the bus; saw {ready:?}"
            );
        }
        if i == 1 {
            for id in ["agent", "sink"] {
                assert!(
                    killed.contains(id),
                    "run {i}: the previous run's '{id}' is killed before respawn; saw {killed:?}"
                );
            }
        }
        assert_eq!(
            run_result.get("status").and_then(Value::as_str),
            Some("completed"),
            "run {i} completes"
        );
        assert_eq!(
            run_result.get("in_reply_to").and_then(Value::as_str),
            Some(*exec_id)
        );
    }

    shutdown_plugin(plugin).await;
}
