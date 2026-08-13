//! Tracing setup.
//!
//! File logging keeps output from painting over a plugin that may have taken
//! over the terminal (alternate-screen buffer, raw mode, etc.). Explicit
//! stderr mode remains available for headless runs and terminal-visible
//! debugging.
//!
//! Filter comes from `RUST_LOG` via `EnvFilter`, defaulting to `info`.
//!
//! File mode never writes live log records to stderr. A terminal UI may own
//! the same terminal through `/dev/tty`; writing through inherited stderr
//! would then scroll its alternate-screen buffer behind the renderer's back.
//! User-facing fatal and abnormal-exit diagnostics are emitted explicitly by
//! the process boundary after terminal-owning plugins have shut down.

use std::fs::OpenOptions;
use std::path::PathBuf;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::{SubscriberInitExt, TryInitError};
use tracing_subscriber::{fmt, EnvFilter, Layer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogDestination {
    File(PathBuf),
    Stderr,
}

impl std::fmt::Display for LogDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => path.display().fmt(f),
            Self::Stderr => f.write_str("stderr"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LogInitError {
    #[error("failed to open log file {path:?}: {source}")]
    OpenFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Init(#[from] TryInitError),
}

/// Initialize the global tracing subscriber for the selected destination.
///
/// File destinations are used exactly as selected. Their parent directories
/// are created if needed, and existing files are appended to.
pub fn init(destination: &LogDestination) -> Result<(), LogInitError> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let LogDestination::File(log_path) = destination else {
        fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .finish()
            .try_init()?;
        return Ok(());
    };

    if let Some(parent) = log_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|source| LogInitError::OpenFile {
            path: log_path.to_path_buf(),
            source,
        })?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|source| LogInitError::OpenFile {
            path: log_path.to_path_buf(),
            source,
        })?;

    let file_layer = fmt::layer()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_filter(filter);

    tracing_subscriber::registry().with(file_layer).try_init()?;
    Ok(())
}
