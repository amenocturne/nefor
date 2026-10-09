use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::error::ToolError;
use crate::tools::process::{self, Request, StreamChunk};

pub const NAME: &str = "shell.script";

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
