//! The failing session's own program, end-to-end: a minimal `(agent …)` +
//! sink program in the CURRENT authoring dialect is compiled through
//! `mag.load` and executed kernel-path against a scripted `chat.*` provider —
//! exactly the flow the lead's `mag` tool drives
//! (starter/lead-workflow/init.lua begin_mag_load → resume_pending_load).
//!
//! What it pins:
//!   - `mag.load` lowers the agent template into the namespaced constellation
//!     (worker.entry / worker.llm / … / sink) and replies with the
//!     modification + the registry factories — the shape the lead's compile
//!     preview and execute validators read;
//!   - eval_agent lowers the agent's `:profile` onto its llm actors' params,
//!     so the lead's profile resolution finds it and the params overlay keys
//!     on the namespaced llm actor id;
//!   - `mag.execute` with a `params_overlay` keyed on `worker.llm` lands the
//!     resolved model/reasoning_effort on the provider's `chat.create`;
//!   - the provider's final answer flows through the sink and the run
//!     completes with the persisted output path.
//!
//! Harness pattern: provider_bridge.rs (spawn the plugin, speak the real
//! chat.* protocol from the test).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::time::timeout;

const PROVIDER: &str = "test-provider";
const SESSION_ID: &str = "agent-sink-session";
const RUN_NAME: &str = "agent-sink-run";
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The minimal agent+sink program the session's lead tried to write, in the
/// current dialect: the `agent` compiler template (there is no "agent" node
/// factory) composed with the program sink via `:terminal`.
const PROGRAM: &str = r#"
(type mag.Task)
(type generic-provider.ProviderOut)
(type generic-provider.FinalAnswer)
(type generic-tool.ToolCalls)
(type generic-tool.ToolHandle)
(type mag.LoopExhausted)

(let [worker (agent {:id "worker"
                     :system "Answer the task."
                     :provider "test-provider"
                     :profile "standard"
                     :tools ["fs/read"]
                     :max-steps 50}
               : mag.Task -> generic-provider.FinalAnswer)
      out    (node "sink" {} : generic-provider.FinalAnswer -> generic-provider.FinalAnswer)]
  (graph worker -> out :terminal out))
"#;

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

fn body_kind(body: &Map<String, Value>) -> Option<&str> {
    body.get("kind").and_then(Value::as_str)
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

#[tokio::test]
async fn minimal_agent_sink_program_compiles_and_runs_kernel_path() {
    let data_dir = std::env::temp_dir().join(format!("mag-agent-sink-{}", std::process::id()));
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::create_dir_all(&data_dir).expect("mkdir data dir");

    // The session workspace the lead writes into.
    let source_dir = data_dir.join("workspace");
    std::fs::create_dir_all(&source_dir).expect("mkdir workspace");
    std::fs::write(source_dir.join("task.mag"), PROGRAM).expect("write program");

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

    // 1. mag.load — the lead's compile/execute handshake.
    let mut load = Map::new();
    load.insert("kind".into(), Value::String("mag.load".into()));
    load.insert("id".into(), Value::String("load-agent-sink".into()));
    load.insert(
        "source_dir".into(),
        Value::String(source_dir.display().to_string()),
    );
    load.insert("entry".into(), Value::String("task.mag".into()));
    send_event(&mut stdin, load).await;

    let loaded = loop {
        let out = read_outgoing(&mut reader, "mag.loaded").await;
        let body = match event_body(&out) {
            Some(b) => b.clone(),
            None => continue,
        };
        match body_kind(&body) {
            Some("mag.loaded") => break body,
            Some("mag.error") => panic!("compile failed: {body:?}"),
            _ => continue,
        }
    };

    // The reply carries what the lead's preview + validators read: the
    // modification's actors and the registry factories.
    let modification = loaded
        .get("modification")
        .and_then(Value::as_object)
        .expect("mag.loaded carries the modification");
    let actors = modification
        .get("actors")
        .and_then(Value::as_array)
        .expect("modification carries actors");
    let actor_ids: Vec<&str> = actors
        .iter()
        .filter_map(|a| a.get("id").and_then(Value::as_str))
        .collect();
    for expected in ["worker.entry", "worker.llm", "worker.loop-counter", "sink"] {
        assert!(
            actor_ids.contains(&expected),
            "the agent template lowers {expected}; saw {actor_ids:?}"
        );
    }
    // eval_agent lowers :profile onto the agent's llm actors — the hook the
    // lead's profile resolution keys the params overlay on.
    let llm = actors
        .iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some("worker.llm"))
        .expect("worker.llm actor");
    assert_eq!(
        llm.get("params")
            .and_then(|p| p.get("profile"))
            .and_then(Value::as_str),
        Some("standard"),
        "the agent's :profile lands on its llm actor's params"
    );
    // The sink actor is canonicalized to id "sink" — the lead's sink
    // validator keys on this.
    let sink = actors
        .iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some("sink"))
        .expect("sink actor");
    assert_eq!(
        sink.get("factory").and_then(Value::as_str),
        Some("sink"),
        "the :terminal node lowers to the canonical sink actor"
    );
    let factories = loaded
        .get("factories")
        .and_then(Value::as_array)
        .expect("mag.loaded carries the registry factories");
    for f in ["llm", "sink", "adapter", "loop-counter"] {
        assert!(
            factories.iter().any(|v| v.as_str() == Some(f)),
            "registry advertises {f}"
        );
    }
    assert!(
        loaded.get("hash").and_then(Value::as_str).is_some(),
        "mag.loaded carries the program hash"
    );

    // 2. mag.execute the RESIDENT program with the lead-resolved profile
    //    params overlaid onto the namespaced llm actor.
    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String("exec-agent-sink".into()));
    execute.insert("session_id".into(), Value::String(SESSION_ID.into()));
    execute.insert("run_id".into(), Value::String(RUN_NAME.into()));
    execute.insert("run_name".into(), Value::String(RUN_NAME.into()));
    execute.insert(
        "params_overlay".into(),
        json!({
            "worker.llm": {
                "model": "gpt-test",
                "reasoning_effort": "medium",
                "provider": PROVIDER
            }
        }),
    );
    send_event(&mut stdin, execute).await;

    // 3. Speak the real provider protocol. One turn: the agent's llm fires
    //    off the initial task seed; answer with a plain final (no tool calls)
    //    so it exits straight to FinalAnswer → sink.
    let create_kind = format!("{PROVIDER}.chat.create");
    let append_kind = format!("{PROVIDER}.chat.append");
    let complete_kind = format!("{PROVIDER}.chat.complete");
    let mut saw_create = false;
    let mut saw_append = false;
    let mut spawned_ids: Vec<String> = Vec::new();
    let mut ready_ids: Vec<String> = Vec::new();
    let run_result;

    loop {
        let out = read_outgoing(&mut reader, "agent-sink event").await;
        let body = match event_body(&out) {
            Some(b) => b.clone(),
            None => continue,
        };
        let kind = match body_kind(&body) {
            Some(k) => k.to_owned(),
            None => continue,
        };

        if kind == "mag.actor_spawned" {
            if let Some(id) = body.get("id").and_then(Value::as_str) {
                spawned_ids.push(id.to_owned());
            }
        } else if kind == "mag.actor_ready" {
            if let Some(id) = body.get("id").and_then(Value::as_str) {
                ready_ids.push(id.to_owned());
            }
        } else if kind == create_kind {
            saw_create = true;
            let chat_id = body
                .get("chat_id")
                .and_then(Value::as_str)
                .expect("create carries chat_id");
            // The wire handle is run-scoped by the kernel:
            // `r<K>/worker.llm@r<seq>`.
            assert!(
                chat_id.contains("worker.llm@r"),
                "the namespaced llm actor drives the provider turn; got {chat_id}"
            );
            assert_eq!(
                body.get("system").and_then(Value::as_str),
                Some("Answer the task."),
                "the agent's :system is threaded"
            );
            // The params overlay landed: resolved model + reasoning effort
            // reach the provider's chat.create.
            assert_eq!(
                body.get("model").and_then(Value::as_str),
                Some("gpt-test"),
                "the overlaid model reaches chat.create"
            );
            assert_eq!(
                body.get("reasoning_effort").and_then(Value::as_str),
                Some("medium"),
                "the overlaid reasoning_effort reaches chat.create"
            );
        } else if kind == append_kind {
            saw_append = true;
        } else if kind == complete_kind {
            assert!(saw_create && saw_append, "complete follows create+append");
            let chat_id = body
                .get("chat_id")
                .and_then(Value::as_str)
                .expect("complete carries chat_id")
                .to_owned();
            send_event(
                &mut stdin,
                chat_result(&chat_id, json!({ "text": "session-final-answer" })),
            )
            .await;
        } else if kind == "mag.run_result" {
            run_result = body;
            break;
        }
    }

    assert_eq!(
        run_result.get("status").and_then(Value::as_str),
        Some("completed"),
        "the session's program runs to completion on the kernel path"
    );
    // Lazy construction: every actor is spawned (registered) at apply, but
    // ready — "began work" — fires only for actors that actually activated.
    // The bounded loop finished without exhausting, so the routed-but-never-
    // activated exhaust summarizer is spawned yet NEVER ready.
    assert!(
        spawned_ids.iter().any(|id| id == "worker.exhaust"),
        "the exhaust summarizer is spawned at apply; saw {spawned_ids:?}"
    );
    assert!(
        !ready_ids.iter().any(|id| id == "worker.exhaust"),
        "the exhaust summarizer never fired, so it never constructs/readies; saw {ready_ids:?}"
    );
    for id in ["worker.entry", "worker.llm", "sink"] {
        assert!(
            ready_ids.iter().any(|r| r == id),
            "{id} fired and readied; saw {ready_ids:?}"
        );
    }
    assert_eq!(
        run_result.get("in_reply_to").and_then(Value::as_str),
        Some("exec-agent-sink")
    );
    let sink_path = run_result
        .get("output_path")
        .and_then(Value::as_str)
        .expect("run_result carries the sink output PATH");
    let sink_output = std::fs::read_to_string(sink_path).expect("read persisted sink output");
    assert!(
        sink_output.contains("session-final-answer"),
        "the sink persisted the final answer; got {sink_output:?}"
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
    std::fs::remove_dir_all(&data_dir).ok();
}
