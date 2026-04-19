//! Tracing setup.
//!
//! `tracing_subscriber::fmt` writing to stderr (the TUI owns stdout once it
//! comes online in a later commit), filtered by `RUST_LOG` via `EnvFilter`,
//! defaulting to `info`.

use tracing_subscriber::util::{SubscriberInitExt, TryInitError};
use tracing_subscriber::{fmt, EnvFilter};

/// Initialize the global tracing subscriber. Returns a typed error so the
/// caller (currently `main`) can decide whether to abort or continue.
pub fn init() -> Result<(), TryInitError> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .finish()
        .try_init()
}
