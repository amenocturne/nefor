//! Plugin spawn configuration and registry.
//!
//! [`PluginSpec`] captures what `nefor.plugins.spawn { ... }` in `init.lua`
//! declares: a plugin name plus a [`PluginKind`] — subprocess command, CLI
//! entry point, or both. The enum makes the "neither" case structurally
//! unrepresentable (D-10 — every spawn must do something).
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
    /// What this plugin does: subprocess, CLI entry point, or both.
    /// Replaces the previous `command: Option<Vec<String>>` + `has_cli: bool`
    /// pair — the old representation allowed an invalid state (neither
    /// command nor cli) that was caught at runtime. The enum makes it
    /// unrepresentable at the type level.
    pub kind: PluginKind,
}

/// How a plugin is launched. Every variant carries at least one capability,
/// so the "pointless entry" state is structurally impossible.
#[derive(Debug, Clone)]
pub enum PluginKind {
    /// Subprocess plugin: launch `command[0]` with `command[1..]` as args.
    Command(Vec<String>),
    /// Virtual plugin — CLI-only, no subprocess. The cli function lives in
    /// `_NEFOR_CLI[name]` inside the Lua VM.
    Cli,
    /// Subprocess plugin that also exposes a CLI entry point.
    Both { command: Vec<String> },
}

impl PluginSpec {
    /// The exec command, if this spec launches a subprocess.
    pub fn command(&self) -> Option<&[String]> {
        match &self.kind {
            PluginKind::Command(cmd) | PluginKind::Both { command: cmd } => Some(cmd),
            PluginKind::Cli => None,
        }
    }

    /// Whether this spec registered a `cli` function.
    pub fn has_cli(&self) -> bool {
        matches!(self.kind, PluginKind::Cli | PluginKind::Both { .. })
    }
}

/// Failure modes from [`PluginRegistry::register`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegisterError {
    /// Another spec with the same name was already registered. Catching
    /// duplicates at config time gives a clearer error than a spawn-time
    /// failure once two processes would share an identity on the wire.
    #[error("plugin {0:?} is already registered")]
    DuplicateName(String),
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

    /// Register a plugin. Rejects duplicate names and empty command arrays.
    /// The "pointless entry" case (neither command nor cli) is structurally
    /// impossible — [`PluginKind`] always carries at least one capability.
    pub fn register(&mut self, spec: PluginSpec) -> Result<(), RegisterError> {
        if let Some(cmd) = spec.command() {
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
            .filter(|s| s.has_cli())
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
            kind: PluginKind::Command(vec!["echo".into()]),
        }
    }

    fn cli_only_spec(name: &str) -> PluginSpec {
        PluginSpec {
            name: PluginName::new(name).expect("valid"),
            kind: PluginKind::Cli,
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
            kind: PluginKind::Command(vec![]),
        };
        let err = r.register(empty).unwrap_err();
        assert_eq!(err, RegisterError::EmptyCommand { name: "p".into() });
    }

    #[test]
    fn register_accepts_cli_only_spec() {
        let mut r = PluginRegistry::new();
        r.register(cli_only_spec("virtual")).unwrap();
        assert_eq!(r.list().len(), 1);
        assert!(r.list()[0].has_cli());
        assert!(r.list()[0].command().is_none());
    }

    #[test]
    fn list_with_cli_filters() {
        let mut r = PluginRegistry::new();
        r.register(cmd_spec("only-cmd")).unwrap();
        r.register(cli_only_spec("only-cli")).unwrap();
        r.register(PluginSpec {
            name: PluginName::new("both").expect("valid"),
            kind: PluginKind::Both { command: vec!["bin".into()] },
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
