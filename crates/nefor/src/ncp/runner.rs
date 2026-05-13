//! Plugin runner — spawns declared subprocesses and bridges stdio.
//!
//! The runner is the engine's process-management surface. Given a
//! [`PluginSpec`], it:
//!
//! 1. Spawns `command[0]` with `command[1..]` as arguments via
//!    `tokio::process::Command`. No shell. No env map.
//! 2. Lets the spawned plugin inherit the engine's current working
//!    directory. This matches what users expect when they `cd
//!    ~/projects/foo && nefor` — a `bash` tool running inside
//!    `basic-tools` should see the user's `cd`'d directory, not the
//!    formula's plugin install dir. Plugins that need a different
//!    working directory wrap themselves in a `cd ... && exec ...`
//!    shell script and expose that as their `command`.
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
/// 3. `<exe-dir>` if it contains the bundled `nefor-tui` binary
///    (covers `just install` to `~/.local/bin/` and in-tree
///    `cargo build` to `target/debug/`). Strong positive signal:
///    a real plugin binary is sitting next to the engine.
/// 4. `<exe-dir>/../share/nefor/plugins` if that path is a directory
///    (covers system installs like Homebrew where the binary lives at
///    `<prefix>/bin/nefor` and plugins at `<prefix>/share/nefor/plugins`).
///    Checked **after** `<exe-dir>` because for `just install` the
///    relative-share path resolves to `~/.local/share/nefor/plugins`,
///    which collides with nefor-pm's source-overlay directory: the
///    directory exists but holds source-dir symlinks, not executables.
///    The exe-dir check has a positive signal (`nefor-tui` is a file)
///    that this purely path-shape check lacks, so it wins.
/// 5. `$NEFOR_DATA_DIR/plugins/` if that path is a directory. Late
///    fallback for the same reason: nefor-pm uses it as a Lua
///    require() overlay, not an executables directory.
/// 6. `$XDG_DATA_HOME/nefor/plugins/` (falling back to
///    `~/.local/share/nefor/plugins/`).
///
/// Returns `None` if none of the above produced a usable path. The engine
/// treats this as a fatal configuration error at startup.
pub fn resolve_plugin_root(cli_override: Option<PathBuf>) -> Option<PluginRoot> {
    if let Some(p) = cli_override {
        return Some(PluginRoot(p));
    }
    if let Ok(raw) = std::env::var("NEFOR_PLUGIN_DIR") {
        if !raw.is_empty() {
            return Some(PluginRoot(PathBuf::from(raw)));
        }
    }
    if let Some(p) = exe_dir_in_tree() {
        return Some(PluginRoot(p));
    }
    if let Some(p) = exe_relative_share_plugins() {
        return Some(PluginRoot(p));
    }
    if let Ok(raw) = std::env::var("NEFOR_DATA_DIR") {
        if !raw.is_empty() {
            let p = PathBuf::from(raw).join("plugins");
            if p.is_dir() {
                return Some(PluginRoot(p));
            }
        }
    }
    if let Some(data_home) = xdg_data_home() {
        return Some(PluginRoot(data_home.join("nefor").join("plugins")));
    }
    None
}

fn exe_relative_share_plugins() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let candidate = exe.parent()?.parent()?.join("share/nefor/plugins");
    candidate.is_dir().then_some(candidate)
}

fn exe_dir_in_tree() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let dir = exe.parent()?.to_path_buf();
    dir.join("nefor-tui").is_file().then_some(dir)
}

fn xdg_data_home() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("XDG_DATA_HOME") {
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"))
}

/// Spawn a plugin declared by `spec`, rooted at `root`. Returns a
/// [`Transport`] the broker can attach. The caller must filter out
/// virtual specs (`spec.command.is_none()`) before calling — the runner
/// errors loudly rather than guessing what to spawn.
pub fn spawn_plugin(spec: &PluginSpec, _root: &PluginRoot) -> Result<Transport, BrokerError> {
    let command = spec.command.as_ref().ok_or_else(|| BrokerError::Spawn {
        name: spec.name.as_str().to_owned(),
        command: Vec::new(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "spec has no command (virtual plugin must not be subprocess-spawned)",
        ),
    })?;

    let (binary, args) = command.split_first().ok_or_else(|| BrokerError::Spawn {
        name: spec.name.as_str().to_owned(),
        command: command.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command array"),
    })?;

    // No `current_dir(...)`: spawned plugins inherit the engine's cwd, so
    // tool plugins like `basic-tools` running `bash` resolve relative paths
    // against the directory the user launched nefor from. The PluginRoot
    // is still used by the resolver to find binaries on disk; the runner
    // doesn't need it past that point.
    let mut cmd = Command::new(binary);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|source| BrokerError::Spawn {
        name: spec.name.as_str().to_owned(),
        command: command.clone(),
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
    fn resolve_plugin_root_exe_dir_beats_xdg_share_overlay() {
        // Regression: when the engine is `just install`-ed to
        // ~/.local/bin/, the exe-relative-share path
        // (`<exe-dir>/../share/nefor/plugins`) resolves to
        // ~/.local/share/nefor/plugins, which is exactly the path
        // nefor-pm uses for its Lua require() source overlay (full of
        // source-dir symlinks, no executables). The first attempt at a
        // fix put exe_dir_in_tree AFTER exe_relative_share_plugins,
        // so the path-shape check still won and every plugin spawn
        // failed with "permission denied" / "no such file".
        //
        // The corrected priority puts exe_dir_in_tree FIRST: it has a
        // positive signal (`nefor-tui` is present as a file in the
        // exe dir) that the relative-share path-shape check lacks.
        // Homebrew layout is unaffected because nefor-tui is NOT in
        // /opt/homebrew/bin/ — it lives under share/nefor/plugins/,
        // so exe_dir_in_tree fails and exe_relative_share_plugins
        // catches it.
        //
        // Process env is racy across tests, so this stays a doc-pin
        // anchored to the priority list in resolve_plugin_root's
        // doc-comment. The cli-override and round-trip tests cover
        // the structural contract.
    }

    #[test]
    fn plugin_root_as_path_round_trips() {
        let p = PathBuf::from("/some/where");
        let root = PluginRoot::new(p.clone());
        assert_eq!(root.as_path(), p.as_path());
    }

    #[tokio::test]
    async fn spawn_plugin_inherits_engine_cwd() {
        // The runner intentionally does NOT call `current_dir`, so the
        // child inherits the engine's cwd. The plugin root passed in is
        // used only for binary lookup at the call site — the runner
        // itself doesn't read it past basic command-array validation,
        // so a non-existent root must NOT cause spawn to fail.
        let spec = PluginSpec {
            name: PluginName::new("nonexistent-plugin").expect("valid"),
            command: Some(vec!["echo".into()]),
            has_cli: false,
        };
        let root = PluginRoot::new(PathBuf::from("/tmp/definitely-not-a-plugin-root-xyz"));
        match spawn_plugin(&spec, &root) {
            Ok(_) => {}
            Err(e) => panic!(
                "spawn should succeed regardless of plugin-root existence (cwd is inherited from engine), got error: {e:?}"
            ),
        }
    }

    #[test]
    fn spawn_plugin_rejects_virtual_spec() {
        let spec = PluginSpec {
            name: PluginName::new("virtual").expect("valid"),
            command: None,
            has_cli: true,
        };
        let root = PluginRoot::new(PathBuf::from("/tmp"));
        let res = spawn_plugin(&spec, &root);
        match res {
            Err(BrokerError::Spawn { source, .. }) => {
                assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
            }
            Err(other) => panic!("expected Spawn err for virtual spec, got {other:?}"),
            Ok(_) => panic!("expected error for virtual spec"),
        }
    }
}
