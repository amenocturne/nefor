#![deny(unsafe_code)]

use clap::Parser;
use git_worktree_plugin::{CreateSpec, OpenSpec, WorktreeError, CREATE_CAPABILITY, OPEN_CAPABILITY};
use nefor_plugin_sdk::{
    await_ready_ok, checked_send, spawn_stdin_reader, spawn_stdout_writer, TransportError,
};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde::Serialize;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

const PLUGIN_NAME: &str = "git-worktree";
const PROTOCOL_VERSION: &str = "0.1";
const CHANNEL_CAP: usize = 64;

/// Stateless Git worktree capability provider. Model-facing tool definitions
/// live in Lua (`lua/libs/git-worktree`).
#[derive(Debug, Parser)]
#[command(name = "git-worktree", version, about)]
struct Args {}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(error) = run(Args::parse()).await {
        tracing::error!(%error, "git-worktree exited with error");
        eprintln!("git-worktree: {error}");
        std::process::exit(1);
    }
    std::process::exit(0);
}

async fn run(_args: Args) -> Result<(), TransportError> {
    let (out_tx, writer) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let _reader = spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = tokio::select! {
        result = await_ready_ok(&mut in_rx) => result?,
        _ = out_tx.closed() => return Err(TransportError::WriterClosed),
    };
    tracing::info!(%engine_version, "ready");
    send_event(&out_tx, hello_body()).await?;

    let mut tasks = tokio::task::JoinSet::new();
    let result = loop {
        tokio::select! {
            _ = out_tx.closed() => break Err(TransportError::WriterClosed),
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(error) = task_result(match completed { Some(result) => result, None => unreachable!("nonempty task set") }) {
                    break Err(error);
                }
            }
            message = in_rx.recv() => match message {
                Some(Ok(env)) => match env.body {
                    Body::System(SystemBody::Shutdown { .. }) => break Ok(()),
                    Body::System(other) => tracing::warn!(?other, "unexpected system envelope"),
                    Body::Event(body) if is_invoke(&body) => {
                        tasks.spawn(invoke_task(out_tx.clone(), body));
                    }
                    Body::Event(_) => (),
                },
                Some(Err(TransportError::Parse(error))) => tracing::warn!(%error, "dropping malformed envelope"),
                Some(Err(error)) => break Err(error),
                None => break Ok(()),
            }
        }
    };
    // spawn_blocking Git operations cannot be canceled by dropping their waiter.
    // Retain ownership until they finish; never delete effects during teardown.
    let mut result = result;
    while let Some(completed) = tasks.join_next().await {
        if let Err(error) = task_result(completed) {
            if result.is_ok() {
                result = Err(error);
            }
        }
    }
    result?;
    send_event(
        &out_tx,
        object(json!({"kind": "git-worktree.goodbye", "reason": "stream closed"})),
    )
    .await?;
    drop(out_tx);
    if let Err(error) = writer.await {
        std::panic::resume_unwind(error.into_panic());
    }
    Ok(())
}

fn task_result(
    result: Result<Result<(), TransportError>, tokio::task::JoinError>,
) -> Result<(), TransportError> {
    match result {
        Ok(result) => result,
        Err(error) => std::panic::resume_unwind(error.into_panic()),
    }
}

fn is_invoke(body: &Map<String, Value>) -> bool {
    body.get("kind").and_then(Value::as_str) == Some(&format!("{PLUGIN_NAME}.capability.invoke"))
}

async fn invoke_task(
    out_tx: mpsc::Sender<PluginOutgoing>,
    body: Map<String, Value>,
) -> Result<(), TransportError> {
    let id = match body.get("id").and_then(Value::as_str) {
        Some(id) => id.to_owned(),
        None => return Ok(()),
    };
    let name = body
        .get("capability")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let args = body.get("args").cloned().unwrap_or_else(|| json!({}));
    let result = tokio::task::spawn_blocking(move || invoke(&name, args)).await;
    let output = match result {
        Ok(output) => output,
        Err(error) => std::panic::resume_unwind(error.into_panic()),
    };
    send_result(&out_tx, &id, output).await
}

async fn send_result(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    id: &str,
    output: Value,
) -> Result<(), TransportError> {
    match send_event(out_tx, result_body(id, output)).await {
        Err(TransportError::FrameTooLarge { .. } | TransportError::InvalidFraming) => {
            send_event(out_tx, object(json!({
                "kind": "capability.result", "id": id,
                "error": "output admission failed; Git effects may already exist and were not rolled back"
            }))).await
        }
        result => result,
    }
}

fn invoke(name: &str, args: Value) -> Value {
    match name {
        CREATE_CAPABILITY => match serde_json::from_value::<CreateSpec>(args) {
            Ok(spec) => match git_worktree_plugin::create(&spec) {
                Ok(worktree) => success(worktree),
                Err(error) => failure(error),
            },
            Err(error) => invalid_args("create", error),
        },
        OPEN_CAPABILITY => match serde_json::from_value::<OpenSpec>(args) {
            Ok(spec) => match git_worktree_plugin::open(&spec) {
                Ok(worktree) => success(worktree),
                Err(error) => failure(error),
            },
            Err(error) => invalid_args("open", error),
        },
        _ => invalid_args("invoke", format!("unknown capability {name}")),
    }
}

fn success(worktree: impl Serialize) -> Value {
    json!({ "ok": true, "worktree": worktree })
}

fn failure(error: WorktreeError) -> Value {
    json!({ "ok": false, "error": error })
}

fn invalid_args(operation: &'static str, error: impl std::fmt::Display) -> Value {
    failure(WorktreeError {
        operation,
        kind: "invalid-args",
        message: error.to_string(),
    })
}

fn hello_body() -> Map<String, Value> {
    object(json!({
        "kind": "git-worktree.hello",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

fn result_body(id: &str, output: Value) -> Map<String, Value> {
    object(json!({ "kind": "capability.result", "id": id, "output": output }))
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().expect("object literal").clone()
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), TransportError> {
    checked_send(
        out_tx,
        PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }),
    )
    .await
}

async fn send_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: Map<String, Value>,
) -> Result<(), TransportError> {
    checked_send(out_tx, PluginOutgoing::event(body)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejected_result_preserves_created_git_effects() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        let path = temp.path().join("worktree");
        std::fs::create_dir(&repository).unwrap();
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "base",
            ],
        ] {
            assert!(std::process::Command::new("git")
                .current_dir(&repository)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        }
        let output = invoke(
            CREATE_CAPABILITY,
            json!({"repository": repository, "path": path, "branch": "feature/retained", "base": "HEAD"}),
        );
        assert_eq!(output["ok"], true, "{output}");
        let (tx, mut rx) = mpsc::channel(8);
        send_result(
            &tx,
            "created",
            json!({"receipt": output, "stderr": "x".repeat(16 * 1024 * 1024)}),
        )
        .await
        .unwrap();
        let failure: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(failure["body"]["id"], "created");
        assert!(failure["body"].get("error").is_some());
        assert!(path.join(".git").exists());
        assert_eq!(
            invoke(
                OPEN_CAPABILITY,
                json!({"repository": repository, "path": path, "branch": "feature/retained"})
            )["ok"],
            true
        );
    }

    #[tokio::test]
    async fn teardown_awaits_running_blocking_operation_instead_of_aborting_waiter() {
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = finished.clone();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async move {
            tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                released.recv().unwrap();
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await
            .unwrap();
            Ok(())
        });
        started_rx.await.unwrap();
        assert!(!finished.load(std::sync::atomic::Ordering::SeqCst));
        release.send(()).unwrap();
        task_result(tasks.join_next().await.unwrap()).unwrap();
        assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn oversized_output_uses_actual_id_and_subsequent_request_works() {
        let (tx, mut rx) = mpsc::channel(8);
        send_result(
            &tx,
            "actual-id",
            json!({"stderr": "x".repeat(16 * 1024 * 1024)}),
        )
        .await
        .unwrap();
        let rejected: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(rejected["body"]["id"], "actual-id");
        assert!(rejected["body"]["error"]
            .as_str()
            .unwrap()
            .contains("not rolled back"));
        assert!(rejected["body"].get("output").is_none());
        invoke_task(tx, object(json!({"kind": "git-worktree.capability.invoke", "id": "later", "capability": CREATE_CAPABILITY, "args": {}}))).await.unwrap();
        let later: Value = serde_json::from_str(&rx.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(later["body"]["id"], "later");
        assert_eq!(later["body"]["output"]["error"]["kind"], "invalid-args");
    }

    #[tokio::test]
    async fn closed_writer_errors_reach_invocation_supervisor() {
        let (tx, rx) = mpsc::channel(8);
        drop(rx);
        let task = tokio::spawn(invoke_task(
            tx,
            object(json!({"id": "owned", "capability": CREATE_CAPABILITY, "args": {}})),
        ));
        assert!(matches!(
            task_result(task.await),
            Err(TransportError::WriterClosed)
        ));
    }

    #[test]
    fn malformed_create_returns_structured_failure() {
        let output = invoke(CREATE_CAPABILITY, json!({ "repository": "/tmp" }));
        assert_eq!(output.get("ok").and_then(Value::as_bool), Some(false));
        assert_eq!(
            output.pointer("/error/kind").and_then(Value::as_str),
            Some("invalid-args")
        );
    }
}
