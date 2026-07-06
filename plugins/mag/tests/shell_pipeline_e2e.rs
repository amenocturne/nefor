//! End-to-end mag-as-shell pipeline: a MAG expression compiles through
//! `mag.load` (the shell defaults fill in ids and the implicit terminal),
//! executes inline through the kernel, and its bash capability nodes surface
//! as gate-visible `tool-gate.tool.invoke` envelopes — answered here by a
//! scripted responder, exactly as the tool gate would. Asserts the two load-
//! bearing pipe semantics:
//!
//!   * the second command of a chain receives the first command's stdout as
//!     its stdin (`->` is the pipe), and
//!   * the terminal `mag.run_result` settles back carrying the last command's
//!     stdout as the inline text result — what the lead-side `mag-eval` tool
//!     relays as the tool result.
//!
//! Plus the loud-failure contract: a non-zero exit fails the run with the
//! stderr detail on the terminal reply.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::time::timeout;

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mag-plugin"))
}

fn kernel_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel/init.lua")
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

fn event_body(out: &PluginOutgoing) -> Option<&Map<String, Value>> {
    match &out.body {
        Body::Event(map) => Some(map),
        _ => None,
    }
}

/// Read outgoing events until one matches `kind`; return its body.
async fn read_until<R: AsyncBufReadExt + Unpin>(reader: &mut R, kind: &str) -> Map<String, Value> {
    loop {
        let out = read_outgoing(reader, kind).await;
        if let Some(body) = event_body(&out) {
            if body.get("kind").and_then(Value::as_str) == Some(kind) {
                return body.clone();
            }
        }
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

/// Load a MAG expression from a temp source dir; return the lowered
/// modification off the `mag.loaded` reply.
async fn load_expression<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    stdin: &mut ChildStdin,
    source_dir: &std::path::Path,
    expr: &str,
) -> Value {
    std::fs::write(source_dir.join("eval.mag"), expr).expect("write expression source");
    let mut load = Map::new();
    load.insert("kind".into(), Value::String("mag.load".into()));
    load.insert("id".into(), Value::String("load-1".into()));
    load.insert(
        "source_dir".into(),
        Value::String(source_dir.display().to_string()),
    );
    load.insert("entry".into(), Value::String("eval.mag".into()));
    send_event(stdin, load).await;

    let loaded = read_until(reader, "mag.loaded").await;
    assert_eq!(
        loaded.get("in_reply_to").and_then(Value::as_str),
        Some("load-1")
    );
    loaded
        .get("modification")
        .cloned()
        .expect("mag.loaded carries the lowered modification")
}

/// Reply to a gate invoke the way tool-gate/basic-tools would: a broadcast
/// `tool.result` keyed by the invoke's correlation id, carrying the combined
/// output string.
async fn answer_invoke(stdin: &mut ChildStdin, invoke: &Map<String, Value>, output: &str) {
    let id = invoke
        .get("id")
        .and_then(Value::as_str)
        .expect("invoke carries a correlation id");
    let mut result = Map::new();
    result.insert("kind".into(), Value::String("tool.result".into()));
    result.insert("id".into(), Value::String(id.to_owned()));
    result.insert("output".into(), Value::String(output.to_owned()));
    send_event(stdin, result).await;
}

#[tokio::test]
async fn pipe_expression_runs_bash_through_the_gate_and_settles_with_the_text() {
    let base = std::env::temp_dir().join(format!("mag-shell-e2e-{}", std::process::id()));
    let data_dir = base.join("data");
    let source_dir = base.join("src");
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

    // The two-command pipe, no let-binding, no :terminal — pure defaults.
    let modification = load_expression(
        &mut reader,
        &mut stdin,
        &source_dir,
        r#"((bash "rg -n foo") -> (bash "sort"))"#,
    )
    .await;
    let ids: Vec<&str> = modification["actors"]
        .as_array()
        .expect("actors")
        .iter()
        .map(|a| a["id"].as_str().expect("actor id"))
        .collect();
    assert_eq!(
        ids,
        vec!["bash-1", "bash-2", "sink"],
        "implicit ids in appearance order plus the implicit terminal"
    );

    // Execute the loaded program inline (the mag-eval tool's execute path).
    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String("eval-1".into()));
    execute.insert("run_id".into(), Value::String("eval-1".into()));
    execute.insert("run_name".into(), Value::String("eval-1".into()));
    execute.insert("session_id".into(), Value::String("shell-e2e".into()));
    execute.insert("modification".into(), modification);
    send_event(&mut stdin, execute).await;

    // First command fires dependency-style: gate-visible, no stdin.
    let first = read_until(&mut reader, "tool-gate.tool.invoke").await;
    assert_eq!(first.get("name").and_then(Value::as_str), Some("bash"));
    assert_eq!(first.get("from").and_then(Value::as_str), Some("bash-1"));
    let args = first.get("args").and_then(Value::as_object).expect("args");
    assert_eq!(
        args.get("command").and_then(Value::as_str),
        Some("rg -n foo")
    );
    assert!(
        args.get("stdin").is_none(),
        "a Unit-fired command carries no stdin; got {args:?}"
    );
    answer_invoke(&mut stdin, &first, "b: foo\na: foo\n[exit 0]").await;

    // Second command receives the first's stdout as stdin — the pipe.
    let second = read_until(&mut reader, "tool-gate.tool.invoke").await;
    assert_eq!(second.get("from").and_then(Value::as_str), Some("bash-2"));
    let args = second.get("args").and_then(Value::as_object).expect("args");
    assert_eq!(args.get("command").and_then(Value::as_str), Some("sort"));
    assert_eq!(
        args.get("stdin").and_then(Value::as_str),
        Some("b: foo\na: foo\n"),
        "the pipe: first command's stdout is the second's stdin"
    );
    answer_invoke(&mut stdin, &second, "a: foo\nb: foo\n[exit 0]").await;

    // The run settles with the last command's stdout as the inline text —
    // exactly what mag-eval relays as the tool result.
    let result = read_until(&mut reader, "mag.run_result").await;
    assert_eq!(
        result.get("in_reply_to").and_then(Value::as_str),
        Some("eval-1"),
        "the terminal reply correlates to the execute request"
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
        Some("a: foo\nb: foo\n"),
        "the terminal output is the last command's stdout"
    );

    shutdown(stdin, child).await;
    std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn nonzero_exit_fails_the_run_with_the_stderr_detail() {
    let base = std::env::temp_dir().join(format!("mag-shell-fail-{}", std::process::id()));
    let data_dir = base.join("data");
    let source_dir = base.join("src");
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

    let modification = load_expression(
        &mut reader,
        &mut stdin,
        &source_dir,
        r#"(bash "cat missing.txt")"#,
    )
    .await;

    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String("eval-fail".into()));
    execute.insert("run_id".into(), Value::String("eval-fail".into()));
    execute.insert("run_name".into(), Value::String("eval-fail".into()));
    execute.insert("session_id".into(), Value::String("shell-e2e".into()));
    execute.insert("modification".into(), modification);
    send_event(&mut stdin, execute).await;

    let invoke = read_until(&mut reader, "tool-gate.tool.invoke").await;
    answer_invoke(
        &mut stdin,
        &invoke,
        "[stderr]\ncat: missing.txt: No such file or directory\n[exit 1]",
    )
    .await;

    // The unrouted failure escalates: the run fails loudly, stderr surfaced.
    let result = read_until(&mut reader, "mag.run_result").await;
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("failed"),
        "a non-zero exit fails the run; got {result:?}"
    );
    let error = result
        .get("error")
        .and_then(Value::as_str)
        .expect("the failure carries an error detail");
    assert!(
        error.contains("exited 1") && error.contains("No such file or directory"),
        "the error names the exit code and the stderr detail; got {error:?}"
    );
    assert_eq!(
        result.get("in_reply_to").and_then(Value::as_str),
        Some("eval-fail")
    );

    shutdown(stdin, child).await;
    std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn bare_bash_expression_settles_with_its_stdout() {
    let base = std::env::temp_dir().join(format!("mag-shell-bare-{}", std::process::id()));
    let data_dir = base.join("data");
    let source_dir = base.join("src");
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

    let modification =
        load_expression(&mut reader, &mut stdin, &source_dir, r#"(bash "ls")"#).await;

    let mut execute = Map::new();
    execute.insert("kind".into(), Value::String("mag.execute".into()));
    execute.insert("id".into(), Value::String("eval-bare".into()));
    execute.insert("run_id".into(), Value::String("eval-bare".into()));
    execute.insert("run_name".into(), Value::String("eval-bare".into()));
    execute.insert("session_id".into(), Value::String("shell-e2e".into()));
    execute.insert("modification".into(), modification);
    send_event(&mut stdin, execute).await;

    let invoke = read_until(&mut reader, "tool-gate.tool.invoke").await;
    assert_eq!(
        invoke
            .get("args")
            .and_then(|a| a.get("command"))
            .and_then(Value::as_str),
        Some("ls")
    );
    answer_invoke(&mut stdin, &invoke, "Cargo.toml\nsrc\n[exit 0]").await;

    let result = read_until(&mut reader, "mag.run_result").await;
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("completed")
    );
    assert_eq!(
        result
            .get("result")
            .and_then(|r| r.get("text"))
            .and_then(Value::as_str),
        Some("Cargo.toml\nsrc\n")
    );

    shutdown(stdin, child).await;
    std::fs::remove_dir_all(&base).ok();
}
