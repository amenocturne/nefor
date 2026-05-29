//! Tool registry for basic-tools.
//!
//! One module per tool. Each tool is a small struct (or zero-sized type)
//! with three pieces:
//!
//! - `NAME` — the wire name advertised in `tool.register` and matched in
//!   `tool.invoke`.
//! - `schema()` — JSON Schema (OpenAI tool-call shape) for the tool's
//!   parameters; included in `tool.register`.
//! - `run(args)` — the implementation. Async because future tools (bash)
//!   will be inherently async; read_file fits naturally here too via
//!   `tokio::fs`.
//!
//! The dispatch layer in [`crate::ncp`] looks up the named tool, parses
//! `args` per the schema, and calls `run`. Tool failures surface as
//! [`crate::error::ToolError`]; the dispatcher folds them into
//! `tool.result { error }` envelopes.

use serde_json::Value;

use crate::error::ToolError;

pub mod bash;
pub mod edit_file;
pub mod read_file;
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
}

/// Static catalog of every tool this plugin exposes. The dispatch layer
/// linearly scans this — fine at the current scale (a handful of tools);
/// upgrade to a `HashMap` once we cross ~16 entries.
pub const TOOLS: &[ToolDescriptor] = &[
    ToolDescriptor {
        name: read_file::NAME,
        description: read_file::DESCRIPTION,
        schema: read_file::schema,
    },
    ToolDescriptor {
        name: write_file::NAME,
        description: write_file::DESCRIPTION,
        schema: write_file::schema,
    },
    ToolDescriptor {
        name: edit_file::NAME,
        description: edit_file::DESCRIPTION,
        schema: edit_file::schema,
    },
    ToolDescriptor {
        name: bash::NAME,
        description: bash::DESCRIPTION,
        schema: bash::schema,
    },
    ToolDescriptor {
        name: search_text::NAME,
        description: search_text::DESCRIPTION,
        schema: search_text::schema,
    },
];

/// Run a tool by name. Returns the tool's textual output on success or a
/// [`ToolError`] for the dispatcher to render as `tool.result { error }`.
///
/// Unknown names produce [`ToolError::BadArgs`] — the closest match in the
/// closed set. The caller MUST match `name` against [`TOOLS`] before
/// invoking; this is just a defensive fallback so a stale catalog doesn't
/// panic.
pub async fn run_tool(name: &str, args: &Value) -> Result<String, ToolError> {
    match name {
        read_file::NAME => read_file::run(args).await,
        write_file::NAME => write_file::run(args).await,
        edit_file::NAME => edit_file::run(args).await,
        bash::NAME => bash::run(args).await,
        search_text::NAME => search_text::run(args).await,
        other => Err(ToolError::BadArgs {
            tool: other.to_owned(),
            message: format!("unknown tool `{other}`"),
        }),
    }
}
