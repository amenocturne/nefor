//! Top-level error type for the `nefor` binary.
//!
//! Per spec §Code-Level Conventions: typed domain errors with `thiserror`;
//! `anyhow` stays at the top boundary only. Each submodule defines its own
//! `Error` enum; [`NeforError`] aggregates them for the binary's return type.

use crate::config::ConfigError;
use crate::lua::LuaError;
use crate::ncp::BrokerError;

/// Top-level error for the nefor binary.
#[derive(Debug, thiserror::Error)]
pub enum NeforError {
    /// Argument parsing failed — clap already formats the user-facing message.
    #[error(transparent)]
    Cli(#[from] clap::Error),

    /// Config-directory resolution failed.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// Lua VM bootstrap / init.lua load failures. `init.lua` *execution*
    /// errors are deliberately not elevated to this top-level type — the
    /// main loop logs them and continues with partial state per spec
    /// §Error handling. This variant is for "the VM couldn't even come up",
    /// which is fatal.
    #[error(transparent)]
    Lua(#[from] LuaError),

    /// NCP broker failures (spawn, runtime).
    #[error(transparent)]
    Broker(#[from] BrokerError),

    /// Filesystem / IO error. Kept now so early FS callers wire through the
    /// same enum instead of inventing ad-hoc error types.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
