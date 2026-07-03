//! In-process execute integration test for the mag plugin.
//!
//! Spawns the compiled `mag-plugin`, completes the handshake, then drives a
//! `mag.execute` with an inline synchronous modification through the kernel and
//! asserts:
//!   - the constellation's lifecycle events stream onto the NCP wire
//!     (`mag.run_started`, actor spawn/ready, `mag.modification_applied`,
//!     `mag.run_complete`),
//!   - the ready barrier releases and the run completes in the same turn, and
//!   - the terminal `mag.run_result` carries the sink's final result INLINE
//!     (text the lead relays to the model) plus the persisted output PATH,
//!     and that file was persisted (`persisted` reflects the actual write).
//!
//! The modification is passed **inline** rather than compiled from a shipped
//! `.mag` fixture: the shipped programs (`two-agents.mag`) instantiate `llm`
//! actors bound to `chatgpt-provider`, which isn't present in tests and would
//! hang every actor pending on a provider round-trip. A single `sink` seeded
//! with a `generic-provider.FinalAnswer` uses only the synchronous shipped
//! factory and runs to completion with no bus round-trip — exercising the full
//! begin_run → start → barrier → deliver → run-complete drive.

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

async fn spawn_mag(data_dir: &std::path::Path) -> Child {
    spawn_mag_args(data_dir, &kernel_path(), &[]).await
}

/// Spawn the plugin with an explicit kernel path plus extra argv. Clears
/// `NEFOR_DEV_DIR` so shared-lib resolution is exactly what the arguments
/// say, independent of the developer's shell.
async fn spawn_mag_args(
    data_dir: &std::path::Path,
    kernel: &std::path::Path,
    extra: &[&std::ffi::OsStr],
) -> Child {
    let mut cmd = tokio::process::Command::new(binary_path());
    cmd.arg("--kernel").arg(kernel);
    for a in extra {
        cmd.arg(a);
    }
    cmd.env("NEFOR_DATA_DIR", data_dir)
        .env_remove("NEFOR_DEV_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn().expect("spawn mag-plugin")
}

const READ_TIMEOUT: Duration = Duration::from_secs(120);

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

fn event_kind(out: &PluginOutgoing) -> Option<&str> {
    match &out.body {
        Body::Event(map) => map.get("kind").and_then(Value::as_str),
        _ => None,
    }
}

fn event_body(out: &PluginOutgoing) -> Option<&Map<String, Value>> {
    match &out.body {
        Body::Event(map) => Some(map),
        _ => None,
    }
}

/// Read outgoing events until one matches `kind`, collecting every event kind
/// seen along the way (so the test can assert the lifecycle stream), and
/// returning the matching event.
async fn read_collecting<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    kind: &str,
    seen: &mut Vec<String>,
) -> PluginOutgoing {
    loop {
        let out = read_outgoing(reader, kind).await;
        if let Some(k) = event_kind(&out) {
            seen.push(k.to_owned());
            if k == kind {
                return out;
            }
        }
    }
}

#[tokio::test]
async fn executes_synchronous_sink_program_and_streams_lifecycle_events() {
    let data_dir = std::env::temp_dir().join(format!("mag-execute-{}", std::process::id()));
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

    // A single sink seeded with a FinalAnswer: synchronous, terminal, no bus
    // round-trip. session_id/run_name drive per-node output persistence.
    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String("exec-1".into()));
    execute.insert("session_id".into(), Value::String("test-session".into()));
    execute.insert("run_id".into(), Value::String("sink-run".into()));
    execute.insert("run_name".into(), Value::String("sink-run".into()));
    execute.insert(
        "modification".into(),
        json!({
            "actors": [ { "id": "sink", "factory": "sink", "params": {}, "routes": {} } ],
            "messages": [
                { "to": "sink", "content": { "kind": "generic-provider.FinalAnswer", "text": "hello from mag execute" } }
            ],
            "kills": [],
            "rules": []
        }),
    );
    send_event(&mut stdin, execute).await;

    // Drive to the terminal reply, collecting the lifecycle stream on the way.
    let mut seen: Vec<String> = Vec::new();
    let result = read_collecting(&mut reader, "mag.run_result", &mut seen).await;

    for expected in [
        "mag.run_started",
        "mag.actor_ready",
        "mag.modification_applied",
        "mag.run_complete",
    ] {
        assert!(
            seen.iter().any(|k| k == expected),
            "expected lifecycle event {expected} on the wire; saw {seen:?}"
        );
    }

    let body = event_body(&result).expect("run_result event body");
    assert_eq!(
        body.get("in_reply_to").and_then(Value::as_str),
        Some("exec-1"),
        "run_result correlates to the execute request"
    );
    assert_eq!(
        body.get("status").and_then(Value::as_str),
        Some("completed"),
        "synchronous sink run completes"
    );
    assert_eq!(body.get("run_id").and_then(Value::as_str), Some("sink-run"));

    // The sink's final result rides the terminal reply inline — the lead
    // relays this text to the model without another tool call.
    let result = body
        .get("result")
        .and_then(Value::as_object)
        .expect("run_result carries the sink's final result inline");
    assert_eq!(
        result.get("text").and_then(Value::as_str),
        Some("hello from mag execute"),
        "the inline result is the sink's final answer"
    );

    assert_eq!(
        body.get("persisted").and_then(Value::as_bool),
        Some(true),
        "persisted flags an actual write"
    );
    let output_path = body
        .get("output_path")
        .and_then(Value::as_str)
        .expect("run_result carries the sink output PATH");
    assert!(
        output_path.contains("sessions/test-session/mag/runs/sink-run/"),
        "output persists under the session's mag run dir; got {output_path}"
    );
    let content = std::fs::read_to_string(output_path).expect("read persisted sink output");
    assert!(
        content.contains("hello from mag execute"),
        "the persisted file carries the final answer; got {content:?}"
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

/// Complete the NCP ready handshake for a freshly spawned mag plugin.
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

/// A `mag.execute` carrying a `params_overlay` runs to completion: the overlay
/// is parsed and applied before spawn (unknown ids ignored, named ids patched)
/// and never breaks the run. The precise merge semantics are unit-tested in
/// `apply_params_overlay`; this asserts the wire surface end-to-end.
#[tokio::test]
async fn execute_accepts_and_applies_params_overlay() {
    let data_dir = std::env::temp_dir().join(format!("mag-overlay-{}", std::process::id()));
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

    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String("exec-overlay".into()));
    execute.insert("session_id".into(), Value::String("test-session".into()));
    execute.insert("run_id".into(), Value::String("overlay-run".into()));
    execute.insert("run_name".into(), Value::String("overlay-run".into()));
    execute.insert(
        "modification".into(),
        json!({
            "actors": [ { "id": "sink", "factory": "sink", "params": {}, "routes": {} } ],
            "messages": [
                { "to": "sink", "content": { "kind": "generic-provider.FinalAnswer", "text": "overlaid" } }
            ],
            "kills": [],
            "rules": []
        }),
    );
    // A patch for the sink plus a no-op patch for an id that isn't in the
    // constellation — the unknown id must be ignored, not error.
    execute.insert(
        "params_overlay".into(),
        json!({
            "sink":    { "reasoning_effort": "high" },
            "ghost":   { "model": "does-not-exist" }
        }),
    );
    send_event(&mut stdin, execute).await;

    let mut seen: Vec<String> = Vec::new();
    let result = read_collecting(&mut reader, "mag.run_result", &mut seen).await;
    let body = event_body(&result).expect("run_result body");
    assert_eq!(
        body.get("status").and_then(Value::as_str),
        Some("completed"),
        "overlay-carrying execute still runs to completion; saw {seen:?}"
    );
    assert_eq!(
        body.get("in_reply_to").and_then(Value::as_str),
        Some("exec-overlay")
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

/// Recursively copy a directory tree (the kernel sources into an isolated
/// location that carries no sibling `lua/` tree).
fn copy_dir(src: &std::path::Path, dest: &std::path::Path) {
    std::fs::create_dir_all(dest).expect("mkdir copy dest");
    for entry in std::fs::read_dir(src).expect("read copy src") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let target = dest.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target).expect("copy file");
        }
    }
}

/// Drive one synchronous sink execute to its terminal reply and return the
/// `mag.run_result` body.
async fn drive_sink_execute(child: &mut Child, run_id: &str, text: &str) -> Map<String, Value> {
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

    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String(format!("exec-{run_id}")));
    execute.insert("session_id".into(), Value::String("test-session".into()));
    execute.insert("run_id".into(), Value::String(run_id.to_owned()));
    execute.insert("run_name".into(), Value::String(run_id.to_owned()));
    execute.insert(
        "modification".into(),
        json!({
            "actors": [ { "id": "sink", "factory": "sink", "params": {}, "routes": {} } ],
            "messages": [
                { "to": "sink", "content": { "kind": "generic-provider.FinalAnswer", "text": text } }
            ],
            "kills": [],
            "rules": []
        }),
    );
    send_event(&mut stdin, execute).await;

    let mut seen: Vec<String> = Vec::new();
    let result = read_collecting(&mut reader, "mag.run_result", &mut seen).await;
    let body = event_body(&result).expect("run_result body").clone();

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
    body
}

/// `persisted` reflects an actual write, and `--lua-root` is the composition
/// seam that makes persistence work for a kernel whose config dir carries no
/// sibling `lua/` tree (installed configs). With no resolvable shared tree the
/// run still completes and the reply still carries the result inline, but
/// claims `persisted:false` and no path; with `--lua-root` the write lands.
#[tokio::test]
async fn persisted_flag_reflects_actual_persistence() {
    let base = std::env::temp_dir().join(format!("mag-persist-{}", std::process::id()));
    let data_dir = base.join("data");
    std::fs::create_dir_all(&data_dir).expect("mkdir data dir");

    // An isolated kernel copy: grandparent has no `lua/`, so the shared
    // output-persistence lib is unresolvable without --lua-root.
    let kernel_dir = base.join("isolated/mag-kernel");
    copy_dir(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../starter/mag-kernel"),
        &kernel_dir,
    );
    let kernel = kernel_dir.join("init.lua");

    // No shared Lua tree → no persistence, and the reply says so.
    let mut child = spawn_mag_args(&data_dir, &kernel, &[]).await;
    let body = drive_sink_execute(&mut child, "no-libs-run", "answer without libs").await;
    assert_eq!(
        body.get("status").and_then(Value::as_str),
        Some("completed")
    );
    assert_eq!(
        body.get("persisted").and_then(Value::as_bool),
        Some(false),
        "no write happened, so persisted must be false; got {body:?}"
    );
    assert!(
        body.get("output_path").is_none(),
        "no output_path is claimed when nothing was written"
    );
    assert_eq!(
        body.get("result")
            .and_then(Value::as_object)
            .and_then(|r| r.get("text"))
            .and_then(Value::as_str),
        Some("answer without libs"),
        "the result still rides the reply inline"
    );

    // The composition's --lua-root wiring restores persistence for the same
    // isolated kernel.
    let lua_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../lua");
    let mut child = spawn_mag_args(
        &data_dir,
        &kernel,
        &["--lua-root".as_ref(), lua_root.as_os_str()],
    )
    .await;
    let body = drive_sink_execute(&mut child, "with-libs-run", "answer with libs").await;
    assert_eq!(
        body.get("persisted").and_then(Value::as_bool),
        Some(true),
        "--lua-root makes persistence real; got {body:?}"
    );
    let output_path = body
        .get("output_path")
        .and_then(Value::as_str)
        .expect("persisted run carries the output path");
    let content = std::fs::read_to_string(output_path).expect("read persisted output");
    assert!(content.contains("answer with libs"));

    std::fs::remove_dir_all(&base).ok();
}

/// `mag.apply` outside a live run is rejected with the named guard error — the
/// control plane's apply authority is scoped to an active session with an
/// in-flight run (docs/ir.md, Kernel operations).
#[tokio::test]
async fn apply_without_live_run_is_rejected() {
    let data_dir = std::env::temp_dir().join(format!("mag-apply-guard-{}", std::process::id()));
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

    // No mag.execute first → no live run. The apply must be rejected.
    let mut apply = Map::new();
    apply.insert("kind".into(), Value::String("mag.apply".into()));
    apply.insert("id".into(), Value::String("apply-norun".into()));
    apply.insert("source".into(), Value::String("test".into()));
    apply.insert(
        "modification".into(),
        json!({ "actors": [], "messages": [], "kills": ["whatever"], "rules": [] }),
    );
    send_event(&mut stdin, apply).await;

    let mut seen: Vec<String> = Vec::new();
    let applied = read_collecting(&mut reader, "mag.applied", &mut seen).await;
    let body = event_body(&applied).expect("applied body");
    assert_eq!(
        body.get("in_reply_to").and_then(Value::as_str),
        Some("apply-norun")
    );
    assert_eq!(
        body.get("ok").and_then(Value::as_bool),
        Some(false),
        "apply outside a live run is rejected"
    );
    let err = body
        .get("error")
        .and_then(Value::as_str)
        .expect("rejection carries a named error");
    assert!(
        err.contains("no live run"),
        "named guard error explains the policy; got {err:?}"
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
