//! `nefor` binary entry point.
//!
//! Per spec §Startup sequence step 1: resolve the config dir from env + args,
//! log it, exit. Reading `init.lua`, booting the Lua VM, and entering the TUI
//! event loop are follow-up commits.
//!
//! `anyhow` is allowed here — this is the top boundary. Everywhere below is
//! typed via [`NeforError`].

mod cli;
mod config;
mod error;
mod ids;
mod log;
mod paths;

use anyhow::Context as _;

use crate::error::NeforError;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    log::init().ok();

    let args = cli::parse();
    let config_dir = config::resolve(&args)
        .map_err(NeforError::from)
        .context("resolving config directory")?;

    tracing::info!(config_dir = %config_dir, "nefor starting");

    Ok(())
}
