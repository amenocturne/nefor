//! Domain errors for the mag plugin.
//!
//! Transport failures fold in from the shared plugin SDK; Lua and kernel
//! loading failures are mag-specific. `anyhow` is not used — the binary
//! surfaces `MagError` directly at the `main.rs` boundary.

/// Errors the mag plugin can fail with.
#[derive(Debug, thiserror::Error)]
pub enum MagError {
    /// NCP stdio transport failure (handshake, reader/writer tasks).
    #[error(transparent)]
    Transport(#[from] nefor_plugin_sdk::TransportError),

    /// Error raised inside the embedded Lua VM.
    #[error("lua error: {0}")]
    Lua(#[from] mlua::Error),

    /// The kernel source file could not be read from disk.
    #[error("failed to read kernel {path}: {source}")]
    KernelRead {
        /// Path we tried to read.
        path: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// The kernel chunk evaluated to something other than a table.
    #[error("kernel {path} did not return a table (got {got})")]
    KernelNotTable {
        /// Path of the offending kernel.
        path: String,
        /// Lua type name of what it returned instead.
        got: String,
    },

    /// No kernel path could be resolved from argv or the environment.
    #[error(
        "no kernel path: pass --kernel <path>, or set NEFOR_DEV_DIR / NEFOR_CONFIG_DIR \
         so <dir>/.../mag-kernel/init.lua resolves"
    )]
    NoKernelPath,
}
