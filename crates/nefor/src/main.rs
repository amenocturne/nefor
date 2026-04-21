//! `nefor` binary entry point.
//!
//! Per spec §Startup sequence:
//! 1. Resolve config dir (env + args).
//! 2. Bring up the shared [`EventBus`], the shared [`WidgetRegistry`], and
//!    the [`LuaHost`] that owns the `nefor.*` API.
//! 3. If `<config_dir>/init.lua` exists, load it — plugins register widgets,
//!    event handlers, process-spawned children, etc. during that call. Lua
//!    errors that escape the loader are logged and skipped per spec §Error
//!    handling; the binary keeps going with whatever partial state `init.lua`
//!    managed to set up.
//! 4. If no widgets got registered (no init.lua *or* init.lua failed before
//!    touching `nefor.ui.register_widget`), install the no-config placeholder
//!    so the user always sees *something* explaining the state.
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

use std::sync::{Arc, Mutex};

use anyhow::Context as _;

use crate::error::NeforError;
use crate::events::EventBus;
use crate::lua::LuaHost;
use crate::ui::{NoConfigWidget, Region, SharedRegistry, WidgetRegistry};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // CLI + config-dir resolution run *before* logging so we know where to
    // put the log file. Errors here go to stderr via clap / the anyhow chain;
    // we don't have a log destination yet anyway.
    let args = cli::parse();
    let config_dir = config::resolve(&args)
        .map_err(NeforError::from)
        .context("resolving config directory")?;

    let log_path = config_dir.as_path().join("nefor.log");
    if let Err(e) = log::init(&log_path) {
        eprintln!("nefor: failed to initialize logging at {log_path:?}: {e}");
    }

    tracing::info!(config_dir = %config_dir, log_path = %log_path.display(), "nefor starting");

    let bus = Arc::new(EventBus::new());
    let registry: SharedRegistry = Arc::new(Mutex::new(WidgetRegistry::new()));
    let host = LuaHost::new(Arc::clone(&bus), Arc::clone(&registry))
        .map_err(NeforError::from)
        .context("initializing Lua VM")?;

    let init_lua = config_dir.as_path().join("init.lua");
    if init_lua.exists() {
        match host.load_init(&init_lua) {
            Ok(()) => {
                tracing::info!(path = %init_lua.display(), "init.lua loaded");
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
            }
            Err(other) => {
                tracing::error!(
                    path = %init_lua.display(),
                    error = %other,
                    "failed to load init.lua",
                );
            }
        }
    } else {
        tracing::info!(path = %init_lua.display(), "no init.lua at expected path");
    }

    // If no widget claimed a region during init.lua, install the no-config
    // placeholder. This is the "Lua had nothing to say" fallback — once
    // Lua registers a widget we defer to it.
    {
        let mut guard = match registry.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_empty() {
            guard
                .register(
                    Region::Center,
                    Box::new(NoConfigWidget::new(config_dir.clone())),
                )
                .map_err(NeforError::from)
                .context("registering no-config placeholder widget")?;
        }
    }

    ui::app::run(bus.clone(), Arc::clone(&registry))
        .await
        .map_err(NeforError::from)
        .context("running TUI event loop")?;

    // Host stays alive across the event loop so subscribers can keep calling
    // back into Lua; drop it here as the process is about to exit anyway.
    drop(host);

    Ok(())
}
