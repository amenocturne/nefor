//! Nested mag-eval recursion e2e: an agent RUNNING ON THE KERNEL calls the
//! `mag-eval` tool, whose handler dispatches a SECOND concurrent run on the
//! same kernel; the nested run completes and its terminal result settles the
//! outer agent's blocked tool firing, after which the outer run finishes.
//!
//! The harness plays both the tool gate and the lead-workflow `mag-eval`
//! handler (starter/lead-workflow/mag-eval.lua):
//!
//!   * the outer agent's provider turn returns a `mag-eval` tool call; the
//!     run-tool leg surfaces it as a `tool-gate.tool.invoke {name:"mag-eval"}`
//!     exactly like any shell invocation — the firing stays open;
//!   * the harness answers the way mag-eval.lua does: `mag.load` for the
//!     expression source, then on `mag.loaded` a second `mag.execute` with
//!     its own run_id and the SAME session_id (the invariant that keeps
//!     begin_run from reaping the still-live outer run at a session boundary);
//!   * the nested run's bash node fires its own gate invoke under its own run
//!     scope — the correlation ids never collide with the outer firing's;
//!   * the nested `mag.run_result` relays back as the outer firing's
//!     `tool.result`, unblocking the outer agent into its final provider turn.
//!
//! Pins: the nested run settles first, the outer run settles only after the
//! relay, and the outer run is never reaped or failed while the nested run is
//! in flight.

use std::collections::HashMap;
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
const SESSION_ID: &str = "nested-eval-session";
const OUTER_RUN: &str = "outer-run";
const EVAL_RUN: &str = "eval-run";
const READ_TIMEOUT: Duration = Duration::from_secs(30);

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mag-plugin"))
}

fn kernel_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../starter/mag-kernel/init.lua")
}

async fn spawn_mag(data_dir: &std::path::Path) -> Child {
    let mut cmd = tokio::process::Command::new(binary_path());
    cmd.arg("--kernel").arg(kernel_path());
    cmd.env("NEFOR_DATA_DIR", data_dir)
        .env_remove("NEFOR_DEV_DIR")
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

/// The OUTER program: one agent loop (`llm → run-tool → tool-result → llm`)
/// exiting to the program sink — the shape a kernel sub-agent runs as.
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
                "messages": [ { "role": "user", "content": "evaluate hi" } ]
            } }
        ],
        "kills": [],
        "rules": []
    })
}

/// Wire recorder: per-run lifecycle events plus settled run results in
/// arrival order — the evidence for the never-reaped and settle-ordering
/// assertions.
#[derive(Default)]
struct Wire {
    /// run_id → lifecycle event kinds, in arrival order.
    events: HashMap<String, Vec<String>>,
    run_results: HashMap<String, Map<String, Value>>,
    /// run_ids in the order their `mag.run_result` settled.
    settle_order: Vec<String>,
}

enum Step {
    /// A `chat.complete` for this chat_id awaits a provider answer.
    ProviderTurn(String),
    /// A gate invoke awaits a tool.result; the full body for field asserts.
    GateInvoke(Map<String, Value>),
    /// The compile reply carrying the lowered modification.
    Loaded(Map<String, Value>),
    /// A `mag.run_result` settled for this run_id.
    RunResult(String),
    Other,
}

impl Wire {
    fn observe(&mut self, body: &Map<String, Value>) -> Step {
        let kind = match body.get("kind").and_then(Value::as_str) {
            Some(k) => k.to_owned(),
            None => return Step::Other,
        };
        if kind == "tool.invoke" {
            panic!("bare tool.invoke reached the wire: {body:?}");
        }
        if kind == format!("{GATE}.tool.invoke") {
            return Step::GateInvoke(body.clone());
        }
        if kind == "mag.loaded" {
            return Step::Loaded(body.clone());
        }
        if kind == format!("{PROVIDER}.chat.complete") {
            let chat_id = body
                .get("chat_id")
                .and_then(Value::as_str)
                .expect("chat.complete carries chat_id")
                .to_owned();
            return Step::ProviderTurn(chat_id);
        }
        // Every kernel lifecycle event and the terminal reply carry their
        // run_id (docs/ir.md, Run contexts) — record them per run.
        if kind.starts_with("mag.") {
            if let Some(run_id) = body.get("run_id").and_then(Value::as_str) {
                self.events
                    .entry(run_id.to_owned())
                    .or_default()
                    .push(kind.clone());
                if kind == "mag.run_result" {
                    self.run_results.insert(run_id.to_owned(), body.clone());
                    self.settle_order.push(run_id.to_owned());
                    return Step::RunResult(run_id.to_owned());
                }
            }
        }
        Step::Other
    }

    /// A run is live iff it has settled no terminal reply and lost no actor
    /// to a reap.
    fn assert_live(&self, run_id: &str, when: &str) {
        assert!(
            !self.run_results.contains_key(run_id),
            "{run_id} settled prematurely ({when})"
        );
        let events = self.events.get(run_id).map(Vec::as_slice).unwrap_or(&[]);
        assert!(
            !events
                .iter()
                .any(|k| k == "mag.actor_killed" || k == "mag.run_failed"),
            "{run_id} was reaped or failed while it should be live ({when}); saw {events:?}"
        );
    }
}

/// The scope prefix of a run-scoped wire id (`r2/cap-1` → `r2/`).
fn scope_of(id: &str) -> &str {
    let idx = id
        .find('/')
        .expect("run-scoped wire ids carry a scope prefix");
    &id[..=idx]
}

#[tokio::test]
async fn kernel_agent_evaluates_a_nested_mag_run_and_both_settle() {
    let base = std::env::temp_dir().join(format!("mag-nested-eval-{}", std::process::id()));
    let data_dir = base.join("data");
    let source_dir = base.join("src");
    std::fs::remove_dir_all(&base).ok();
    std::fs::create_dir_all(&data_dir).expect("mkdir data");
    std::fs::create_dir_all(&source_dir).expect("mkdir src");

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

    // Read (recording every event) until the wanted step surfaces.
    macro_rules! next_step {
        ($pat:pat => $out:expr, $expecting:expr) => {{
            loop {
                let out = read_outgoing(&mut reader, $expecting).await;
                let body = match event_body(&out) {
                    Some(b) => b.clone(),
                    None => continue,
                };
                if let $pat = wire.observe(&body) {
                    break $out;
                }
            }
        }};
    }

    // ── Launch the OUTER agent run; its llm fires its round-1 turn. ─────────
    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String("exec-outer".into()));
    execute.insert("session_id".into(), Value::String(SESSION_ID.into()));
    execute.insert("run_id".into(), Value::String(OUTER_RUN.into()));
    execute.insert("run_name".into(), Value::String(OUTER_RUN.into()));
    execute.insert("modification".into(), agent_loop_modification());
    send_event(&mut stdin, execute).await;

    let outer_chat_1 = next_step!(Step::ProviderTurn(c) => c, "outer round-1 turn");

    // Turn 1: the model calls mag-eval. This drives run-tool into the gate
    // invoke that a real deployment's mag-eval handler answers.
    send_event(
        &mut stdin,
        chat_result(
            &outer_chat_1,
            json!({ "tool_calls": [
                { "id": "call-1", "name": "mag-eval",
                  "args": { "expr": "(bash \"echo hi\")" } }
            ] }),
        ),
    )
    .await;

    let eval_invoke = next_step!(Step::GateInvoke(b) => b, "mag-eval gate invoke");
    assert_eq!(
        eval_invoke.get("name").and_then(Value::as_str),
        Some("mag-eval"),
        "the sub-agent's eval call is gate-visible like any tool"
    );
    assert_eq!(
        eval_invoke.get("from").and_then(Value::as_str),
        Some("agent.run-tool")
    );
    assert_eq!(
        eval_invoke.get("args"),
        Some(&json!({ "expr": "(bash \"echo hi\")" }))
    );
    let outer_cap_id = eval_invoke
        .get("id")
        .and_then(Value::as_str)
        .expect("gate invoke carries the kernel correlation id")
        .to_owned();

    // ── Play mag-eval.lua: compile the expression, then execute it as a
    // SECOND run on the same kernel — same session, distinct run_id. ─────────
    std::fs::write(source_dir.join("eval.mag"), r#"(bash "echo hi")"#)
        .expect("write expression source");
    let mut load = Map::new();
    load.insert("kind".into(), Value::String("mag.load".into()));
    load.insert("id".into(), Value::String("eval-load".into()));
    load.insert(
        "source_dir".into(),
        Value::String(source_dir.display().to_string()),
    );
    load.insert("entry".into(), Value::String("eval.mag".into()));
    send_event(&mut stdin, load).await;

    let loaded = next_step!(Step::Loaded(b) => b, "mag.loaded reply");
    assert_eq!(
        loaded.get("in_reply_to").and_then(Value::as_str),
        Some("eval-load")
    );
    let modification = loaded
        .get("modification")
        .cloned()
        .expect("mag.loaded carries the lowered modification");

    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String(EVAL_RUN.into()));
    execute.insert("session_id".into(), Value::String(SESSION_ID.into()));
    execute.insert("run_id".into(), Value::String(EVAL_RUN.into()));
    execute.insert("run_name".into(), Value::String(EVAL_RUN.into()));
    execute.insert("modification".into(), modification);
    send_event(&mut stdin, execute).await;

    // The nested run's bash node fires through the gate under its own scope.
    let bash_invoke = next_step!(Step::GateInvoke(b) => b, "nested bash invoke");
    assert_eq!(
        bash_invoke.get("name").and_then(Value::as_str),
        Some("bash")
    );
    assert_eq!(
        bash_invoke.get("from").and_then(Value::as_str),
        Some("bash-1")
    );
    assert_eq!(
        bash_invoke
            .get("args")
            .and_then(|a| a.get("command"))
            .and_then(Value::as_str),
        Some("echo hi")
    );
    let bash_cap_id = bash_invoke
        .get("id")
        .and_then(Value::as_str)
        .expect("bash invoke carries a correlation id")
        .to_owned();
    assert_ne!(
        scope_of(&bash_cap_id),
        scope_of(&outer_cap_id),
        "the nested run's correlation ids live in their own run scope — its \
         tool.result cannot settle the outer firing"
    );
    // The same-session execute reset nothing: the outer run — with its open
    // tool firing — is still fully live while the nested run works.
    wire.assert_live(OUTER_RUN, "after the nested run started");
    let outer_events = wire.events.get(OUTER_RUN).expect("outer run events");
    assert_eq!(
        outer_events
            .iter()
            .filter(|k| *k == "mag.actor_spawned")
            .count(),
        4,
        "the outer constellation is intact; saw {outer_events:?}"
    );

    send_event(&mut stdin, tool_result(&bash_cap_id, json!("hi\n[exit 0]"))).await;

    // ── The nested run settles FIRST, its terminal text the echo output. ────
    let settled = next_step!(Step::RunResult(r) => r, "nested run result");
    assert_eq!(settled, EVAL_RUN, "the nested run settles before the outer");
    let eval_result = wire.run_results[EVAL_RUN].clone();
    assert_eq!(
        eval_result.get("in_reply_to").and_then(Value::as_str),
        Some(EVAL_RUN)
    );
    assert_eq!(
        eval_result.get("status").and_then(Value::as_str),
        Some("completed"),
        "the nested run completed independently; got {eval_result:?}"
    );
    let eval_text = eval_result
        .get("result")
        .and_then(|r| r.get("text"))
        .and_then(Value::as_str)
        .expect("the nested terminal reply carries the inline text");
    assert_eq!(eval_text, "hi\n");
    wire.assert_live(OUTER_RUN, "when the nested run settled");

    // ── Relay the nested result as the OUTER tool result (mag-eval.lua's
    // on_run_result), unblocking the outer agent's firing. ───────────────────
    send_event(&mut stdin, tool_result(&outer_cap_id, json!(eval_text))).await;

    let outer_chat_2 = next_step!(Step::ProviderTurn(c) => c, "outer round-2 turn");
    assert_eq!(
        scope_of(&outer_chat_2),
        scope_of(&outer_chat_1),
        "the post-eval turn stays in the outer run's scope"
    );
    send_event(
        &mut stdin,
        chat_result(&outer_chat_2, json!({ "text": "evaluated-hi" })),
    )
    .await;

    // ── The outer run settles LAST, its final answer post-eval. ─────────────
    let settled = next_step!(Step::RunResult(r) => r, "outer run result");
    assert_eq!(settled, OUTER_RUN);
    assert_eq!(
        wire.settle_order,
        vec![EVAL_RUN.to_owned(), OUTER_RUN.to_owned()],
        "the outer run settles only after the nested run's result was relayed"
    );
    let outer_result = &wire.run_results[OUTER_RUN];
    assert_eq!(
        outer_result.get("in_reply_to").and_then(Value::as_str),
        Some("exec-outer")
    );
    assert_eq!(
        outer_result.get("status").and_then(Value::as_str),
        Some("completed"),
        "the outer run completed after the nested round-trip; got {outer_result:?}"
    );
    let sink_path = outer_result
        .get("output_path")
        .and_then(Value::as_str)
        .expect("the outer run_result carries the sink output path");
    let sink_output = std::fs::read_to_string(sink_path).expect("read persisted sink output");
    assert!(
        sink_output.contains("evaluated-hi"),
        "the sink persisted the post-eval final answer; got {sink_output:?}"
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
    std::fs::remove_dir_all(&base).ok();
}
