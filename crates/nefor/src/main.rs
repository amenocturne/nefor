//! `nefor` engine entry point.
//!
//! Startup sequence:
//!
//! 1. Parse CLI (`--config <DIR>`).
//! 2. Resolve the config dir, initialize tracing to a file under it.
//! 3. Boot the Lua VM and — if `init.lua` exists — run it. `init.lua` calls
//!    `nefor.plugins.spawn { ... }` for every plugin the user wants to run.
//! 4. Build a [`Broker`](crate::ncp::Broker), spawn every registered plugin
//!    through it, install a `ctrl_c` shutdown hook, and run the broker until
//!    it exits.
//!
//! Per D-02 the engine is pure glue: no plugins registered → log a message
//! and exit cleanly. No UI, no bundled harness.

mod cli;
mod config;
mod error;
mod events;
mod ids;
mod log;
mod lua;
mod ncp;
mod paths;

use std::sync::{Arc, Mutex};

use anyhow::Context as _;

use crate::error::NeforError;
use crate::events::EventBus;
use crate::lua::bindings::{EngineOps, NoopEngineOps};
use crate::lua::LuaHost;
use crate::ncp::{resolve_plugin_root, spawn_plugin, Broker, PluginRegistry, SharedPluginRegistry};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = cli::parse();
    let config_dir = config::resolve(&args)
        .map_err(NeforError::from)
        .context("resolving config directory")?;

    let log_path = config_dir.as_path().join("nefor.log");
    if let Err(e) = log::init(&log_path) {
        eprintln!("nefor: failed to initialize logging at {log_path:?}: {e}");
    }

    tracing::info!(
        config_dir = %config_dir,
        log_path = %log_path.display(),
        "nefor starting"
    );

    let bus = Arc::new(EventBus::new());
    let plugins: SharedPluginRegistry = Arc::new(Mutex::new(PluginRegistry::new()));
    // Slice 2 I2: step function + nefor.engine.send are installed, but the
    // broker-backed routing sink lands in I3. Until then the VM holds a
    // NoopEngineOps — calls from `step` are logged and dropped.
    let engine_ops: Arc<dyn EngineOps> = Arc::new(NoopEngineOps);
    let host = LuaHost::new(Arc::clone(&bus), Arc::clone(&plugins), engine_ops)
        .map_err(NeforError::from)
        .context("initializing Lua VM")?;

    let init_lua = config_dir.as_path().join("init.lua");
    if init_lua.exists() {
        match host.load_init(&init_lua) {
            Ok(()) => {
                tracing::info!(path = %init_lua.display(), "init.lua loaded");
            }
            Err(lua::LuaError::InitLuaExec { source, location }) => match location {
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
            },
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

    let specs = {
        let mut guard = match plugins.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.drain()
    };

    if specs.is_empty() {
        tracing::info!(
            config_dir = %config_dir,
            "no plugins registered; exiting. See starter/init.lua for an example config."
        );
        drop(host);
        return Ok(());
    }

    let plugin_root = match resolve_plugin_root(args.plugin_dir.clone()) {
        Some(r) => r,
        None => {
            tracing::error!(
                "could not resolve plugin root directory; set NEFOR_PLUGIN_DIR or pass --plugin-dir"
            );
            return Ok(());
        }
    };
    tracing::info!(plugin_root = %plugin_root.as_path().display(), "plugin root resolved");

    let mut broker = Broker::new(env!("CARGO_PKG_VERSION"));
    for spec in &specs {
        match spawn_plugin(spec, &plugin_root) {
            Ok(transport) => {
                let id = broker.attach_transport(transport, spec.name.clone());
                tracing::info!(
                    plugin = %spec.name,
                    command = ?spec.command,
                    conn = %id,
                    "plugin spawned"
                );
            }
            Err(e) => {
                tracing::error!(
                    plugin = %spec.name,
                    command = ?spec.command,
                    error = %e,
                    "failed to spawn plugin"
                );
            }
        }
    }

    let shutdown = broker.shutdown_handle();
    let ctrl_c_task = tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            tracing::info!("ctrl_c received; requesting broker shutdown");
            shutdown
                .shutdown(crate::ncp::broker::DEFAULT_SHUTDOWN_GRACE_MS)
                .await;
        }
    });

    let stop_reason = broker.run().await;
    tracing::info!(?stop_reason, "broker stopped");
    ctrl_c_task.abort();

    drop(host);
    Ok(())
}
