// Capability registry for basic-tools.
//
// One module per capability, each exposing its wire `NAME` and a `run(args)`
// implementation. Model-facing tool definitions (descriptions, parameter
// schemas, display contracts) live in Lua (`lua/libs/basic-tools`); this
// plugin only executes capabilities and validates its own typed inputs.

use serde_json::Value;

use crate::error::ToolError;

/// Every capability this plugin executes.
pub const CAPABILITIES: &[&str] = &[
    read_file::NAME,
    read_image::NAME,
    write_file::NAME,
    process_exec::NAME,
    shell_script::NAME,
];

/// Run a non-process capability by name. Process capabilities go through
/// their cancellable streaming entry points instead.
pub async fn run_capability(name: &str, args: &Value) -> Result<Value, ToolError> {
    match name {
        read_file::NAME => read_file::run(args, read_file::configured_max_bytes())
            .await
            .map(Value::String),
        read_image::NAME => read_image::run(args).await,
        write_file::NAME => write_file::run(args).await.map(Value::String),
        process_exec::NAME => process_exec::run(args).await,
        shell_script::NAME => shell_script::run(args).await,
        other => Err(ToolError::BadArgs {
            tool: other.to_owned(),
            message: format!("unknown capability `{other}`"),
        }),
    }
}
