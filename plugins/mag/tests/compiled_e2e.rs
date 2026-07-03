//! Compiled-program end-to-end test for the mag actor-kernel runtime.
//!
//! This closes the "switch the fixture to the compiled program" step of the
//! acceptance task: instead of hand-seeding `llm` inputs (acceptance.rs) or a
//! single inline sink (execute.rs), it drives the REAL shipped program.
//!
//!   1. `mag.load` the `two-agents.mag` fixture workspace in-process — the
//!      loader (`nefor_mag::load`) lowers it to the resident modification
//!      (namespaced `docs-explorer.*` / `code-writer.*` constellations plus the
//!      program sink, each agent opening with an `adapter` entry node).
//!   2. `mag.execute` the resident program against scripted deterministic
//!      provider replies.
//!
//! What it proves the adapter factory closes:
//!   - the program's initial `{ kind = "task", prompt }` seed reaches
//!     `docs-explorer.entry` (adapter), which lifts it into the ProviderOut turn
//!     `docs-explorer.llm` consumes (task-seed direction);
//!   - the first agent's `FinalAnswer` routes into `code-writer.entry`
//!     (adapter), which lifts it into `code-writer.llm`'s turn (agent hand-off
//!     direction);
//!   - both entry adapters construct and ready at their first firing (lazy
//!     construction), and the pipeline runs to the shared sink and completes.
//!
//! PROVIDER NAME (flagged). The fixture's `llm` actors target the capability
//! `chatgpt-provider` (two-agents.modification.json). Rather than add a
//! test-provider fixture variant, this test answers that capability name
//! directly from the script — the same choice acceptance.rs makes with its
//! `PROVIDER` const. No provider process exists; the harness IS the provider,
//! so the real name costs nothing and keeps the shipped fixture untouched.
//!
//! PLACEMENT. Plugin level (spawn `mag-plugin`, drive its stdio), the pattern of
//! execute.rs / acceptance.rs / load.rs — not engine-spawn. Fully event-driven,
//! settles in well under a second.

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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../starter/mag-kernel/init.lua")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The provider capability the shipped fixture's `llm` actors target.
const PROVIDER: &str = "chatgpt-provider";
const SESSION_ID: &str = "compiled-e2e-session";
const RUN_NAME: &str = "compiled-e2e-run";

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

/// A provider `chat.complete.result` reply — the REAL provider protocol shape
/// (starter/mock-provider: `<name>.chat.complete.result { chat_id, output }`).
/// The mag plugin's provider bridge correlates it back to the driving `llm`
/// actor by chat_id. This is the shape a scripted `tool.invoke` responder would
/// never exercise — the exact gap this test now covers.
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
async fn compiled_two_agents_program_runs_through_the_entry_adapters_to_the_sink() {
    let data_dir = std::env::temp_dir().join(format!("mag-compiled-e2e-{}", std::process::id()));
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

    // 1. Load the compiled two-agents program in-process. The plugin caches it
    //    as the session's resident program and replies `mag.loaded`.
    let mut load = Map::new();
    load.insert("kind".into(), Value::String("mag.load".into()));
    load.insert("id".into(), Value::String("load-e2e".into()));
    load.insert(
        "source_dir".into(),
        Value::String(fixtures_dir().display().to_string()),
    );
    load.insert("entry".into(), Value::String("two-agents.mag".into()));
    send_event(&mut stdin, load).await;

    // Drain to the mag.loaded reply; assert the resident modification contains
    // both entry adapters (the nodes this task's factory constructs).
    let loaded_actors: Vec<String> = loop {
        let out = read_outgoing(&mut reader, "mag.loaded").await;
        let body = match event_body(&out) {
            Some(b) => b,
            None => continue,
        };
        if body_kind(body) == Some("mag.loaded") {
            let actors = body
                .get("modification")
                .and_then(Value::as_object)
                .and_then(|m| m.get("actors"))
                .and_then(Value::as_array)
                .expect("loaded modification carries actors");
            break actors
                .iter()
                .filter_map(|a| a.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect();
        }
    };
    for entry in ["docs-explorer.entry", "code-writer.entry"] {
        assert!(
            loaded_actors.iter().any(|id| id == entry),
            "the compiled program has the {entry} adapter node; saw {loaded_actors:?}"
        );
    }

    // 2. Execute the RESIDENT program (no inline modification) against scripted
    //    provider replies. The initial task seed drives docs-explorer.entry.
    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String("exec-e2e".into()));
    execute.insert("session_id".into(), Value::String(SESSION_ID.into()));
    execute.insert("run_id".into(), Value::String(RUN_NAME.into()));
    execute.insert("run_name".into(), Value::String(RUN_NAME.into()));
    send_event(&mut stdin, execute).await;

    // Deterministic event loop speaking the REAL provider protocol. The bridge
    // drives each provider turn as `chat.create` → `chat.append` → `chat.complete`
    // (NOT a bare tool.invoke — asserted below); we answer each `chat.complete`
    // with a `chat.complete.result` carrying a distinguishable final answer, so
    // each agent's llm exits straight to FinalAnswer (no tool_calls):
    //   seed -> docs-explorer.entry -> docs-explorer.llm -(FinalAnswer)->
    //   code-writer.entry -> code-writer.llm -(FinalAnswer)-> sink.
    let mut seen: Vec<String> = Vec::new();
    let mut ready_ids: Vec<String> = Vec::new();
    let mut provider_agents: std::collections::BTreeSet<String> = Default::default();
    let complete_kind = format!("{PROVIDER}.chat.complete");
    let run_result;

    loop {
        let out = read_outgoing(&mut reader, "compiled-e2e event").await;
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
            // The bridge must translate every capability invoke — provider
            // requests to the chat.* protocol, tool invocations onto the gate's
            // `<gate>.tool.invoke`. A bare tool.invoke on the wire is the exact
            // deadlock regression this guards against: nothing subscribes to it.
            "tool.invoke" => {
                panic!(
                    "bare tool.invoke reached the wire (name {:?}) — every capability \
                     invoke must be bridged (provider → chat.*, tool → <gate>.tool.invoke)",
                    body.get("name")
                );
            }
            k if k == complete_kind => {
                let chat_id = body
                    .get("chat_id")
                    .and_then(Value::as_str)
                    .expect("chat.complete carries chat_id")
                    .to_owned();
                // Answer each agent's turn with a distinguishable final
                // answer. The wire handle is run-scoped by the kernel
                // (`r<K>/<actor>@r<seq>`); match on the actor segment.
                let text = if chat_id.contains("docs-explorer") {
                    provider_agents.insert("docs-explorer".to_owned());
                    "explorer-findings"
                } else if chat_id.contains("code-writer") {
                    provider_agents.insert("code-writer".to_owned());
                    "writer-final-answer"
                } else {
                    panic!("unexpected provider chat_id {chat_id}");
                };
                send_event(&mut stdin, chat_result(&chat_id, json!({ "text": text }))).await;
            }
            "mag.run_result" => {
                run_result = body;
                break;
            }
            _ => {}
        }
    }

    // Both entry adapters constructed and readied at their first firing.
    for entry in ["docs-explorer.entry", "code-writer.entry"] {
        assert!(
            ready_ids.iter().any(|id| id == entry),
            "{entry} adapter readied; saw {ready_ids:?}"
        );
    }

    // Both agents ran their provider turn — the task seed drove the first, the
    // first agent's FinalAnswer (via the second entry adapter) drove the second.
    assert_eq!(
        provider_agents.iter().cloned().collect::<Vec<_>>(),
        vec!["code-writer".to_owned(), "docs-explorer".to_owned()],
        "both agents fired their provider turn through their entry adapter"
    );

    // Lifecycle: the run started and the initial modification applied.
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

    // The pipeline completed to the shared sink with the second agent's answer.
    assert_eq!(
        run_result.get("status").and_then(Value::as_str),
        Some("completed"),
        "the compiled program ran to completion"
    );
    assert_eq!(
        run_result.get("in_reply_to").and_then(Value::as_str),
        Some("exec-e2e"),
        "run_result correlates to the execute request"
    );
    let sink_path = run_result
        .get("output_path")
        .and_then(Value::as_str)
        .expect("run_result carries the sink output PATH");
    let sink_output = std::fs::read_to_string(sink_path).expect("read persisted sink output");
    assert!(
        sink_output.contains("writer-final-answer"),
        "the sink persisted the second agent's final answer; got {sink_output:?}"
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
