//! Broker-internal error types.

use std::path::PathBuf;

/// Errors produced by the NCP broker.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum BrokerError {
    /// Spawning the plugin subprocess failed before we could observe a line
    /// of input (exec not found, permission denied, cwd missing).
    #[error("failed to spawn plugin {name:?} (command {command:?}): {source}")]
    Spawn {
        /// Plugin name (from spawn config).
        name: String,
        /// The exec command (first element is the binary).
        command: Vec<String>,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// The plugin's working directory does not exist. The runner expects
    /// `<plugin-root>/<name>/` to be present; directory creation is a
    /// plugin-manager concern, not the engine's.
    #[error("plugin {name:?} working directory {cwd:?} does not exist")]
    MissingPluginDir {
        /// Plugin name.
        name: String,
        /// The resolved cwd path.
        cwd: PathBuf,
    },

    /// Generic IO failure not attributable to a single connection.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
