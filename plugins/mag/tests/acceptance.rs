//! MVP acceptance test for the mag actor-kernel runtime — the definition of
//! done, in code, deterministic and rerunnable.
//!
//! It spawns the real `mag-plugin`, drives its stdio, and acts as the
//! deterministic provider/tool capability itself. Crucially it speaks the REAL
//! protocols on both capability legs: the mag plugin's capability bridge drives
//! each provider turn as `chat.create` → `chat.append` → `chat.complete` (NOT a
//! bare `tool.invoke`), answered here with a `chat.complete.result`; and it
//! rewrites each tool invocation onto the gate's `<gate>.tool.invoke` contract
//! (unwrapped `{ id, name, args }` — NOT the kernel's bare double-wrapped
//! `tool.invoke`, which nothing on the bus subscribes to), answered here with
//! the gate's broadcast `tool.result { id, output }`. Scripted bare-`tool.invoke`
//! responders are exactly how both bridge gaps survived earlier tests. No
//! wall-clock synchronization — every step is driven off an observed event.
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
//!      aborts (the `<provider>.chat.cancel` envelope observably reaches the
//!      wire), its late provider reply is voided (no output file), and the
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
use std::process::Stdio;
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

/// The provider capability name the `llm` actors target — the shipped fixture's
/// provider, so the abort envelope is the real `chatgpt-provider.chat.cancel`.
const PROVIDER: &str = "chatgpt-provider";
/// The gate bus name threaded via `--tool-gate`, mirroring the starter
/// composition (starter/init.lua). Tool invocations must reach the wire as
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
                    "generic-tool.ToolCalls": [format!("{prefix}.run-tool")],
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
                "routes": { "generic-tool.ToolHandle": [format!("{prefix}.tool-result")] }
            }),
            json!({
                "id": format!("{prefix}.tool-result"),
                "factory": "tool-result",
                "evidence": evidence("nefor.factory.tool-result",tool_handle.clone(),provider_input.clone()),
                "input":{"wire":"generic-tool.ToolHandle","type":tool_handle},
                "outputs":[{"wire":"generic-provider.ProviderOut","type":provider_input}],
                "params": {},
                "routes": { "generic-provider.ProviderOut": [format!("{prefix}.llm")] }
            }),
        ]
    }

    let mut actors = agent("a1");
    actors.extend(agent("a2"));
    json!({
        "actors": actors,
        "messages": [
            { "to": "a1.llm", "content": { "kind": "generic-provider.ProviderOut", "messages": [{ "role": "user", "content": "go" }] } },
            { "to": "a2.llm", "content": { "kind": "generic-provider.ProviderOut", "messages": [{ "role": "user", "content": "go" }] } }
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

/// `("r1/a1.llm@r2")` → `("a1", 2)`. Wire chat handles are run-scoped by the
/// kernel (`r<K>/<actor>@r<seq>` — plugins/mag/lua/mag-kernel/init.lua bus_emit); the
/// test peels the scope prefix off the node segment only to script per-agent
/// behavior — real consumers never parse a chat_id.
fn parse_chat_id(chat_id: &str) -> (String, u32) {
    let idx = chat_id.find("@r").expect("chat_id carries @r turn suffix");
    let node = &chat_id[..idx];
    let node = node.split('/').next_back().unwrap_or(node);
    let turn: u32 = chat_id[idx + 2..]
        .parse()
        .expect("chat_id turn is a number");
    let agent = node.split('.').next().unwrap_or(node).to_owned();
    (agent, turn)
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

/// A provider `chat.complete.result` reply — the REAL provider protocol shape
/// (starter/mock-provider: `<name>.chat.complete.result { chat_id, output }`),
/// correlated back to the driving `llm` actor by chat_id through the mag plugin's
/// provider bridge. Provider turns are answered with this, NOT a bare
/// `tool.result` — the exact protocol a scripted `tool.invoke` responder skipped.
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
    let mut cancel_chat_id: Option<String> = None;
    let mut a2_pending_chat: Option<String> = None;
    let mut a2_kill_sent = false;
    let mut a2_void_sent = false;
    let complete_kind = format!("{PROVIDER}.chat.complete");
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
            // Nothing on the bus subscribes to a bare `tool.invoke`: a provider
            // request must be bridged to chat.*, a tool invocation onto the
            // gate's `<GATE>.tool.invoke`. Any bare invoke is a bridge regression
            // (a run would hang at its first capability call).
            "tool.invoke" => {
                panic!(
                    "bare tool.invoke reached the wire (name {:?}) — every capability \
                     invoke must be bridged (provider → chat.*, tool → {GATE}.tool.invoke)",
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
            // A driven provider turn: the bridge finished `chat.create`/`.append`
            // and asks the provider to complete. Answer with a `chat.complete.result`
            // keyed by the same chat_id (the REAL protocol).
            k if k == complete_kind => {
                let chat_id = body
                    .get("chat_id")
                    .and_then(Value::as_str)
                    .expect("chat.complete carries chat_id")
                    .to_owned();
                let (agent, turn) = parse_chat_id(&chat_id);
                provider_agents.insert(agent.clone());
                match (agent.as_str(), turn) {
                    // a1 turn 1 → tool calls; drives the loop one turn.
                    ("a1", 1) => {
                        send_event(
                            &mut stdin,
                            chat_result(
                                &chat_id,
                                json!({ "tool_calls": [{ "id": "c1", "name": "echo", "args": { "text": "hi" } }] }),
                            ),
                        )
                        .await;
                    }
                    // a1 turn 2 → final answer; reaches the structural result boundary.
                    ("a1", 2) => {
                        send_event(
                            &mut stdin,
                            chat_result(&chat_id, json!({ "text": "final-a1" })),
                        )
                        .await;
                    }
                    // a2 turn 1 → the doomed request. Withhold the reply and kill
                    // a2 mid-completion (SIX STEPS #4). Record the chat_id so its
                    // late reply can be delivered and asserted voided.
                    ("a2", 1) => {
                        a2_pending_chat = Some(chat_id.clone());
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
                    other => panic!("unexpected provider turn {other:?} (chat_id {chat_id})"),
                }
            }
            k if k == format!("{PROVIDER}.chat.cancel") => {
                // SIX STEPS #4: the in-flight provider request aborts — the
                // cancel envelope reaches the wire, keyed by a2's chat_id.
                let chat_id = body
                    .get("chat_id")
                    .and_then(Value::as_str)
                    .expect("cancel envelope carries chat_id")
                    .to_owned();
                cancel_chat_id = Some(chat_id);
                // Now deliver a2's withheld provider reply on the same chat: it
                // must be voided (the requester is dead, its correlation dropped),
                // so the bridge's bus_response lands on no open correlation.
                if !a2_void_sent {
                    a2_void_sent = true;
                    let chat = a2_pending_chat.clone().expect("a2 chat recorded");
                    send_event(
                        &mut stdin,
                        chat_result(&chat, json!({ "text": "late-a2-should-be-voided" })),
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
    //    tool-result ProviderOut). ─────────────────────────────────────────────
    for node in ["a1.llm", "a1.run-tool", "a1.tool-result"] {
        assert!(
            node_output(node).exists(),
            "per-node output persisted for {node} at {:?}",
            node_output(node)
        );
    }

    // ── SIX STEPS #4: a2 killed mid-flight; abort envelope on the wire; late
    //    reply voided (no a2 node output). The wire handle is the kernel's
    //    run-scoped form (`r<K>/a2.llm@r1`). ──────────────────────────────────
    let cancel = cancel_chat_id
        .as_deref()
        .expect("the in-flight provider request's cancel envelope reached the wire");
    assert!(
        cancel.ends_with("/a2.llm@r1"),
        "the cancel names a2's run-scoped chat handle; got {cancel:?}"
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
