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
        /// The OS command.
        command: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// Misconfigured cwd that doesn't exist when we went to set it.
    #[error("plugin {name:?} cwd {cwd:?} does not exist")]
    InvalidCwd {
        /// Plugin name.
        name: String,
        /// The cwd path.
        cwd: PathBuf,
    },

    /// Generic IO failure not attributable to a single connection.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
