//! Top-level error type for the `nefor` binary.
//!
//! Per spec §Code-Level Conventions: typed domain errors with `thiserror`;
//! `anyhow` stays at the top boundary only. Each submodule defines its own
//! `Error` enum; [`NeforError`] aggregates them for the binary's return type.
//!
//! Variants are only added as modules land — no pre-populated `LuaError` /
//! `UiError` / `PluginError` here yet.

use crate::config::ConfigError;
use crate::ui::UiError;

/// Top-level error for the nefor binary.
#[derive(Debug, thiserror::Error)]
pub enum NeforError {
    /// Argument parsing failed — clap already formats the user-facing message.
    #[error(transparent)]
    Cli(#[from] clap::Error),

    /// Config-directory resolution failed.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// TUI / widget-registry failures.
    #[error(transparent)]
    Ui(#[from] UiError),

    /// Filesystem / IO error. Kept now so early FS callers wire through the
    /// same enum instead of inventing ad-hoc error types.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
