use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::error::ToolError;
use crate::tools::process::{self, Request, StreamChunk};

pub const NAME: &str = "shell.script";
pub const DESCRIPTION: &str = "Execute a script through /bin/sh -c.";

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "script": { "type": "string" },
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
        "required": ["script", "cwd", "timeout"]
    })
}

pub fn display() -> Value {
    json!({
        "label": "execute shell script",
        "primary": { "arg": "script" },
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
    let object = args
        .as_object()
        .ok_or_else(|| process::bad_args(NAME, "args must be a JSON object"))?;
    let script = object
        .get("script")
        .and_then(Value::as_str)
        .filter(|script| !script.is_empty())
        .ok_or_else(|| {
            process::bad_args(NAME, "missing required non-empty string field `script`")
        })?;
    let (cwd, timeout, stdin) = process::parse_common(NAME, object)?;
    process::execute(
        Request {
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd,
            timeout,
            stdin,
        },
        cancel,
        stream,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lowers_to_shell_and_returns_structured_result() {
        let result = run(&json!({
            "script": "printf out; printf err >&2",
            "cwd": "/",
            "timeout": { "present": false, "milliseconds": 0 }
        }))
        .await
        .unwrap();
        assert_eq!(result["stdout"], "out");
        assert_eq!(result["stderr"], "err");
        assert_eq!(result["termination"], json!({ "kind": "code", "code": 0 }));
    }
}
