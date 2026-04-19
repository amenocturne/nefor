//! `nefor` binary entry point.
//!
//! Per spec §Startup sequence:
//! 1. Resolve config dir (env + args).
//! 2. Bring up the shared [`EventBus`] and the [`LuaHost`].
//! 3. If `<config_dir>/init.lua` exists, load it — plugins register widgets,
//!    event handlers, etc. during that call. Lua errors that escape the
//!    loader are logged and skipped per spec §Error handling; the binary
//!    keeps going with whatever partial state `init.lua` managed to set up.
//! 4. If no `init.lua` was loaded (missing file *or* loader error), register
//!    a placeholder pane so the user sees *something* explaining the state.
//! 5. Enter the TUI event loop.
//!
//! `anyhow` is allowed here — this is the top boundary. Everywhere below is
//! typed via [`NeforError`].

mod cli;
mod config;
mod error;
mod events;
mod ids;
mod log;
mod lua;
mod paths;
mod ui;

use std::sync::Arc;

use anyhow::Context as _;

use crate::error::NeforError;
use crate::events::EventBus;
use crate::lua::LuaHost;
use crate::ui::{NoConfigWidget, Region, WidgetRegistry};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    log::init().ok();

    let args = cli::parse();
    let config_dir = config::resolve(&args)
        .map_err(NeforError::from)
        .context("resolving config directory")?;

    tracing::info!(config_dir = %config_dir, "nefor starting");

    let bus = Arc::new(EventBus::new());
    let host = LuaHost::new(Arc::clone(&bus))
        .map_err(NeforError::from)
        .context("initializing Lua VM")?;

    let init_lua = config_dir.as_path().join("init.lua");
    let init_loaded = if init_lua.exists() {
        match host.load_init(&init_lua) {
            Ok(()) => {
                tracing::info!(path = %init_lua.display(), "init.lua loaded");
                true
            }
            Err(lua::LuaError::InitLuaExec { source, location }) => {
                // Per spec: print + continue with partial state. We don't
                // want a single typo in the user's init.lua to wedge the
                // whole TUI before they can see anything.
                match location {
                    Some(loc) => tracing::error!(
                        at = %loc,
                        error = %source,
                        "init.lua execution error (continuing with partial state)",
                    ),
                    None => tracing::error!(
                        path = %init_lua.display(),
                        error = %source,
                        "init.lua execution error (continuing with partial state)",
                    ),
                }
                // Lua may still have registered *some* handlers / widgets
                // before the error — treat that as a real load, not a fresh
                // "no config" state.
                true
            }
            Err(other) => {
                // Read errors, VM init errors: these shouldn't recover to a
                // partial state because nothing got to register. Show the
                // no-config pane and log the reason.
                tracing::error!(
                    path = %init_lua.display(),
                    error = %other,
                    "failed to load init.lua",
                );
                false
            }
        }
    } else {
        tracing::info!(path = %init_lua.display(), "no init.lua at expected path");
        false
    };

    let mut registry = WidgetRegistry::new();
    if !init_loaded {
        // Only fall back to the no-config placeholder if Lua didn't get the
        // chance to register widgets. Once Lua *does* register widgets (next
        // commit wires `nefor.ui.register_widget`), we defer to whatever it
        // set up.
        registry
            .register(
                Region::Center,
                Box::new(NoConfigWidget::new(config_dir.clone())),
            )
            .map_err(NeforError::from)
            .context("registering no-config placeholder widget")?;
    }

    ui::app::run(bus.clone(), registry)
        .await
        .map_err(NeforError::from)
        .context("running TUI event loop")?;

    // Host stays alive across the event loop so subscribers can keep calling
    // back into Lua; drop it here as the process is about to exit anyway.
    drop(host);

    Ok(())
}
