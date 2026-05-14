//! Tool catalog assembled from `tool.register` events on the bus.
//!
//! Each tool-providing plugin broadcasts a `tool.register { tools: [...] }`
//! after its handshake. The provider collects every such broadcast and
//! flattens the union into the `tools` array of each outgoing Responses
//! request (via the translator).
//!
//! Two responsibilities live here:
//!
//! 1. Catalog state — keyed by sender plugin name, replaced on
//!    re-register from the same sender, unioned across senders.
//! 2. Reverse lookup — when the model returns a tool call, the
//!    dispatcher needs to know which plugin owns the named tool to
//!    target the `<plugin>.tool.invoke` event correctly.

use std::collections::HashMap;

use serde_json::Value;
use tokio::sync::Mutex;

/// One tool's wire shape entry, as carried inside a `tool.register`'s
/// `tools[]` array.
///
/// **Schema field is named `input_schema` on the nefor side**, matching
/// the chat-contract that tool plugins emit. Anthropic-style. The
/// translator maps this to the Responses API's `parameters` at request
/// time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Concurrent-safe tool catalog.
///
/// Keyed by sender plugin name (the `from` on the `tool.register`
/// envelope). `register_from` replaces every entry from a given sender —
/// re-registers from the same plugin idempotently overwrite, and
/// distinct plugins union their entries.
#[derive(Debug, Default)]
pub struct ToolCatalog {
    inner: Mutex<HashMap<String, Vec<ToolSpec>>>,
}

impl ToolCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace `from`'s contribution to the catalog. Pass an empty vec
    /// to clear it (useful if a future protocol allows tool plugins to
    /// drop their catalog).
    pub async fn register_from(&self, from: &str, tools: Vec<ToolSpec>) {
        let mut g = self.inner.lock().await;
        if tools.is_empty() {
            g.remove(from);
        } else {
            g.insert(from.to_owned(), tools);
        }
    }

    /// Return every registered tool, in HashMap-iteration order (stable
    /// within a single Catalog instance, otherwise unspecified). The
    /// translator picks them up at request build time.
    pub async fn all(&self) -> Vec<ToolSpec> {
        let g = self.inner.lock().await;
        g.values().flat_map(|v| v.iter().cloned()).collect()
    }

    /// Reverse-map: tool `name` → owning plugin's `from` identity.
    /// Returns `None` if the name isn't in the catalog. With "last
    /// register wins" semantics across plugins — if two plugins
    /// register the same name, whichever registered later (in
    /// HashMap-iteration order, i.e. arbitrary) is returned. The wire
    /// contract calls this out as a v1 limitation; consumers MAY warn.
    pub async fn owner_of(&self, name: &str) -> Option<String> {
        let g = self.inner.lock().await;
        for (from, tools) in g.iter() {
            if tools.iter().any(|t| t.name == name) {
                return Some(from.clone());
            }
        }
        None
    }

    /// Parse the `tools` array out of a `tool.register` event body.
    /// Skips entries missing a `name` — keeping a malformed entry would
    /// just cause the model to call it with garbage. `description`
    /// defaults to empty and `input_schema` to `{}` so an under-spec'd
    /// tool still rides through (the model can still address it by name).
    pub fn parse_tools(value: &Value) -> Vec<ToolSpec> {
        let arr = match value.as_array() {
            Some(a) => a,
            None => return Vec::new(),
        };
        arr.iter()
            .filter_map(|t| {
                let name = t.get("name").and_then(Value::as_str)?.to_owned();
                let description = t
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let input_schema = t
                    .get("input_schema")
                    .or_else(|| t.get("parameters"))
                    .cloned()
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                Some(ToolSpec {
                    name,
                    description,
                    input_schema,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_file_spec() -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a file.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
        }
    }

    fn write_file_spec() -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Write a file.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["path", "content"],
            }),
        }
    }

    #[tokio::test]
    async fn register_from_one_plugin_lists_its_tools() {
        let cat = ToolCatalog::new();
        cat.register_from("basic-tools", vec![read_file_spec()])
            .await;
        let tools = cat.all().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
    }

    #[tokio::test]
    async fn register_two_plugins_unions_catalogs() {
        let cat = ToolCatalog::new();
        cat.register_from("basic-tools", vec![read_file_spec()])
            .await;
        cat.register_from("web-tools", vec![write_file_spec()])
            .await;
        let names: Vec<String> = cat.all().await.into_iter().map(|t| t.name).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"write_file".to_string()));
    }

    #[tokio::test]
    async fn re_register_from_same_plugin_replaces() {
        let cat = ToolCatalog::new();
        cat.register_from("basic-tools", vec![read_file_spec(), write_file_spec()])
            .await;
        cat.register_from("basic-tools", vec![read_file_spec()])
            .await;
        let tools = cat.all().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
    }

    #[tokio::test]
    async fn owner_of_returns_registering_plugin() {
        let cat = ToolCatalog::new();
        cat.register_from("basic-tools", vec![read_file_spec()])
            .await;
        cat.register_from("web-tools", vec![write_file_spec()])
            .await;
        assert_eq!(
            cat.owner_of("read_file").await.as_deref(),
            Some("basic-tools")
        );
        assert_eq!(
            cat.owner_of("write_file").await.as_deref(),
            Some("web-tools")
        );
        assert_eq!(cat.owner_of("nonexistent").await, None);
    }

    #[tokio::test]
    async fn empty_register_clears_a_senders_entries() {
        let cat = ToolCatalog::new();
        cat.register_from("basic-tools", vec![read_file_spec()])
            .await;
        cat.register_from("basic-tools", vec![]).await;
        assert!(cat.all().await.is_empty());
        assert!(cat.owner_of("read_file").await.is_none());
    }

    #[test]
    fn parse_tools_uses_input_schema_field() {
        let v = json!([
            {"name": "read_file", "description": "Read.", "input_schema": {"type": "object"}},
        ]);
        let parsed = ToolCatalog::parse_tools(&v);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "read_file");
        assert_eq!(parsed[0].input_schema, json!({"type": "object"}));
    }

    #[test]
    fn parse_tools_falls_back_to_parameters_field() {
        // Some tool plugins use the OpenAI-style `parameters` key
        // instead of `input_schema`. Accept both.
        let v = json!([
            {"name": "read_file", "parameters": {"type": "object"}},
        ]);
        let parsed = ToolCatalog::parse_tools(&v);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].input_schema, json!({"type": "object"}));
    }

    #[test]
    fn parse_tools_skips_entries_without_name() {
        let v = json!([
            {"name": "read_file", "input_schema": {"type": "object"}},
            {"description": "Missing name."},
            {"name": "write_file"},
        ]);
        let parsed = ToolCatalog::parse_tools(&v);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "read_file");
        assert_eq!(parsed[1].name, "write_file");
        assert_eq!(parsed[1].description, "");
    }
}
