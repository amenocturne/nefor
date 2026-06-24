//! Tool registry for basic-tools.
//!
//! One module per tool. Each tool is a small struct (or zero-sized type)
//! with four pieces:
//!
//! - `NAME` — the wire name advertised in `tool.register` and matched in
//!   `tool.invoke`.
//! - `schema()` — JSON Schema (OpenAI tool-call shape) for the tool's
//!   parameters; included in `tool.register`.
//! - `context()` — internal metadata consumed by wrappers. It is not part
//!   of the model-facing schema.
//! - `run(args)` — the implementation. Async because future tools (bash)
//!   will be inherently async; read_file fits naturally here too via
//!   `tokio::fs`.
//!
//! The dispatch layer in [`crate::ncp`] looks up the named tool, parses
//! `args` per the schema, and calls `run`. Tool failures surface as
//! [`crate::error::ToolError`]; the dispatcher folds them into
//! `tool.result { error }` envelopes.

use serde_json::{json, Value};

use crate::error::ToolError;

pub mod bash;
pub mod edit_file;
pub mod read_file;
pub mod read_image;
pub mod search_text;
pub mod write_file;

/// Descriptor for a single registered tool. Used by the catalog builder
/// in [`crate::ncp`] to assemble the `tool.register` event body.
pub struct ToolDescriptor {
    /// Wire name (e.g. `"read_file"`).
    pub name: &'static str,
    /// Human-readable description shipped to the LLM via the provider.
    pub description: &'static str,
    /// JSON Schema for the tool's parameters (OpenAI tool-call format).
    pub schema: fn() -> Value,
    /// Internal context metadata for runtime hooks.
    pub context: fn() -> Value,
}

fn file_path_context() -> Value {
    json!({
        "folders": [
            { "from": "file_path", "arg": "path", "cwd_arg": "cwd" }
        ]
    })
}

fn path_or_file_context() -> Value {
    json!({
        "folders": [
            { "from": "path_or_file", "arg": "path", "cwd_arg": "cwd", "default": "." }
        ]
    })
}

fn cwd_context() -> Value {
    json!({
        "folders": [
            { "from": "cwd", "arg": "cwd", "default": "." }
        ]
    })
}

/// Static catalog of every tool this plugin exposes. The dispatch layer
/// linearly scans this — fine at the current scale (a handful of tools);
/// upgrade to a `HashMap` once we cross ~16 entries.
pub const TOOLS: &[ToolDescriptor] = &[
    ToolDescriptor {
        name: read_file::NAME,
        description: read_file::DESCRIPTION,
        schema: read_file::schema,
        context: file_path_context,
    },
    ToolDescriptor {
        name: read_image::NAME,
        description: read_image::DESCRIPTION,
        schema: read_image::schema,
        context: file_path_context,
    },
    ToolDescriptor {
        name: write_file::NAME,
        description: write_file::DESCRIPTION,
        schema: write_file::schema,
        context: file_path_context,
    },
    ToolDescriptor {
        name: edit_file::NAME,
        description: edit_file::DESCRIPTION,
        schema: edit_file::schema,
        context: file_path_context,
    },
    ToolDescriptor {
        name: bash::NAME,
        description: bash::DESCRIPTION,
        schema: bash::schema,
        context: cwd_context,
    },
    ToolDescriptor {
        name: search_text::NAME,
        description: search_text::DESCRIPTION,
        schema: search_text::schema,
        context: path_or_file_context,
    },
];

/// Run a tool by name. Returns the tool's textual output on success or a
/// [`ToolError`] for the dispatcher to render as `tool.result { error }`.
///
/// Unknown names produce [`ToolError::BadArgs`] — the closest match in the
/// closed set. The caller MUST match `name` against [`TOOLS`] before
/// invoking; this is just a defensive fallback so a stale catalog doesn't
/// panic.
pub async fn run_tool(name: &str, args: &Value) -> Result<Value, ToolError> {
    match name {
        read_file::NAME => read_file::run(args).await.map(Value::String),
        read_image::NAME => read_image::run(args).await,
        write_file::NAME => write_file::run(args).await.map(Value::String),
        edit_file::NAME => edit_file::run(args).await.map(Value::String),
        bash::NAME => bash::run(args).await.map(Value::String),
        search_text::NAME => search_text::run(args).await.map(Value::String),
        other => Err(ToolError::BadArgs {
            tool: other.to_owned(),
            message: format!("unknown tool `{other}`"),
        }),
    }
}
