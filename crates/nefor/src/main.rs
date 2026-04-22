//! `nefor` engine entry point.
//!
//! Startup sequence:
//!
//! 1. Parse CLI (`--config <DIR>`).
//! 2. Resolve the config dir, initialize tracing to a file under it.
//! 3. Open a fresh session log (optionally hydrating a parent session
//!    referenced by `nefor.parent_session` after `init.lua` runs).
//! 4. Boot the Lua VM with a [`BrokerOps`] routing sink and run
//!    `init.lua`. Cache the global `step` function — fatal if missing.
//! 5. Build a [`Broker`], spawn every registered plugin through it, install
//!    a `ctrl_c` shutdown hook, and run the broker until it exits.
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
mod session;

use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use nefor_protocol::Timestamp;

use crate::error::NeforError;
use crate::events::EventBus;
use crate::lua::bindings::EngineOps;
use crate::lua::LuaHost;
use crate::ncp::{
    resolve_plugin_root, spawn_plugin, Broker, BrokerOps, BrokerShared, PluginRegistry,
    SharedPluginRegistry,
};
use crate::session::{load_session, SessionError, SessionHeader, SessionId, SessionWriter};

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

    // Open the session file up-front so the broker's shared state can own it
    // from the moment it exists. Parent-session hydration happens *after*
    // init.lua runs, because init.lua is where `nefor.parent_session` is
    // declared.
    let session_id = SessionId::new();
    let header = SessionHeader::new(session_id.clone(), None, Timestamp::now());
    let session = SessionWriter::create(header).context("opening session log")?;
    tracing::info!(session_id = %session_id, path = %session.path().display(), "session log opened");

    let shared = Arc::new(Mutex::new(BrokerShared::new(session)));
    let engine_ops: Arc<dyn EngineOps> = Arc::new(BrokerOps::new(Arc::clone(&shared)));

    let bus = Arc::new(EventBus::new());
    let plugins: SharedPluginRegistry = Arc::new(Mutex::new(PluginRegistry::new()));
    let mut host = LuaHost::new(Arc::clone(&bus), Arc::clone(&plugins), engine_ops)
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

    // Hydrate a parent session if init.lua set `nefor.parent_session`. Fatal
    // if the id is present but malformed, or the session file is missing /
    // malformed — the user explicitly asked to resume, so silent fallback
    // would be worse than a loud exit.
    let saved_log = match load_parent_session(&host) {
        Ok(v) => v,
        Err(e) => return Err(NeforError::Session(e).into()),
    };
    if !saved_log.is_empty() {
        tracing::info!(entries = saved_log.len(), "parent session hydrated");
    }

    // Cache step now — fatal if init.lua didn't define one.
    host.cache_step()
        .map_err(NeforError::from)
        .context("caching step function from init.lua")?;

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

    let mut broker = Broker::with_saved_log(Arc::clone(&shared), host, saved_log);
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

    Ok(())
}

/// Read the global `nefor.parent_session` string (if any) and load the
/// referenced session log. Returns an empty vec when no parent is declared.
fn load_parent_session(host: &LuaHost) -> Result<Vec<session::LogEntry>, SessionError> {
    let nefor: mlua::Table = match host.lua().globals().get("nefor") {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };
    let parent: mlua::Value = match nefor.get("parent_session") {
        Ok(v) => v,
        Err(_) => return Ok(Vec::new()),
    };
    let parent = match parent {
        mlua::Value::Nil => return Ok(Vec::new()),
        mlua::Value::String(s) => s
            .to_str()
            .map_err(|e| SessionError::InvalidSessionId {
                raw: "<non-utf8>".to_string(),
                reason: e.to_string(),
            })?
            .to_owned(),
        other => {
            return Err(SessionError::InvalidSessionId {
                raw: format!("<{}>", other.type_name()),
                reason: "nefor.parent_session must be a string".to_string(),
            });
        }
    };
    let id = SessionId::parse(&parent)?;
    let loaded = load_session(&id)?;
    Ok(loaded.entries)
}
