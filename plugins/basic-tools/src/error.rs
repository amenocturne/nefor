//! Domain errors for the basic-tools plugin.
//!
//! Plugin-level transport errors come from [`nefor_plugin_sdk::TransportError`].
//! [`ToolError`] covers tool-call failures surfaced on the wire as
//! `tool.result { error }`. These are *not* fatal — they're a tool's
//! normal error channel and the plugin keeps serving the bus.

/// Tool-call failure modes. These surface on the wire as
/// `tool.result { id, error: "<message>" }`. The variant carries enough
/// context to format a useful diagnostic without leaking internal types.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The path didn't resolve to a file.
    #[error("file not found: {path}")]
    NotFound {
        /// Path the caller asked for.
        path: String,
    },

    /// The path resolved, but it's a directory.
    #[error("path is a directory: {path}")]
    IsDirectory {
        /// Path the caller asked for.
        path: String,
    },

    /// Heuristic: the first 8 KiB of the file contains a NUL byte.
    #[error("file appears to be binary: {path}")]
    BinaryContent {
        /// Path the caller asked for.
        path: String,
    },

    /// The file is larger than the 1 MiB cap.
    #[error("file too large ({size} bytes; cap is 1 MiB): {path}")]
    TooLarge {
        /// Actual size in bytes.
        size: u64,
        /// Path the caller asked for.
        path: String,
    },

    /// The image is larger than the image-read cap.
    #[error("image file too large ({size} bytes; cap is {cap} bytes): {path}")]
    ImageTooLarge {
        /// Actual size in bytes.
        size: u64,
        /// Configured cap in bytes.
        cap: u64,
        /// Path the caller asked for.
        path: String,
    },

    /// The file is not one of the image formats `read_image` accepts.
    #[error("unsupported image format: {path}")]
    UnsupportedImage {
        /// Path the caller asked for.
        path: String,
    },

    /// The file contents weren't valid UTF-8.
    #[error("file is not valid UTF-8: {path}")]
    NotUtf8 {
        /// Path the caller asked for.
        path: String,
    },

    /// Generic IO error opening / reading the file.
    #[error("io error reading {path}: {message}")]
    Io {
        /// Path the caller asked for.
        path: String,
        /// Underlying error message.
        message: String,
    },

    /// `args` payload was structurally invalid for the named tool.
    #[error("invalid args for tool `{tool}`: {message}")]
    BadArgs {
        /// Tool name from the `tool.invoke` event.
        tool: String,
        /// Diagnostic for the caller.
        message: String,
    },

    /// The executable or configured working directory could not be spawned.
    #[error("failed to spawn process `{executable}`: {message}")]
    ProcessSpawn {
        /// Requested argv[0].
        executable: String,
        /// Operating-system diagnostic.
        message: String,
    },

    /// Process pipe or wait handling failed after spawn.
    #[error("process I/O failed while {operation}: {message}")]
    ProcessIo {
        /// Lifecycle operation which failed.
        operation: String,
        /// Operating-system or task diagnostic.
        message: String,
    },

    /// The wall-clock bound elapsed. Captured streams remain separate.
    #[error("process timed out after {timeout_ms}ms; partial stdout: {stdout:?}; partial stderr: {stderr:?}")]
    ProcessTimeout {
        /// Configured timeout in milliseconds.
        timeout_ms: u64,
        /// Stdout captured before termination.
        stdout: String,
        /// Stderr captured before termination.
        stderr: String,
    },

    /// Cancellation terminated and reaped the process group.
    #[error("process cancelled; partial stdout: {stdout:?}; partial stderr: {stderr:?}")]
    ProcessCancelled {
        /// Stdout captured before termination.
        stdout: String,
        /// Stderr captured before termination.
        stderr: String,
    },
}
