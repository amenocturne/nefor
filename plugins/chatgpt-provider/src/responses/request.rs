//! Wire types for the OpenAI Responses API request body.
//!
//! Field ordering, `skip_serializing_if`, and rename rules match the
//! shape codex-rs's server expects. The reference is
//! `codex-rs/codex-api/src/common.rs::ResponsesApiRequest`; we keep the
//! same wire footprint but drop fields nefor doesn't drive yet
//! (`client_metadata`, websocket-bridging shapes).

use serde::{Deserialize, Serialize};

/// Canonical request body POSTed to `…/codex/responses`.
///
/// `tools` is `Vec<serde_json::Value>` because tool schemas are
/// pass-through — the server validates them, not us. Same logic for
/// `service_tier` / `prompt_cache_key`: opaque strings.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResponsesApiRequest {
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub instructions: String,
    pub input: Vec<ResponseItem>,
    pub tools: Vec<serde_json::Value>,
    pub tool_choice: String,
    pub parallel_tool_calls: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    pub store: bool,
    pub stream: bool,
    pub include: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextControls>,
}

/// Reasoning-track controls. Codex's protocol crate splits these into
/// two enums (`ReasoningEffort` in `openai_models.rs`,
/// `ReasoningSummary` in `config_types.rs`) — we mirror that split so
/// the wire serialization is identical.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReasoningSummary>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningSummary {
    Auto,
    Concise,
    Detailed,
    None,
}

/// `text` controls combining verbosity and JSON-schema output format.
/// Kept as a raw passthrough struct so we don't lock callers into a
/// specific schema shape this phase — Phase 4 / 5 can refine.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TextControls {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<Verbosity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Verbosity {
    Low,
    Medium,
    High,
}

/// One conversation turn item. Tag is on `type` with snake_case values,
/// matching codex's `protocol/src/models.rs::ResponseItem`. The
/// minimal-but-useful set for v1:
///
///   - `message` — user/assistant text turns
///   - `function_call` — model invoking a tool
///   - `function_call_output` — result fed back to the model
///   - `reasoning` — chain-of-thought (passed through verbatim across
///     turns to preserve state on the subscription path)
///   - `compaction` — native opaque compaction state returned by the
///     Responses compaction endpoint
///
/// `Other` is a catch-all so unknown items round-trip without losing
/// data; serde's `tag = "type"` with an inner `serde_json::Value`
/// requires the untagged escape hatch (see serde issue #912), so we
/// model `Other` as the externally-tagged value and rely on the
/// `untagged` fallback ordering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseItem {
    Message {
        role: String,
        content: Vec<MessageContent>,
    },
    FunctionCall {
        /// Server-assigned item id (`fc_…`). Populated on items
        /// received via SSE so the dispatcher can correlate streamed
        /// argument-delta events (keyed by item_id) back to the
        /// originating call. Optional + skip_serializing_if so locally
        /// constructed calls (e.g. tests, replay paths) don't have to
        /// invent one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        name: String,
        /// JSON-encoded string (the Responses API does not pre-parse
        /// tool arguments — we mirror that).
        arguments: String,
        call_id: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
    Reasoning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
        #[serde(default)]
        summary: Vec<ReasoningSummaryPart>,
    },
    Compaction {
        encrypted_content: String,
    },
}

/// Single content part inside a `Message`. The Responses API uses
/// distinct `input_text` / `output_text` types depending on who
/// authored the text (user vs. assistant) — we keep both so a single
/// `MessageContent` value round-trips through input and output paths.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    InputText { text: String },
    InputImage { image_url: String },
    OutputText { text: String },
}

/// One summary segment inside a `Reasoning` item. Codex emits these as
/// `{ "type": "summary_text", "text": "…" }`; we mirror the tagged
/// shape so untouched values serialize identically.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningSummaryPart {
    SummaryText { text: String },
}
