//! Plugin spawn configuration and registry.
//!
//! [`PluginSpec`] captures what `nefor.plugins.spawn { ... }` in `init.lua`
//! declares: a plugin name and the command array to exec. The engine
//! collects these specs during `init.lua` load and hands them to the
//! broker when it enters the run phase.
//!
//! The runner spawns subprocesses with direct `Command::new(binary)` —
//! no shell, no env-map, no cwd override. Working directory is
//! `<plugin-root>/<name>/` (resolved at spawn time by the engine). Plugins
//! that need shell features wrap themselves in a user-chosen wrapper
//! script and expose that as their `command`.

use std::sync::{Arc, Mutex};

use nefor_protocol::PluginName;

/// A single plugin launch config.
#[derive(Debug, Clone)]
pub struct PluginSpec {
    /// Validated plugin name. The engine stamps `from = name` on every
    /// envelope this connection emits.
    pub name: PluginName,
    /// The exec command: `[binary, ...args]`. First element is the binary
    /// path (looked up via `PATH` if not absolute); remaining elements are
    /// positional arguments. Must be non-empty.
    pub command: Vec<String>,
}

/// Failure modes from [`PluginRegistry::register`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegisterError {
    /// Another spec with the same name was already registered. Catching
    /// duplicates at config time gives a clearer error than a spawn-time
    /// failure once two processes would share an identity on the wire.
    #[error("plugin {0:?} is already registered")]
    DuplicateName(String),
    /// The command array was empty. The first element is the binary to
    /// exec; without it there's nothing to spawn.
    #[error("plugin {name:?} has an empty command array")]
    EmptyCommand {
        /// The offending plugin name.
        name: String,
    },
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

    /// Register a plugin. Rejects duplicate names and empty command arrays
    /// at config time — both would otherwise surface as obscure runtime
    /// failures once the broker tried to spawn.
    pub fn register(&mut self, spec: PluginSpec) -> Result<(), RegisterError> {
        if spec.command.is_empty() {
            return Err(RegisterError::EmptyCommand {
                name: spec.name.as_str().to_owned(),
            });
        }
        if self.specs.iter().any(|s| s.name == spec.name) {
            return Err(RegisterError::DuplicateName(spec.name.as_str().to_owned()));
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
            name: PluginName::new(name).expect("valid"),
            command: vec!["echo".into()],
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
        assert_eq!(err, RegisterError::DuplicateName("a".into()));
    }

    #[test]
    fn register_rejects_empty_command() {
        let mut r = PluginRegistry::new();
        let empty = PluginSpec {
            name: PluginName::new("p").expect("valid"),
            command: vec![],
        };
        let err = r.register(empty).unwrap_err();
        assert_eq!(err, RegisterError::EmptyCommand { name: "p".into() });
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
