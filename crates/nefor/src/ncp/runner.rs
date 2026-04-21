//! Plugin runner — spawns declared subprocesses and bridges stdio.
//!
//! The runner is the engine's process-management surface. Given a
//! [`PluginSpec`], it:
//!
//! 1. Resolves the working directory to `<plugin-root>/<name>/` (errors if
//!    the directory does not exist — creation is a plugin-manager concern).
//! 2. Spawns `command[0]` with `command[1..]` as arguments via
//!    `tokio::process::Command`. No shell. No env map. Working directory
//!    is the per-plugin directory; all other environment is inherited
//!    from the engine.
//! 3. Wraps the child's stdio + wait-future in a [`Transport`].
//!
//! Plugins that need shell features (expansions, pipes, builtins) or
//! env-var massaging wrap themselves in a user-chosen wrapper script and
//! expose that as their `command`. See `docs/plugin-authoring.md` for
//! supervision and daemon patterns.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::ncp::error::BrokerError;
use crate::ncp::spawn::PluginSpec;
use crate::ncp::transport::{stdio_transport, ExitOutcome, Transport};

/// Resolved root directory under which each plugin gets a `<name>/`
/// subdirectory used as its cwd.
#[derive(Debug, Clone)]
pub struct PluginRoot(PathBuf);

impl PluginRoot {
    /// Construct from an explicit path (e.g. CLI override).
    #[allow(dead_code)]
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }

    /// Underlying path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Resolve the plugin root directory using, in order of precedence:
///
/// 1. `cli_override` — explicit `--plugin-dir` flag.
/// 2. `NEFOR_PLUGIN_DIR` environment variable.
/// 3. `$XDG_DATA_HOME/nefor/plugins/` (falling back to
///    `~/.local/share/nefor/plugins/`).
/// 4. Dev fallback — walk upward from the current executable looking for
///    a `Cargo.toml` with a `plugins/` sibling. Returns that `plugins/`
///    directory when found. This is the only branch that reaches into
///    the repo layout; it exists so `cargo run` from the worktree Just
///    Works during development.
///
/// Returns `None` if none of the above produced a usable path (missing
/// env vars and no dev layout). The engine treats this as a fatal
/// configuration error at startup.
pub fn resolve_plugin_root(cli_override: Option<PathBuf>) -> Option<PluginRoot> {
    if let Some(p) = cli_override {
        return Some(PluginRoot(p));
    }
    if let Ok(raw) = std::env::var("NEFOR_PLUGIN_DIR") {
        if !raw.is_empty() {
            return Some(PluginRoot(PathBuf::from(raw)));
        }
    }
    if let Some(data_home) = xdg_data_home() {
        return Some(PluginRoot(data_home.join("nefor").join("plugins")));
    }
    dev_fallback().map(PluginRoot)
}

fn xdg_data_home() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("XDG_DATA_HOME") {
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"))
}

fn dev_fallback() -> Option<PathBuf> {
    // Walk up from the current executable looking for a `Cargo.toml` next
    // to a `plugins/` directory. This is purely a dev-time convenience so
    // `cargo run` works without configuration.
    let exe = std::env::current_exe().ok()?;
    let mut cur = exe.as_path();
    while let Some(parent) = cur.parent() {
        let cargo_toml = parent.join("Cargo.toml");
        let plugins_dir = parent.join("plugins");
        if cargo_toml.is_file() && plugins_dir.is_dir() {
            return Some(plugins_dir);
        }
        cur = parent;
    }
    None
}

/// Spawn a plugin declared by `spec`, rooted at `root`. Returns a
/// [`Transport`] the broker can attach.
pub fn spawn_plugin(spec: &PluginSpec, root: &PluginRoot) -> Result<Transport, BrokerError> {
    let cwd = root.as_path().join(spec.name.as_str());
    if !cwd.is_dir() {
        return Err(BrokerError::MissingPluginDir {
            name: spec.name.as_str().to_owned(),
            cwd,
        });
    }

    let (binary, args) = spec
        .command
        .split_first()
        .ok_or_else(|| BrokerError::Spawn {
            name: spec.name.as_str().to_owned(),
            command: spec.command.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command array"),
        })?;

    let mut cmd = Command::new(binary);
    cmd.args(args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|source| BrokerError::Spawn {
        name: spec.name.as_str().to_owned(),
        command: spec.command.clone(),
        source,
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io_err("child stdin missing"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io_err("child stdout missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io_err("child stderr missing"))?;

    let exit = Box::pin(async move {
        match child.wait().await {
            Ok(status) if status.success() => ExitOutcome::CleanExit,
            Ok(_) => ExitOutcome::Crash,
            Err(_) => ExitOutcome::Unknown,
        }
    });

    Ok(stdio_transport(stdin, stdout, stderr, exit))
}

fn io_err(msg: &str) -> BrokerError {
    BrokerError::Io(std::io::Error::other(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nefor_protocol::PluginName;

    #[test]
    fn resolve_plugin_root_cli_override_wins() {
        let p = PathBuf::from("/tmp/explicit");
        let got = resolve_plugin_root(Some(p.clone())).expect("some");
        assert_eq!(got.as_path(), p.as_path());
    }

    #[test]
    fn plugin_root_as_path_round_trips() {
        let p = PathBuf::from("/some/where");
        let root = PluginRoot::new(p.clone());
        assert_eq!(root.as_path(), p.as_path());
    }

    #[test]
    fn spawn_plugin_reports_missing_dir() {
        let spec = PluginSpec {
            name: PluginName::new("nonexistent-plugin").expect("valid"),
            command: vec!["echo".into()],
        };
        let root = PluginRoot::new(PathBuf::from("/tmp/definitely-not-a-plugin-root-xyz"));
        match spawn_plugin(&spec, &root) {
            Err(BrokerError::MissingPluginDir { .. }) => {}
            Err(other) => panic!("expected MissingPluginDir, got {other:?}"),
            Ok(_) => panic!("expected error for nonexistent plugin dir"),
        }
    }
}
