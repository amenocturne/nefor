//! MVP acceptance test for the mag actor-kernel runtime — deterministic and
//! rerunnable against the real plugin process.
//!
//! The fixture speaks both canonical capability protocols. Provider turns are
//! thin `conversation.provider.invoke.request` messages answered by correlated
//! manager-relayed `conversation.provider.event` observations. Tool calls
//! are rewritten onto `<gate>.tool.invoke` and answered with `tool.result`.
//! Every response is driven by observed request content and correlated only by
//! the request's opaque `request_id`; no provider-owned conversation state is
//! reconstructed or parsed.
//!
//! The graph: TWO agents (`a1.*`, `a2.*`) composed in one modification, each a
//! full provider loop (`llm → run-tool → tool-result → llm`),
//! with `a1.llm` declared as the structural result boundary. Each agent is seeded with a
//! `generic-provider.ProviderOut`, so both fire their `llm` turn straight off
//! the initial messages with no `adapter` factory needed (the shipped kernel registers
//! no `adapter`; the entry adapter of the design fixture is elided by seeding
//! the `llm` input directly).
//!
//! The six acceptance steps, each asserted below (see `SIX STEPS` markers):
//!   1. Two stdlib agents in one graph.
//!   2. Load → initial modification → constellation registers, actors
//!      construct + ready at their first firing.
//!   3. Both agents run their provider loops against the deterministic
//!      provider; every surviving-agent node output persists to its per-node
//!      file, typed per the declared contracts.
//!   4. One agent (`a2`) is killed mid-flight: its in-flight provider request
//!      aborts (the `<provider>.completion.cancel` envelope observably reaches
//!      the wire), its late terminal event is voided (no output file), and the
//!      other agent is unaffected.
//!   5. The structural result boundary receives the surviving agent's typed
//!      result (asserted inline and on the persisted actor output).
//!   6. The kernel speaks only its own wire vocabulary: no legacy `dag.*` /
//!      `graph.*` event appears on the wire.
//!
//! PLACEMENT (flagged). This runs at plugin level (spawn `mag-plugin`, drive
//! its stdio), the pattern of `execute.rs` — NOT engine-spawn (the
//! `agentic_cli_mock_e2e` engine-spawn tests are the repo's known flaky spot).
//! Step 6 guards against the kernel ever (re)growing the deleted
//! reasoner-graph kinds. The whole run is event-driven and settles in well
//! under a second.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::time::timeout;

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mag-plugin"))
}

fn kernel_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel/init.lua")
}

/// The provider capability name targeted by the `llm` actors.
const PROVIDER: &str = "chatgpt-provider";
/// The gate bus name threaded via `--tool-gate`, mirroring the starter
/// composition (examples/nefor-agent/init.lua). Tool invocations must reach the wire as
/// `<GATE>.tool.invoke`, never a bare `tool.invoke`.
const GATE: &str = "tool-gate";
const SESSION_ID: &str = "acceptance-session";
const RUN_NAME: &str = "acceptance-run";

async fn spawn_mag(data_dir: &std::path::Path) -> Child {
    let mut cmd = tokio::process::Command::new(binary_path());
    cmd.arg("--kernel")
        .arg(kernel_path())
        .arg("--tool-gate")
        .arg(GATE)
        .env("NEFOR_DATA_DIR", data_dir)
        .env("NEFOR_SESSIONS_DIR", data_dir.join("sessions"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn().expect("spawn mag-plugin")
}

const READ_TIMEOUT: Duration = Duration::from_secs(30);

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
    let kind = body.get("kind").and_then(Value::as_str);
    let source = if kind == Some("mag.execute") {
        PluginName::new("custom-runner").expect("custom runner plugin name")
    } else if kind == Some("conversation.provider.event") {
        PluginName::new("conversation-manager").expect("manager plugin name")
    } else {
        PluginName::engine()
    };
    write_env(stdin, Envelope::event(source, Timestamp::now(), body)).await;
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

/// Build the two-agent modification. Each agent `aN` is a full provider loop:
/// `aN.llm → aN.run-tool → aN.tool-result → aN.llm`. `a1.llm` is the declared
/// result on `generic-provider.FinalAnswer`. Both `llm`s are seeded with a
/// `generic-provider.ProviderOut` so they fire off the initial messages.
fn two_agent_modification() -> Value {
    fn named(name: &str) -> Value {
        json!({"kind":"named","name":name,"arguments":[]})
    }
    fn evidence(identity: &str, input: Value, output: Value) -> Value {
        json!({"version":2,"identity":identity,"arguments":[],"input":input,"output":output})
    }
    fn agent(prefix: &str) -> Vec<Value> {
        let provider_input = named("nefor.contracts.ProviderInput");
        let tool_calls = named("nefor.contracts.ToolCalls");
        let final_answer = named("nefor.contracts.FinalAnswer");
        let tool_handle = named("nefor.contracts.ToolHandle");
        vec![
            json!({
                "id": format!("{prefix}.llm"),
                "factory": "llm",
                "evidence": evidence("nefor.factory.llm",provider_input.clone(),json!({"kind":"union","items":[tool_calls.clone(),final_answer.clone()]})),
                "input":{"wire":"generic-provider.ProviderOut","type":provider_input.clone()},
                "outputs":[{"wire":"generic-tool.ToolCalls","type":tool_calls.clone()},{"wire":"generic-provider.FinalAnswer","type":final_answer}],
                "params": { "model": "opus", "provider": PROVIDER, "system": "work" },
                "routes": {
                    "generic-tool.ToolCalls": [{
                        "actor": format!("{prefix}.run-tool"),
                        "wire": "generic-tool.ToolCalls"
                    }],
                    "generic-provider.FinalAnswer": []
                }
            }),
            json!({
                "id": format!("{prefix}.run-tool"),
                "factory": "run-tool",
                "evidence": evidence("nefor.factory.run-tool",tool_calls.clone(),tool_handle.clone()),
                "input":{"wire":"generic-tool.ToolCalls","type":tool_calls},
                "outputs":[{"wire":"generic-tool.ToolHandle","type":tool_handle.clone()}],
                "params": {},
                "routes": { "generic-tool.ToolHandle": [{
                    "actor": format!("{prefix}.tool-result"),
                    "wire": "generic-tool.ToolHandle"
                }] }
            }),
            json!({
                "id": format!("{prefix}.tool-result"),
                "factory": "tool-result",
                "evidence": evidence("nefor.factory.tool-result",tool_handle.clone(),provider_input.clone()),
                "input":{"wire":"generic-tool.ToolHandle","type":tool_handle},
                "outputs":[{"wire":"generic-provider.ProviderOut","type":provider_input}],
                "params": {},
                "routes": { "generic-provider.ProviderOut": [{
                    "actor": format!("{prefix}.llm"),
                    "wire": "generic-provider.ProviderOut"
                }] }
            }),
        ]
    }

    let mut actors = agent("a1");
    actors.extend(agent("a2"));
    json!({
        "actors": actors,
        "messages": [
            { "to": "a1.llm", "content": { "kind": "generic-provider.ProviderOut", "messages": [{ "role": "user", "content": "go-a1" }] } },
            { "to": "a2.llm", "content": { "kind": "generic-provider.ProviderOut", "messages": [{ "role": "user", "content": "go-a2" }] } }
        ],
        "kills": [],
        "rules": [],
        "result": {
            "from": {
                "actor": "a1.llm",
                "type": "generic-provider.FinalAnswer",
                "wire": "generic-provider.FinalAnswer"
            }
        }
    })
}

/// A `tool.result` reply carrying `output` — the gate's reply shape
/// (plugins/tool-gate: broadcast `tool.result { id, output | error }` keyed by
/// the caller's id), which the plugin reads via main.rs handle_tool_result.
/// Answers each `<GATE>.tool.invoke` the bridge put on the wire.
fn tool_result(id: &str, output: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("tool.result".into()));
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("output".into(), output);
    m
}

fn completion_event(
    request_id: &str,
    event: &str,
    fields: Map<String, Value>,
) -> Map<String, Value> {
    let mut body = fields;
    body.insert(
        "kind".into(),
        Value::String("conversation.provider.event".into()),
    );
    body.insert("provider".into(), Value::String(PROVIDER.into()));
    body.insert("request_id".into(), Value::String(request_id.to_owned()));
    body.insert("event".into(), Value::String(event.to_owned()));
    body
}

fn completed(request_id: &str, fields: Value) -> Map<String, Value> {
    completion_event(
        request_id,
        "completed",
        fields
            .as_object()
            .expect("completion fields are an object")
            .clone(),
    )
}

#[tokio::test]
async fn two_agents_one_killed_mid_flight_the_other_completes() {
    let data_dir = std::env::temp_dir().join(format!("mag-acceptance-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).expect("mkdir data dir");

    let node_output = |node: &str| {
        data_dir
            .join("sessions")
            .join(SESSION_ID)
            .join("mag/runs")
            .join(RUN_NAME)
            .join(format!("{node}.output"))
    };

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

    // Handshake.
    let ready = read_outgoing(&mut reader, "system ready").await;
    assert!(matches!(ready.body, Body::System(SystemBody::Ready { .. })));
    write_env(
        &mut stdin,
        Envelope::system(
            PluginName::engine(),
            Timestamp::now(),
            SystemBody::ReadyOk {
                engine_version: "test".into(),
            },
        ),
    )
    .await;

    // Start the run. Both agents register at apply; both `llm`s construct and
    // fire their first provider turn inside `start` (lazy construction), so two
    // bridged provider conversations (one per agent) reach the wire before any
    // reply.
    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String("exec-accept".into()));
    execute.insert("session_id".into(), Value::String(SESSION_ID.into()));
    // A hand-authored/custom caller may declare anything, but the kernel
    // assigns it the explicit untrusted/no-notice principal.
    execute.insert("principal".into(), Value::String("lead".into()));
    execute.insert("run_id".into(), Value::String(RUN_NAME.into()));
    execute.insert("run_name".into(), Value::String(RUN_NAME.into()));
    execute.insert(
        "artifact".into(),
        json!({
            "format": "nefor.graph-modification/v1",
            "data": two_agent_modification()
        }),
    );
    send_event(&mut stdin, execute).await;

    // Deterministic event loop. Every action is a reply to an observed event;
    // nothing is timed.
    let mut seen: Vec<String> = Vec::new();
    let mut ready_ids: Vec<String> = Vec::new();
    let mut provider_agents: std::collections::BTreeSet<String> = Default::default();
    let mut killed_ids: Vec<String> = Vec::new();
    let mut cancelled_request_id: Option<String> = None;
    let mut a2_pending_request: Option<String> = None;
    let mut a2_kill_sent = false;
    let mut a2_void_sent = false;
    let request_kind = "conversation.provider.invoke.request";
    let mut provider_rounds = std::collections::HashMap::<String, usize>::new();
    let run_result;

    loop {
        let out = read_outgoing(&mut reader, "acceptance event").await;
        let body = match event_body(&out) {
            Some(b) => b.clone(),
            None => continue,
        };
        let kind = match body_kind(&body) {
            Some(k) => k.to_owned(),
            None => continue,
        };
        seen.push(kind.clone());

        match kind.as_str() {
            "mag.run_started" => {
                assert_eq!(
                    body.get("principal").and_then(Value::as_str),
                    Some("untrusted"),
                    "custom execute is explicitly ineligible for instruction notices"
                );
            }
            "mag.actor_ready" => {
                if let Some(id) = body.get("id").and_then(Value::as_str) {
                    ready_ids.push(id.to_owned());
                }
            }
            "mag.actor_killed" => {
                if let Some(id) = body.get("id").and_then(Value::as_str) {
                    killed_ids.push(id.to_owned());
                }
            }
            // Nothing subscribes to a bare `tool.invoke`: provider calls must
            // be bridged to completion.request, and tool calls to the gate.
            "tool.invoke" => {
                panic!(
                    "bare tool.invoke reached the wire (name {:?}) — every capability \
                     invoke must be bridged (provider → completion.request, tool → {GATE}.tool.invoke)",
                    body.get("name")
                );
            }
            // a1's tool call (from a1.run-tool), rewritten onto the gate's
            // contract with the payload unwrapped and the kernel correlation id
            // as the outer id. Answer like the gate does: a broadcast
            // `tool.result` keyed by that id.
            k if k == format!("{GATE}.tool.invoke") => {
                let name = body.get("name").and_then(Value::as_str).unwrap_or("");
                assert_eq!(name, "echo", "unexpected gated tool invoke: {name}");
                let req_id = body
                    .get("id")
                    .and_then(Value::as_str)
                    .expect("gate invoke carries the kernel correlation id")
                    .to_owned();
                assert_eq!(
                    body.get("invocation")
                        .and_then(|invocation| invocation.get("principal"))
                        .and_then(Value::as_str),
                    Some("untrusted"),
                    "direct execute capabilities cannot mint lead/subagent notices"
                );
                // The double-wrapped kernel payload was unwrapped: `args` is the
                // tool's own args, not `{ name, args, allowlist, da-policy }`.
                let args = body.get("args").and_then(Value::as_object).expect("args");
                assert_eq!(
                    args.get("text").and_then(Value::as_str),
                    Some("hi"),
                    "gate invoke carries the unwrapped tool args; got {args:?}"
                );
                assert!(
                    !args.contains_key("args"),
                    "gate invoke args must be unwrapped, not the kernel's double-wrap"
                );
                send_event(&mut stdin, tool_result(&req_id, json!("echo-ran"))).await;
            }
            // Provider requests carry their complete history. The fixture derives
            // the logical turn from that observed history, while treating the
            // request_id as an opaque correlation token.
            k if k == request_kind => {
                let request_id = body
                    .get("request_id")
                    .and_then(Value::as_str)
                    .expect("provider invoke carries request_id")
                    .to_owned();
                assert!(
                    body.get("messages").is_none(),
                    "thin invoke carries no transcript"
                );
                assert!(
                    body.get("system").is_none(),
                    "thin invoke carries no system prompt"
                );
                let actor = body
                    .get("invocation")
                    .and_then(|invocation| invocation.get("actor_id"))
                    .and_then(Value::as_str)
                    .expect("provider invocation identifies the actor")
                    .to_owned();
                let round = provider_rounds.entry(actor.clone()).or_default();
                *round += 1;

                match (actor.as_str(), *round) {
                    ("a1.llm", 1) => {
                        provider_agents.insert("a1".to_owned());
                        send_event(
                            &mut stdin,
                            completion_event(
                                &request_id,
                                "tool_call",
                                json!({ "id": "c1", "name": "echo", "arguments": { "text": "hi" } })
                                    .as_object()
                                    .expect("tool call fields")
                                    .clone(),
                            ),
                        )
                        .await;
                        send_event(
                            &mut stdin,
                            completed(
                                &request_id,
                                json!({ "text": "", "finish_reason": "tool_calls" }),
                            ),
                        )
                        .await;
                    }
                    ("a1.llm", 2) => {
                        send_event(
                            &mut stdin,
                            completion_event(
                                &request_id,
                                "text_delta",
                                json!({ "text": "final-" })
                                    .as_object()
                                    .expect("delta fields")
                                    .clone(),
                            ),
                        )
                        .await;
                        send_event(
                            &mut stdin,
                            completed(
                                &request_id,
                                json!({ "text": "final-a1", "finish_reason": "stop" }),
                            ),
                        )
                        .await;
                    }
                    ("a2.llm", 1) => {
                        provider_agents.insert("a2".to_owned());
                        a2_pending_request = Some(request_id);
                        if !a2_kill_sent {
                            a2_kill_sent = true;
                            let mut apply = Map::new();
                            apply.insert("kind".into(), Value::String("mag.apply".into()));
                            apply.insert("id".into(), Value::String("kill-a2".into()));
                            apply.insert(
                                "modification".into(),
                                json!({
                                    "actors": [],
                                    "messages": [],
                                    "kills": ["a2.llm", "a2.run-tool", "a2.tool-result"],
                                    "rules": []
                                }),
                            );
                            send_event(&mut stdin, apply).await;
                        }
                    }
                    other => panic!("unexpected provider invocation {other:?}: {body:?}"),
                }
            }
            "conversation.provider.cancel.request" => {
                let request_id = body
                    .get("request_id")
                    .and_then(Value::as_str)
                    .expect("completion.cancel carries request_id")
                    .to_owned();
                assert_eq!(
                    a2_pending_request.as_deref(),
                    Some(request_id.as_str()),
                    "cancel correlates exactly to a2's observed request_id"
                );
                cancelled_request_id = Some(request_id.clone());
                if !a2_void_sent {
                    a2_void_sent = true;
                    send_event(
                        &mut stdin,
                        completion_event(
                            &request_id,
                            "error",
                            json!({ "message": "interrupted" })
                                .as_object()
                                .expect("error fields")
                                .clone(),
                        ),
                    )
                    .await;
                }
            }
            "mag.run_result" => {
                run_result = body;
                break;
            }
            _ => {}
        }
    }

    // ── SIX STEPS #1: two agents in one graph. ──────────────────────────────
    assert!(
        ready_ids.iter().any(|id| id == "a1.llm") && ready_ids.iter().any(|id| id == "a2.llm"),
        "both agents' llm actors readied; saw {ready_ids:?}"
    );
    assert_eq!(
        provider_agents.iter().cloned().collect::<Vec<_>>(),
        vec!["a1".to_owned(), "a2".to_owned()],
        "both agents ran their loop and issued a provider request"
    );

    // ── SIX STEPS #2: load/start lifecycle events reached the wire. ─────────
    for expected in [
        "mag.run_started",
        "mag.actor_ready",
        "mag.modification_applied",
    ] {
        assert!(
            seen.iter().any(|k| k == expected),
            "lifecycle event {expected} on the wire; saw {seen:?}"
        );
    }

    // ── SIX STEPS #3: the survivor's every node output persisted, typed per
    //    the declared contracts (llm ToolCalls/FinalAnswer, run-tool ToolHandle,
    //    tool-result ProviderInput). ─────────────────────────────────────────────
    for node in ["a1.llm", "a1.run-tool", "a1.tool-result"] {
        assert!(
            node_output(node).exists(),
            "per-node output persisted for {node} at {:?}",
            node_output(node)
        );
    }

    // ── SIX STEPS #4: a2 killed mid-flight; cancellation correlated by the
    //    exact request_id; late terminal error voided (no a2 node output). ───
    let cancelled = cancelled_request_id.as_deref().unwrap_or_else(|| {
        panic!("the in-flight provider request's cancellation reached the wire; saw {seen:?}")
    });
    assert_eq!(
        Some(cancelled),
        a2_pending_request.as_deref(),
        "completion.cancel preserves the observed provider request_id"
    );
    assert!(
        killed_ids.iter().any(|id| id == "a2.llm"),
        "a2.llm killed (mag.actor_killed); saw {killed_ids:?}"
    );
    assert!(
        a2_void_sent,
        "a2's late provider reply was delivered to be voided"
    );
    for node in ["a2.llm", "a2.run-tool", "a2.tool-result"] {
        assert!(
            !node_output(node).exists(),
            "killed agent produced no output — {node} is voided"
        );
    }

    // ── SIX STEPS #5: the result boundary received the survivor's result. ──
    assert_eq!(
        run_result.get("status").and_then(Value::as_str),
        Some("completed"),
        "the run completed on the surviving agent"
    );
    assert_eq!(
        run_result.get("in_reply_to").and_then(Value::as_str),
        Some("exec-accept"),
        "run_result correlates to the execute request"
    );
    assert_eq!(
        run_result
            .get("result")
            .and_then(|result| result.get("text"))
            .and_then(Value::as_str),
        Some("final-a1"),
        "run_result carries the surviving agent's result inline"
    );
    let result_path = run_result
        .get("output_path")
        .and_then(Value::as_str)
        .expect("run_result carries the result actor output PATH");
    let result_output =
        std::fs::read_to_string(result_path).expect("read persisted result actor output");
    assert!(
        result_output.contains("final-a1"),
        "result actor persisted the surviving agent's final answer; got {result_output:?}"
    );

    // ── SIX STEPS #6: only the kernel's own wire vocabulary appears — the
    //    legacy dag.* / graph.* kinds died with the reasoner-graph plugin and
    //    must not regrow. ──────────────────────────────────────────────────
    let legacy_events: Vec<&String> = seen
        .iter()
        .filter(|k| k.starts_with("dag.") || k.starts_with("graph."))
        .collect();
    assert!(
        legacy_events.is_empty(),
        "legacy dag.*/graph.* kinds must not appear; saw {legacy_events:?}"
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

fn git<const N: usize>(cwd: &std::path::Path, args: [&str; N]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git executes");
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_owned()
}

fn worktree_program(operation: &str, repository: &str, path: &str, branch: &str) -> String {
    let constructor = match operation {
        "create" => format!(
            "(nefor.worktree.create \"workspace\" (as nefor.worktree.CreateSpec {{:repository {repository:?} :path {path:?} :branch {branch:?} :base \"main\"}}))"
        ),
        "open" => format!(
            "(nefor.worktree.open \"workspace\" (as nefor.worktree.OpenSpec {{:repository {repository:?} :path {path:?} :branch {branch:?}}}))"
        ),
        _ => panic!("unsupported worktree operation {operation}"),
    };
    format!(
        r#"(require "nefor.artifact")
(require "nefor.graph")
(require "nefor.worktree")
(let [start (nefor.graph.source "start" (type-tag Unit) nil)
      workspace {constructor}
      result (nefor.graph.output-for "result" workspace)]
  (nefor.artifact.compile
    (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph
      (nefor.graph.add-edges graph
        [(nefor.graph.edge start workspace)
         (nefor.graph.edge workspace result)]))))"#
    )
}

async fn load_worktree_program<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    stdin: &mut ChildStdin,
    id: &str,
    source_dir: &std::path::Path,
    entry: &str,
) -> Value {
    let lib_root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/nefor-agent/mag/lib");
    send_event(
        stdin,
        json!({
            "kind": "mag.load",
            "id": id,
            "source_dir": source_dir,
            "entry": entry,
            "module_roots": [lib_root],
        })
        .as_object()
        .expect("load body")
        .clone(),
    )
    .await;
    loop {
        let outgoing = read_outgoing(reader, id).await;
        let Some(body) = event_body(&outgoing) else {
            continue;
        };
        if body.get("in_reply_to").and_then(Value::as_str) == Some(id) {
            assert_eq!(
                body_kind(body),
                Some("mag.loaded"),
                "worktree program must load: {body:#?}"
            );
            return body.get("artifact").expect("loaded artifact").clone();
        }
    }
}

async fn execute_worktree_program<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    stdin: &mut ChildStdin,
    id: &str,
    artifact: Value,
) -> Map<String, Value> {
    send_event(
        stdin,
        json!({
            "kind": "mag.execute",
            "id": id,
            "session_id": "worktree-acceptance",
            "principal": "lead",
            "conversation_id": "acceptance-conversation",
            "run_id": id,
            "run_name": id,
            "artifact": artifact,
        })
        .as_object()
        .expect("execute body")
        .clone(),
    )
    .await;

    loop {
        let outgoing = read_outgoing(reader, id).await;
        let Some(body) = event_body(&outgoing) else {
            continue;
        };
        match body_kind(body) {
            Some("tool-gate.tool.invoke") => {
                let request_id = body.get("id").and_then(Value::as_str).expect("tool id");
                let name = body.get("name").and_then(Value::as_str).expect("tool name");
                let args = body.get("args").cloned().expect("tool args");
                let output = match name {
                    git_worktree_plugin::CREATE_TOOL => {
                        let spec = serde_json::from_value(args).expect("create spec");
                        match git_worktree_plugin::create(&spec) {
                            Ok(worktree) => json!({"ok": true, "worktree": worktree}),
                            Err(error) => json!({"ok": false, "error": error}),
                        }
                    }
                    git_worktree_plugin::OPEN_TOOL => {
                        let spec = serde_json::from_value(args).expect("open spec");
                        match git_worktree_plugin::open(&spec) {
                            Ok(worktree) => json!({"ok": true, "worktree": worktree}),
                            Err(error) => json!({"ok": false, "error": error}),
                        }
                    }
                    other => panic!("unexpected worktree tool {other}"),
                };
                send_event(stdin, tool_result(request_id, output)).await;
            }
            Some("mag.run_result")
                if body.get("in_reply_to").and_then(Value::as_str) == Some(id) =>
            {
                return body.clone();
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn worktree_create_persists_opens_and_rejects_implicit_reuse() {
    let root = std::env::temp_dir().join(format!("mag-worktree-acceptance-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    let repository = root.join("repo");
    let sources = root.join("sources");
    let data_dir = root.join("data");
    let worktree = root.join("worktrees/topic");
    std::fs::create_dir_all(&repository).expect("repository directory");
    std::fs::create_dir_all(&sources).expect("source directory");
    std::fs::create_dir_all(worktree.parent().expect("worktree parent")).expect("worktree parent");
    std::fs::create_dir_all(&data_dir).expect("data directory");
    git(&repository, ["init", "-b", "main"]);
    git(&repository, ["config", "user.email", "test@example.com"]);
    git(&repository, ["config", "user.name", "Test"]);
    std::fs::write(repository.join("README.md"), "base\n").expect("seed repository");
    git(&repository, ["add", "README.md"]);
    git(&repository, ["commit", "-m", "base"]);

    let repository_text = repository.to_string_lossy();
    let worktree_text = worktree.to_string_lossy();
    std::fs::write(
        sources.join("create.mag"),
        worktree_program("create", &repository_text, &worktree_text, "topic"),
    )
    .expect("create program");
    std::fs::write(
        sources.join("open.mag"),
        worktree_program("open", &repository_text, &worktree_text, "topic"),
    )
    .expect("open program");

    let mut child = spawn_mag(&data_dir).await;
    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    let ready = read_outgoing(&mut reader, "system ready").await;
    assert!(matches!(ready.body, Body::System(SystemBody::Ready { .. })));
    write_env(
        &mut stdin,
        Envelope::system(
            PluginName::engine(),
            Timestamp::now(),
            SystemBody::ReadyOk {
                engine_version: "test".into(),
            },
        ),
    )
    .await;

    let create_artifact = load_worktree_program(
        &mut reader,
        &mut stdin,
        "load-create",
        &sources,
        "create.mag",
    )
    .await;
    let created = execute_worktree_program(
        &mut reader,
        &mut stdin,
        "execute-create",
        create_artifact.clone(),
    )
    .await;
    assert_eq!(
        created.get("status").and_then(Value::as_str),
        Some("completed")
    );
    let canonical_worktree = std::fs::canonicalize(&worktree).expect("canonical worktree");
    assert_eq!(
        created
            .get("result")
            .and_then(|result| result.get("value"))
            .and_then(|result| result.get("path"))
            .and_then(Value::as_str),
        Some(canonical_worktree.to_string_lossy().as_ref())
    );
    assert!(worktree.is_dir(), "worktree persists after run completion");

    std::fs::write(worktree.join("dirty.txt"), "in progress\n").expect("dirty worktree");
    let open_artifact =
        load_worktree_program(&mut reader, &mut stdin, "load-open", &sources, "open.mag").await;
    let opened =
        execute_worktree_program(&mut reader, &mut stdin, "execute-open", open_artifact).await;
    assert_eq!(
        opened.get("status").and_then(Value::as_str),
        Some("completed")
    );
    let opened_head = opened
        .get("result")
        .and_then(|result| result.get("value"))
        .and_then(|worktree| worktree.get("head"))
        .and_then(Value::as_str);
    let current_head = git(&worktree, ["rev-parse", "HEAD"]);
    assert_eq!(opened_head, Some(current_head.as_str()));
    assert!(
        worktree.join("dirty.txt").exists(),
        "open preserves dirty state"
    );

    let collision = execute_worktree_program(
        &mut reader,
        &mut stdin,
        "execute-create-again",
        create_artifact,
    )
    .await;
    assert_eq!(
        collision.get("status").and_then(Value::as_str),
        Some("failed")
    );
    assert!(
        collision
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error.contains("worktree path already exists")),
        "collision failure must explain that implicit reuse was rejected: {collision:#?}"
    );
    assert!(
        worktree.is_dir(),
        "failed create does not remove the existing worktree"
    );

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
    std::fs::remove_dir_all(root).ok();
}
