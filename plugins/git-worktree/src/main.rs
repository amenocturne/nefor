#![deny(unsafe_code)]

use clap::Parser;
use git_worktree_plugin::{CreateSpec, OpenSpec, WorktreeError, CREATE_TOOL, OPEN_TOOL};
use nefor_plugin_sdk::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, TransportError};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde::Serialize;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

const PLUGIN_NAME: &str = "git-worktree";
const PROTOCOL_VERSION: &str = "0.1";
const CHANNEL_CAP: usize = 64;

#[derive(Debug, Parser)]
#[command(name = "git-worktree", version, about)]
struct Args {
    #[arg(long, default_value = "tool-gate")]
    gate: String,
}

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

async fn run(args: Args) -> Result<(), TransportError> {
    let (out_tx, _writer) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let _reader = spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = await_ready_ok(&mut in_rx).await?;
    tracing::info!(%engine_version, gate = %args.gate, "ready");
    send_event(&out_tx, hello_body()).await?;
    send_event(&out_tx, tools_advertise_body(&args.gate)).await?;

    loop {
        match in_rx.recv().await {
            Some(Ok(env)) => match env.body {
                Body::System(SystemBody::Shutdown { .. }) => return Ok(()),
                Body::System(other) => tracing::warn!(?other, "unexpected system envelope"),
                Body::Event(body) => {
                    if is_invoke(&body) {
                        spawn_invoke(out_tx.clone(), body);
                    }
                }
            },
            Some(Err(error)) => tracing::warn!(%error, "dropping malformed envelope"),
            None => return Ok(()),
        }
    }
}

fn is_invoke(body: &Map<String, Value>) -> bool {
    body.get("kind").and_then(Value::as_str) == Some("git-worktree.tool.invoke")
}

fn spawn_invoke(out_tx: mpsc::Sender<PluginOutgoing>, body: Map<String, Value>) {
    tokio::spawn(async move {
        let id = match body.get("id").and_then(Value::as_str) {
            Some(id) => id.to_owned(),
            None => return,
        };
        let name = body
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let args = body.get("args").cloned().unwrap_or_else(|| json!({}));
        let result = tokio::task::spawn_blocking(move || invoke(&name, args)).await;
        let output = match result {
            Ok(output) => output,
            Err(error) => failure(WorktreeError {
                operation: "invoke",
                kind: "git-failed",
                message: format!("worktree task failed: {error}"),
            }),
        };
        let _ = send_event(&out_tx, tool_result_body(&id, output)).await;
    });
}

fn invoke(name: &str, args: Value) -> Value {
    match name {
        CREATE_TOOL => match serde_json::from_value::<CreateSpec>(args) {
            Ok(spec) => match git_worktree_plugin::create(&spec) {
                Ok(worktree) => success(worktree),
                Err(error) => failure(error),
            },
            Err(error) => invalid_args("create", error),
        },
        OPEN_TOOL => match serde_json::from_value::<OpenSpec>(args) {
            Ok(spec) => match git_worktree_plugin::open(&spec) {
                Ok(worktree) => success(worktree),
                Err(error) => failure(error),
            },
            Err(error) => invalid_args("open", error),
        },
        _ => invalid_args("invoke", format!("unknown tool {name}")),
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

fn tools_advertise_body(gate: &str) -> Map<String, Value> {
    object(json!({
        "kind": format!("{gate}.tools.advertise"),
        "source": PLUGIN_NAME,
        "tools": [
            {
                "name": CREATE_TOOL,
                "description": "Create a fresh Git worktree; existing paths or branches are errors.",
                "parameters": create_schema(),
                "context": {},
                "display": {
                    "label": "create worktree",
                    "primary": { "arg": "path" },
                    "arguments": [
                        { "label": "branch", "arg": "branch" },
                        { "label": "base", "arg": "base" }
                    ],
                    "result": { "kind": "receipt", "text": "worktree created" }
                }
            },
            {
                "name": OPEN_TOOL,
                "description": "Open and validate an explicitly named existing Git worktree.",
                "parameters": open_schema(),
                "context": {},
                "display": {
                    "label": "open worktree",
                    "primary": { "arg": "path" },
                    "arguments": [
                        { "label": "branch", "arg": "branch" },
                        { "label": "repository", "arg": "repository" }
                    ],
                    "result": { "kind": "receipt", "text": "worktree opened" }
                }
            }
        ]
    }))
}

fn create_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "repository": { "type": "string" },
            "path": { "type": "string" },
            "branch": { "type": "string" },
            "base": { "type": "string" }
        },
        "required": ["repository", "path", "branch", "base"],
        "additionalProperties": false
    })
}

fn open_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "repository": { "type": "string" },
            "path": { "type": "string" },
            "branch": { "type": "string" }
        },
        "required": ["repository", "path", "branch"],
        "additionalProperties": false
    })
}

fn tool_result_body(id: &str, output: Value) -> Map<String, Value> {
    object(json!({ "kind": "tool.result", "id": id, "output": output }))
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().expect("object literal").clone()
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), TransportError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| TransportError::WriterClosed)
}

async fn send_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: Map<String, Value>,
) -> Result<(), TransportError> {
    out_tx
        .send(PluginOutgoing::event(body))
        .await
        .map_err(|_| TransportError::WriterClosed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_create_and_open_as_distinct_tools() {
        let body = tools_advertise_body("gate");
        let tools = body.get("tools").and_then(Value::as_array).expect("tools");
        assert_eq!(tools.len(), 2);
        assert_eq!(
            tools[0].get("name").and_then(Value::as_str),
            Some(CREATE_TOOL)
        );
        assert_eq!(
            tools[1].get("name").and_then(Value::as_str),
            Some(OPEN_TOOL)
        );
        for tool in tools {
            assert!(tool.pointer("/display/label").is_some());
            assert!(tool.pointer("/display/primary/arg").is_some());
            assert!(tool.pointer("/display/result/kind").is_some());
        }
    }

    #[test]
    fn malformed_create_returns_structured_failure() {
        let output = invoke(CREATE_TOOL, json!({ "repository": "/tmp" }));
        assert_eq!(output.get("ok").and_then(Value::as_bool), Some(false));
        assert_eq!(
            output.pointer("/error/kind").and_then(Value::as_str),
            Some("invalid-args")
        );
    }
}
