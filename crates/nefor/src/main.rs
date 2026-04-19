//! `nefor` binary entry point.
//!
//! Per spec §Startup sequence:
//! 1. Resolve config dir (env + args).
//! 2. Check for `<config_dir>/init.lua` and register a placeholder widget that
//!    tells the user what the binary sees. Reading + executing `init.lua` is
//!    a subsequent commit; this commit only surfaces the decision.
//! 3. Enter the TUI event loop.
//!
//! `anyhow` is allowed here — this is the top boundary. Everywhere below is
//! typed via [`NeforError`].

mod cli;
mod config;
mod error;
mod ids;
mod log;
mod paths;
mod ui;

use anyhow::Context as _;

use crate::error::NeforError;
use crate::ui::{InitLuaFoundWidget, NoConfigWidget, Region, WidgetRegistry};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    log::init().ok();

    let args = cli::parse();
    let config_dir = config::resolve(&args)
        .map_err(NeforError::from)
        .context("resolving config directory")?;

    tracing::info!(config_dir = %config_dir, "nefor starting");

    let init_lua = config_dir.as_path().join("init.lua");
    let mut registry = WidgetRegistry::new();

    if init_lua.exists() {
        tracing::info!(path = %init_lua.display(), "found init.lua (loader not yet wired)");
        registry
            .register(Region::Center, Box::new(InitLuaFoundWidget::new(init_lua)))
            .map_err(NeforError::from)
            .context("registering init.lua-found placeholder widget")?;
    } else {
        tracing::info!(path = %init_lua.display(), "no init.lua at expected path");
        registry
            .register(
                Region::Center,
                Box::new(NoConfigWidget::new(config_dir.clone())),
            )
            .map_err(NeforError::from)
            .context("registering no-config placeholder widget")?;
    }

    ui::app::run(registry)
        .await
        .map_err(NeforError::from)
        .context("running TUI event loop")?;

    Ok(())
}
