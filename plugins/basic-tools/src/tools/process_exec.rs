use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::error::ToolError;
use crate::tools::process::{self, LivePreview, Request};

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
        "compact": { "label": "execute process", "primary": { "label": "argv", "select": { "source": "args", "path": "argv" }, "kind": "scalar" } },
        "expanded": { "label": "execute process", "fields": [] },
        "result": { "kind": "content", "fields": [] }
    })
}

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
