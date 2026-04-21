//! Plugin spawn configuration and registry.
//!
//! [`PluginSpec`] captures what `nefor.plugins.spawn { ... }` in `init.lua`
//! declares: a plugin name, an OS command, optional args/env/cwd. The engine
//! collects these specs during `init.lua` load and hands them to the broker
//! when it enters the run phase.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A single plugin launch config.
#[derive(Debug, Clone)]
pub struct PluginSpec {
    /// Plugin name claim. Must match the plugin's `attach.name` on the wire,
    /// otherwise attach will be rejected with `name_taken` against the
    /// plugin's self-declared name.
    pub name: String,
    /// Executable to invoke. Looked up via `PATH` if not absolute.
    pub command: String,
    /// Positional arguments.
    pub args: Vec<String>,
    /// Extra environment variables merged into the child's env.
    pub env: HashMap<String, String>,
    /// Working directory for the child; inherits the engine's cwd if `None`.
    pub cwd: Option<String>,
}

/// In-memory list of plugin specs.
#[derive(Debug, Default)]
pub struct PluginRegistry {
    specs: Vec<PluginSpec>,
}

/// Shared handle on the registry. `init.lua` writes to this through the
/// `nefor.plugins.spawn` binding; the engine drains it after `load_init`
/// returns.
pub type SharedPluginRegistry = Arc<Mutex<PluginRegistry>>;

impl PluginRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a plugin. Rejects duplicate names — the broker requires unique
    /// names across the attach phase, and catching duplicates at config time
    /// gives a clearer error than letting two connections race to attach.
    pub fn register(&mut self, spec: PluginSpec) -> Result<(), String> {
        if self.specs.iter().any(|s| s.name == spec.name) {
            return Err(format!("plugin {:?} is already registered", spec.name));
        }
        self.specs.push(spec);
        Ok(())
    }

    /// All registered specs, in registration order.
    #[allow(dead_code)]
    pub fn list(&self) -> &[PluginSpec] {
        &self.specs
    }

    /// Drain the registry, returning the specs and leaving it empty.
    pub fn drain(&mut self) -> Vec<PluginSpec> {
        std::mem::take(&mut self.specs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> PluginSpec {
        PluginSpec {
            name: name.to_string(),
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
        }
    }

    #[test]
    fn register_accepts_unique_names() {
        let mut r = PluginRegistry::new();
        r.register(spec("a")).unwrap();
        r.register(spec("b")).unwrap();
        assert_eq!(r.list().len(), 2);
    }

    #[test]
    fn register_rejects_duplicate() {
        let mut r = PluginRegistry::new();
        r.register(spec("a")).unwrap();
        let err = r.register(spec("a")).unwrap_err();
        assert!(err.contains("already registered"));
    }

    #[test]
    fn drain_empties_registry() {
        let mut r = PluginRegistry::new();
        r.register(spec("a")).unwrap();
        let drained = r.drain();
        assert_eq!(drained.len(), 1);
        assert!(r.list().is_empty());
    }
}
