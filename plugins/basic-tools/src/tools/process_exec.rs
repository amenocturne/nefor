use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::error::ToolError;
use crate::tools::process::{self, Request, StreamChunk};

pub const NAME: &str = "process.exec";
pub const DESCRIPTION: &str =
    "Execute a program from a structured argv without shell interpretation.";

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "argv": {
                "type": "array",
                "items": { "type": "string" },
                "minItems": 1,
                "description": "Program argv. argv[0] is the executable."
            },
            "cwd": { "type": "string", "minLength": 1 },
            "timeout": {
                "type": "object",
                "properties": {
                    "present": { "type": "boolean" },
                    "milliseconds": { "type": "integer", "minimum": 0 }
                },
                "required": ["present", "milliseconds"],
                "additionalProperties": false
            },
            "stdin": { "type": "string" }
        },
        "required": ["argv", "cwd", "timeout"]
    })
}

pub fn display() -> Value {
    json!({
        "label": "Execute process",
        "primary": { "arg": "argv" },
        "arguments": [
            { "label": "cwd", "arg": "cwd" },
            { "label": "timeout", "arg": "timeout" }
        ],
        "result": { "kind": "content" }
    })
}

pub async fn run(args: &Value) -> Result<Value, ToolError> {
    run_cancellable_streaming(args, None, None).await
}

pub async fn run_cancellable_streaming(
    args: &Value,
    cancel: Option<oneshot::Receiver<()>>,
    stream: Option<mpsc::UnboundedSender<StreamChunk>>,
) -> Result<Value, ToolError> {
    process::execute(parse_args(args)?, cancel, stream).await
}

fn parse_args(args: &Value) -> Result<Request, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| process::bad_args(NAME, "args must be a JSON object"))?;
    let argv = object
        .get("argv")
        .and_then(Value::as_array)
        .ok_or_else(|| process::bad_args(NAME, "missing required array field `argv`"))?
        .iter()
        .map(|arg| {
            arg.as_str()
                .map(str::to_owned)
                .ok_or_else(|| process::bad_args(NAME, "every `argv` item must be a string"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if argv.first().is_none_or(|executable| executable.is_empty()) {
        return Err(process::bad_args(
            NAME,
            "`argv` must be non-empty and `argv[0]` must name an executable",
        ));
    }
    let (cwd, timeout, stdin) = process::parse_common(NAME, object)?;
    Ok(Request {
        argv,
        cwd,
        timeout,
        stdin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unbounded() -> Value {
        json!({ "present": false, "milliseconds": 0 })
    }

    #[tokio::test]
    async fn preserves_argument_boundaries_and_nonzero_status() {
        let result = run(&json!({
            "argv": ["/bin/sh", "-c", "printf '%s' \"$1\"; printf err >&2; exit 7", "sh", "a b"],
            "cwd": "/",
            "timeout": unbounded()
        }))
        .await
        .unwrap();
        assert_eq!(result["stdout"], "a b");
        assert_eq!(result["stderr"], "err");
        assert_eq!(result["termination"], json!({ "kind": "code", "code": 7 }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_signal_termination() {
        let result = run(&json!({
            "argv": ["/bin/sh", "-c", "kill -TERM $$"],
            "cwd": "/",
            "timeout": unbounded()
        }))
        .await
        .unwrap();
        assert_eq!(
            result["termination"],
            json!({ "kind": "signal", "signal": libc::SIGTERM })
        );
    }

    #[tokio::test]
    async fn forwards_stdin() {
        let result = run(&json!({
            "argv": ["/bin/cat"], "cwd": "/", "timeout": unbounded(), "stdin": "hello"
        }))
        .await
        .unwrap();
        assert_eq!(result["stdout"], "hello");
    }

    #[tokio::test]
    async fn rejects_empty_argv() {
        assert!(matches!(
            run(&json!({"argv": [], "cwd": "/", "timeout": unbounded()})).await,
            Err(ToolError::BadArgs { .. })
        ));
    }

    #[tokio::test]
    async fn timeout_returns_partial_streams() {
        let error = run(&json!({
            "argv": ["/bin/sh", "-c", "printf before; printf warning >&2; sleep 5"],
            "cwd": "/",
            "timeout": { "present": true, "milliseconds": 100 }
        }))
        .await
        .unwrap_err();
        assert!(
            matches!(error, ToolError::ProcessTimeout { stdout, stderr, .. } if stdout == "before" && stderr == "warning")
        );
    }

    #[tokio::test]
    async fn cancellation_kills_process_group_and_returns_partial_streams() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("survived");
        let script = format!("printf started; sleep 1; touch {}", marker.display());
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let invocation = tokio::spawn(async move {
            run_cancellable_streaming(
                &json!({ "argv": ["/bin/sh", "-c", script], "cwd": "/", "timeout": unbounded() }),
                Some(cancel_rx),
                None,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel_tx.send(()).unwrap();
        let error = invocation.await.unwrap().unwrap_err();
        assert!(
            matches!(error, ToolError::ProcessCancelled { stdout, stderr } if stdout == "started" && stderr.is_empty())
        );
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert!(!marker.exists(), "a cancelled grandchild must not survive");
    }

    #[tokio::test]
    async fn streams_stdout_and_stderr_independently() {
        let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();
        let _ = run_cancellable_streaming(
            &json!({ "argv": ["/bin/sh", "-c", "printf out; printf err >&2"], "cwd": "/", "timeout": unbounded() }),
            None,
            Some(stream_tx),
        ).await.unwrap();
        let chunks = std::iter::from_fn(|| stream_rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(chunks
            .iter()
            .any(|chunk| matches!(chunk, StreamChunk::Stdout(bytes) if bytes == b"out")));
        assert!(chunks
            .iter()
            .any(|chunk| matches!(chunk, StreamChunk::Stderr(bytes) if bytes == b"err")));
    }
}
