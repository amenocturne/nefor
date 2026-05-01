//! Plugin spawn configuration and registry.
//!
//! [`PluginSpec`] captures what `nefor.plugins.spawn { ... }` in `init.lua`
//! declares: a plugin name plus either a command array (spawn an OS
//! subprocess) or a `cli` field (Lua function — virtual plugin reachable
//! only via `nefor plugin <name>`), or both. Validation rejects the case
//! where both are absent (D-10 — every spawn must do something).
//!
//! The engine collects these specs during `init.lua` load and hands them
//! to the broker when it enters the run phase. Virtual plugins (no
//! `command`) skip subprocess spawning entirely; their `cli` function is
//! reached through a Lua-side registry installed by the spawn binding.
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
    /// positional arguments. `None` for virtual plugins (no subprocess; the
    /// plugin exists only as a CLI entry point).
    pub command: Option<Vec<String>>,
    /// True iff `nefor.plugins.spawn` was called with a `cli` Lua function.
    /// The function itself lives in `_NEFOR_CLI[name]` inside the Lua VM —
    /// the engine looks it up by name at dispatch time. We keep a flag
    /// here so [`PluginRegistry::list_with_cli`] can be answered without
    /// reaching into Lua.
    pub has_cli: bool,
}

/// Failure modes from [`PluginRegistry::register`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegisterError {
    /// Another spec with the same name was already registered. Catching
    /// duplicates at config time gives a clearer error than a spawn-time
    /// failure once two processes would share an identity on the wire.
    #[error("plugin {0:?} is already registered")]
    DuplicateName(String),
    /// Both `command` and `cli` were absent. A spawn entry needs to do
    /// at least one of: launch a subprocess, register a CLI handler.
    /// Pointless entries are rejected loudly per D-10.
    #[error("spawn entry {name:?} has neither command nor cli — pointless")]
    PointlessEntry {
        /// The offending plugin name.
        name: String,
    },
    /// The command array was present but empty. The first element is the
    /// binary to exec; an empty array can't spawn anything.
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

    /// Register a plugin. Rejects duplicate names, empty command arrays,
    /// and entries with neither `command` nor `cli` — all would otherwise
    /// surface as obscure runtime failures.
    pub fn register(&mut self, spec: PluginSpec) -> Result<(), RegisterError> {
        if spec.command.is_none() && !spec.has_cli {
            return Err(RegisterError::PointlessEntry {
                name: spec.name.as_str().to_owned(),
            });
        }
        if let Some(cmd) = &spec.command {
            if cmd.is_empty() {
                return Err(RegisterError::EmptyCommand {
                    name: spec.name.as_str().to_owned(),
                });
            }
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

    /// Names of plugins that registered a `cli` entry, in registration
    /// order. Used by `nefor plugin` (no name) to print the menu.
    pub fn list_with_cli(&self) -> Vec<PluginName> {
        self.specs
            .iter()
            .filter(|s| s.has_cli)
            .map(|s| s.name.clone())
            .collect()
    }

    /// Look up a spec by name. Used by CLI dispatch to confirm a plugin
    /// exists before invoking its `cli` function.
    #[allow(dead_code)]
    pub fn find(&self, name: &str) -> Option<&PluginSpec> {
        self.specs.iter().find(|s| s.name.as_str() == name)
    }

    /// Drain the registry, returning the specs and leaving it empty.
    pub fn drain(&mut self) -> Vec<PluginSpec> {
        std::mem::take(&mut self.specs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd_spec(name: &str) -> PluginSpec {
        PluginSpec {
            name: PluginName::new(name).expect("valid"),
            command: Some(vec!["echo".into()]),
            has_cli: false,
        }
    }

    fn cli_only_spec(name: &str) -> PluginSpec {
        PluginSpec {
            name: PluginName::new(name).expect("valid"),
            command: None,
            has_cli: true,
        }
    }

    #[test]
    fn register_accepts_unique_names() {
        let mut r = PluginRegistry::new();
        r.register(cmd_spec("a")).unwrap();
        r.register(cmd_spec("b")).unwrap();
        assert_eq!(r.list().len(), 2);
    }

    #[test]
    fn register_rejects_duplicate() {
        let mut r = PluginRegistry::new();
        r.register(cmd_spec("a")).unwrap();
        let err = r.register(cmd_spec("a")).unwrap_err();
        assert_eq!(err, RegisterError::DuplicateName("a".into()));
    }

    #[test]
    fn register_rejects_empty_command() {
        let mut r = PluginRegistry::new();
        let empty = PluginSpec {
            name: PluginName::new("p").expect("valid"),
            command: Some(vec![]),
            has_cli: false,
        };
        let err = r.register(empty).unwrap_err();
        assert_eq!(err, RegisterError::EmptyCommand { name: "p".into() });
    }

    #[test]
    fn register_rejects_pointless_entry() {
        let mut r = PluginRegistry::new();
        let pointless = PluginSpec {
            name: PluginName::new("p").expect("valid"),
            command: None,
            has_cli: false,
        };
        let err = r.register(pointless).unwrap_err();
        assert_eq!(err, RegisterError::PointlessEntry { name: "p".into() });
    }

    #[test]
    fn register_accepts_cli_only_spec() {
        let mut r = PluginRegistry::new();
        r.register(cli_only_spec("virtual")).unwrap();
        assert_eq!(r.list().len(), 1);
        assert!(r.list()[0].has_cli);
        assert!(r.list()[0].command.is_none());
    }

    #[test]
    fn list_with_cli_filters() {
        let mut r = PluginRegistry::new();
        r.register(cmd_spec("only-cmd")).unwrap();
        r.register(cli_only_spec("only-cli")).unwrap();
        r.register(PluginSpec {
            name: PluginName::new("both").expect("valid"),
            command: Some(vec!["bin".into()]),
            has_cli: true,
        })
        .unwrap();
        let with_cli: Vec<String> = r
            .list_with_cli()
            .into_iter()
            .map(|n| n.as_str().to_owned())
            .collect();
        assert_eq!(with_cli, vec!["only-cli".to_string(), "both".to_string()]);
    }

    #[test]
    fn find_locates_spec() {
        let mut r = PluginRegistry::new();
        r.register(cmd_spec("a")).unwrap();
        assert!(r.find("a").is_some());
        assert!(r.find("b").is_none());
    }

    #[test]
    fn drain_empties_registry() {
        let mut r = PluginRegistry::new();
        r.register(cmd_spec("a")).unwrap();
        let drained = r.drain();
        assert_eq!(drained.len(), 1);
        assert!(r.list().is_empty());
    }
}
