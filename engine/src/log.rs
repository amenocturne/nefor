//! Tracing setup.
//!
//! Writes to `<config_dir>/nefor.log` by default so log output doesn't paint
//! over a plugin that may have taken over the terminal (alternate-screen
//! buffer, raw mode, etc.). When `NEFOR_LOG_STDERR` is set (any non-empty
//! value), logs go to stderr instead — useful for headless runs
//! (`cargo test`, `--help` inspections, debugging with the terminal visible).
//!
//! Filter comes from `RUST_LOG` via `EnvFilter`, defaulting to `info`.
//!
//! File mode never writes live log records to stderr. A terminal UI may own
//! the same terminal through `/dev/tty`; writing through inherited stderr
//! would then scroll its alternate-screen buffer behind the renderer's back.
//! User-facing fatal and abnormal-exit diagnostics are emitted explicitly by
//! the process boundary after terminal-owning plugins have shut down.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::{SubscriberInitExt, TryInitError};
use tracing_subscriber::{fmt, EnvFilter, Layer};

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

/// Initialize the global tracing subscriber writing to `log_path`.
///
/// Creates parent directories if needed; appends if the file already exists.
/// ANSI color codes are suppressed for file output (terminals don't interpret
/// them mid-file and plain text is friendlier to `cat` / `less`).
pub fn init(log_path: &Path) -> Result<(), LogInitError> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let use_stderr = std::env::var_os("NEFOR_LOG_STDERR").is_some_and(|v| !v.is_empty());

    if use_stderr {
        fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .finish()
            .try_init()?;
        return Ok(());
    }

    if let Some(parent) = log_path.parent() {
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
