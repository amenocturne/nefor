//! Broker-internal error types.

/// Errors produced by the NCP broker.
#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    /// Spawning the plugin subprocess failed before we could observe a line
    /// of input (exec not found or permission denied).
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

    /// Generic IO failure not attributable to a single connection.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
