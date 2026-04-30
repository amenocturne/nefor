//! Tool catalog assembled from `tool.register` events on the bus.
//!
//! Each tool-providing plugin (`basic-tools`, `web-tools`, …) broadcasts
//! a `tool.register { tools: [...] }` after its handshake. The provider
//! collects every such broadcast and flattens the union into the
//! `tools` array of each outgoing chat-completions request.
//!
//! Two responsibilities live here:
//!
//! 1. **Catalog state** — keyed by sender plugin name, replaced on
//!    re-register from the same sender, unioned across senders.
//! 2. **Reverse lookup** — when the model returns a tool call, the
//!    dispatcher needs to know which plugin owns the named tool to
//!    target the `<plugin>.tool.invoke` event correctly.

use std::collections::HashMap;

use serde_json::Value;
use tokio::sync::Mutex;

/// One tool's wire-shape entry, as carried inside a `tool.register`'s
/// `tools[]` array. The provider passes `parameters` straight through to
/// the OpenAI API as JSON Schema — no shape-shifting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Concurrent-safe tool catalog.
///
/// Keyed by sender plugin name (the `from` on the `tool.register`
/// envelope). `register_from` replaces every entry from a given sender
/// — re-registers from the same plugin idempotently overwrite, and
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

    /// Build the `tools` array for a chat-completions request body.
    /// Returns the OpenAI-spec wrapper shape directly:
    /// `[{"type":"function","function":{"name":..,"description":..,"parameters":..}}]`.
    /// Empty when no tool plugins are attached.
    pub async fn to_openai_tools(&self) -> Vec<Value> {
        let g = self.inner.lock().await;
        let mut out = Vec::new();
        for tools in g.values() {
            for t in tools {
                let mut function = serde_json::Map::new();
                function.insert("name".into(), Value::String(t.name.clone()));
                function.insert("description".into(), Value::String(t.description.clone()));
                function.insert("parameters".into(), t.parameters.clone());
                let mut wrapper = serde_json::Map::new();
                wrapper.insert("type".into(), Value::String("function".into()));
                wrapper.insert("function".into(), Value::Object(function));
                out.push(Value::Object(wrapper));
            }
        }
        out
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
    /// Skips entries that don't have a `name`/`description`/`parameters`
    /// — keeping a malformed entry would just cause the model to call
    /// it with garbage.
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
                let parameters = t
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                Some(ToolSpec {
                    name,
                    description,
                    parameters,
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
            parameters: json!({
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
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["path", "content"],
            }),
        }
    }

    #[tokio::test]
    async fn register_from_one_plugin_lists_its_tools() {
        let cat = ToolCatalog::new();
        cat.register_from("basic-tools", vec![read_file_spec()]).await;
        let tools = cat.to_openai_tools().await;
        assert_eq!(tools.len(), 1);
        let t = &tools[0];
        assert_eq!(t.get("type").and_then(Value::as_str), Some("function"));
        let f = t.get("function").expect("function wrapper");
        assert_eq!(f.get("name").and_then(Value::as_str), Some("read_file"));
        assert_eq!(
            f.get("description").and_then(Value::as_str),
            Some("Read a file.")
        );
        assert!(f.get("parameters").is_some());
    }

    #[tokio::test]
    async fn register_two_plugins_unions_catalogs() {
        let cat = ToolCatalog::new();
        cat.register_from("basic-tools", vec![read_file_spec()]).await;
        cat.register_from("web-tools", vec![write_file_spec()]).await;
        let tools = cat.to_openai_tools().await;
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools
            .iter()
            .map(|t| {
                t.get("function")
                    .unwrap()
                    .get("name")
                    .unwrap()
                    .as_str()
                    .unwrap()
            })
            .collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
    }

    #[tokio::test]
    async fn re_register_from_same_plugin_replaces() {
        let cat = ToolCatalog::new();
        cat.register_from("basic-tools", vec![read_file_spec(), write_file_spec()])
            .await;
        // Re-register with just one tool — the other should disappear.
        cat.register_from("basic-tools", vec![read_file_spec()]).await;
        let tools = cat.to_openai_tools().await;
        assert_eq!(tools.len(), 1);
        let f = tools[0].get("function").expect("fn");
        assert_eq!(f.get("name").and_then(Value::as_str), Some("read_file"));
    }

    #[tokio::test]
    async fn owner_of_returns_registering_plugin() {
        let cat = ToolCatalog::new();
        cat.register_from("basic-tools", vec![read_file_spec()]).await;
        cat.register_from("web-tools", vec![write_file_spec()]).await;
        assert_eq!(cat.owner_of("read_file").await.as_deref(), Some("basic-tools"));
        assert_eq!(cat.owner_of("write_file").await.as_deref(), Some("web-tools"));
        assert_eq!(cat.owner_of("nonexistent").await, None);
    }

    #[tokio::test]
    async fn empty_register_clears_a_senders_entries() {
        let cat = ToolCatalog::new();
        cat.register_from("basic-tools", vec![read_file_spec()]).await;
        cat.register_from("basic-tools", vec![]).await;
        assert!(cat.to_openai_tools().await.is_empty());
        assert!(cat.owner_of("read_file").await.is_none());
    }

    #[test]
    fn parse_tools_skips_entries_without_name() {
        let v = json!([
            {"name": "read_file", "description": "Read.", "parameters": {"type": "object"}},
            {"description": "Missing name.", "parameters": {"type": "object"}},
            {"name": "write_file", "parameters": {"type": "object"}}, // no description
        ]);
        let parsed = ToolCatalog::parse_tools(&v);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "read_file");
        assert_eq!(parsed[1].name, "write_file");
        assert_eq!(parsed[1].description, "");
    }
}
