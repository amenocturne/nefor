use std::sync::Arc;

use serde_json::Value;
use tokio::sync::oneshot;

use crate::error::ToolError;
use crate::tools::process::{self, LivePreview, Request};

pub const NAME: &str = "process.exec";

pub async fn run(args: &Value) -> Result<Value, ToolError> {
    run_cancellable_streaming(args, None, None).await
}

pub async fn run_cancellable_streaming(
    args: &Value,
    cancel: Option<oneshot::Receiver<()>>,
    preview: Option<Arc<LivePreview>>,
) -> Result<Value, ToolError> {
    process::execute(parse_args(args)?, cancel, preview).await
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
