//! `nefor` engine entry point.
//!
//! Startup sequence:
//!
//! 1. Parse CLI (`--config`, `--data-dir`, `--plugin-dir`, optional `plugin` subcommand).
//! 2. Resolve the config dir, initialize tracing to a file under it.
//! 3. Boot the Lua VM with a [`BrokerOps`] routing sink and run
//!    `init.lua`. Cache the global `dispatch` function — fatal if missing.
//! 4. Branch on [`cli::EngineMode`]:
//!    - `Tui`: build a [`Broker`], spawn every registered plugin, install
//!      a `ctrl_c` shutdown hook, and run the broker until it exits.
//!    - `PluginList`: print the engine version + every plugin that
//!      registered a `cli` function, exit 0.
//!    - `PluginDispatch`: spawn every registered subprocess plugin, find
//!      the named virtual or hybrid plugin's `cli` function, invoke it
//!      with the leftover argv. The broker continues running afterwards
//!      so registered `nefor.bus.on_event` handlers receive events; the
//!      cli function (or a handler) calls `nefor.engine.exit(code)` to
//!      shut down.
//!
//! Per D-02 the engine is pure glue: no plugins registered → log a message
//! and exit cleanly. No UI, no bundled harness. The engine is fully
//! session-blind: it owns no session id, writes no on-disk log, and does
//! not parse envelope bodies. Cross-session persistence / resumption /
//! impersonation are the responsibility of `starter/sessions.lua`.

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

use crate::cli::{engine_mode_from_cli, EngineMode};
use crate::error::NeforError;
use crate::events::EventBus;
use crate::lua::bindings::EngineOps;
use crate::lua::LuaHost;
use crate::ncp::{
    resolve_plugin_root, spawn_plugin, Broker, BrokerOps, BrokerShared, PluginRegistry, PluginSpec,
    SharedPluginRegistry,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = cli::parse();
    let mode = engine_mode_from_cli(&args);
    let config_dir = config::resolve_config(&args)
        .map_err(NeforError::from)
        .context("resolving config directory")?;

    // Resolve the data dir and propagate it to the environment so every
    // spawned plugin subprocess inherits the canonical value. The CLI flag
    // takes highest precedence; if neither flag nor env var is set, the
    // XDG default lands in the env var so plugin processes see the same
    // path the engine resolved (otherwise a plugin reading
    // `os.getenv("NEFOR_DATA_DIR")` on a default-layout install would get
    // nil and silently fall back to its own resolver).
    let data_dir = config::resolve_data(&args)
        .map_err(NeforError::from)
        .context("resolving data directory")?;
    // SAFETY: single-threaded at this point, before any thread spawns.
    unsafe {
        std::env::set_var("NEFOR_DATA_DIR", data_dir.as_path());
    }

    // Resolve and propagate NEFOR_PLUGIN_DIR so init.lua's `bin()` helper
    // can find the right path before any plugin gets spawned. The starter
    // reads `os.getenv("NEFOR_PLUGIN_DIR")` directly; setting it here means
    // `<exe>/../share/nefor/plugins` (Homebrew layout) and `<exe-dir>`
    // (in-tree dev) both work without per-install configuration.
    let plugin_root_unset = std::env::var("NEFOR_PLUGIN_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .is_none();
    if plugin_root_unset {
        if let Some(root) = resolve_plugin_root(args.plugin_dir.clone()) {
            // SAFETY: single-threaded at this point, before any thread spawns.
            unsafe {
                std::env::set_var("NEFOR_PLUGIN_DIR", root.as_path());
            }
        }
    }

    let log_path = config_dir.as_path().join("nefor.log");
    if let Err(e) = log::init(&log_path) {
        eprintln!("nefor: failed to initialize logging at {log_path:?}: {e}");
    }

    tracing::info!(
        config_dir = %config_dir,
        data_dir = %data_dir,
        log_path = %log_path.display(),
        ?mode,
        "nefor starting"
    );

    // The engine owns no session id and writes no jsonl — those concerns
    // live in `starter/sessions.lua`. The broker's shared state is purely
    // an in-memory event log + connection map.
    let shared = Arc::new(Mutex::new(BrokerShared::new()));
    let engine_ops: Arc<dyn EngineOps> = Arc::new(BrokerOps::new(Arc::clone(&shared)));

    let bus = Arc::new(EventBus::new());
    let plugins: SharedPluginRegistry = Arc::new(Mutex::new(PluginRegistry::new()));
    let mut host = LuaHost::new(
        Arc::clone(&bus),
        Arc::clone(&plugins),
        engine_ops,
        data_dir.clone(),
    )
    .map_err(NeforError::from)
    .context("initializing Lua VM")?;

    // CLI dispatch vs TUI: the bindings need to know the active mode so
    // `nefor.io.read_line` short-circuits to nil in TUI (where stdin is
    // unused) instead of blocking on a channel nothing pumps.
    host.set_mode(mode.clone());

    let init_lua = config_dir.as_path().join("init.lua");
    if init_lua.exists() {
        match host.load_init(&init_lua).await {
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
        eprintln!(
            "nefor: no init.lua found at {}. \
             Set NEFOR_CONFIG_DIR or pass --config <DIR> pointing at a directory \
             that contains init.lua. See README for install instructions.",
            init_lua.display()
        );
        std::process::exit(1);
    }

    // Cache dispatch now — fatal if init.lua didn't define one.
    host.cache_dispatch()
        .map_err(NeforError::from)
        .context("caching dispatch function from init.lua")?;

    match mode {
        EngineMode::Tui => run_tui(host, plugins, shared, args.plugin_dir.clone()).await,
        EngineMode::PluginList => run_plugin_list(plugins),
        EngineMode::PluginDispatch { name, args: argv } => {
            run_plugin_dispatch(host, plugins, shared, args.plugin_dir.clone(), name, argv).await
        }
    }
}

/// Standard run loop — broker until shutdown.
async fn run_tui(
    host: LuaHost,
    plugins: SharedPluginRegistry,
    shared: Arc<Mutex<BrokerShared>>,
    plugin_dir_override: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let specs = drain_specs(&plugins);

    if specs.is_empty() {
        tracing::info!(
            "no plugins registered; exiting. See starter/init.lua for an example config."
        );
        drop(host);
        return Ok(());
    }

    let plugin_root = match resolve_plugin_root(plugin_dir_override) {
        Some(r) => r,
        None => {
            tracing::error!(
                "could not resolve plugin root directory; set NEFOR_PLUGIN_DIR or pass --plugin-dir"
            );
            return Ok(());
        }
    };
    tracing::info!(plugin_root = %plugin_root.as_path().display(), "plugin root resolved");

    let mut broker = Broker::new(Arc::clone(&shared), host);
    spawn_specs(&mut broker, &specs, &plugin_root);

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

/// `nefor plugin` (no name) — print engine + every plugin with a `cli`.
fn run_plugin_list(plugins: SharedPluginRegistry) -> anyhow::Result<()> {
    let names: Vec<String> = {
        let guard = match plugins.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard
            .list_with_cli()
            .into_iter()
            .map(|n| n.as_str().to_owned())
            .collect()
    };
    println!("nefor {}", env!("CARGO_PKG_VERSION"));
    if names.is_empty() {
        println!("(no plugins with a `cli` field registered)");
    } else {
        println!("plugins with cli:");
        for n in names {
            println!("  {n}");
        }
    }
    Ok(())
}

/// `nefor plugin <name> [args...]` — boot, spawn, then call the named
/// `cli` function. Subsequent broker work continues until `engine.exit`.
async fn run_plugin_dispatch(
    host: LuaHost,
    plugins: SharedPluginRegistry,
    shared: Arc<Mutex<BrokerShared>>,
    plugin_dir_override: Option<std::path::PathBuf>,
    name: String,
    argv: Vec<String>,
) -> anyhow::Result<()> {
    let specs = drain_specs(&plugins);

    if let Err(e) = lookup_dispatch_target(&specs, &name) {
        eprintln!("{e}");
        std::process::exit(1);
    }

    let plugin_root = match resolve_plugin_root(plugin_dir_override) {
        Some(r) => r,
        None => {
            tracing::error!(
                "could not resolve plugin root directory; set NEFOR_PLUGIN_DIR or pass --plugin-dir"
            );
            std::process::exit(1);
        }
    };
    tracing::info!(plugin_root = %plugin_root.as_path().display(), "plugin root resolved");

    // Bridge the binary's stdin into nefor.io.read_line. Done before
    // the broker takes ownership of the host so the read_line binding
    // sees the pump receiver before the cli function ever runs.
    let stdin_rx = lua::bindings::spawn_stdin_pump();
    host.attach_stdin_pump(stdin_rx);

    let mut broker = Broker::new(Arc::clone(&shared), host);
    spawn_specs(&mut broker, &specs, &plugin_root);

    let shutdown = broker.shutdown_handle();
    let ctrl_c_task = tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            tracing::info!("ctrl_c received; requesting broker shutdown");
            shutdown
                .shutdown(crate::ncp::broker::DEFAULT_SHUTDOWN_GRACE_MS)
                .await;
        }
    });

    // The cli function runs synchronously before the broker enters its
    // run loop. The broker drives step (and on_event handlers)
    // afterwards; engine.exit signals the shutdown handle and stashes
    // the requested exit code which we propagate to process::exit.
    let exit_code = broker.run_with_cli_dispatch(&name, &argv).await;
    ctrl_c_task.abort();
    std::process::exit(exit_code);
}

/// Failure modes for CLI dispatch lookup. Closed enum (D-16) so callers
/// pattern-match on the cause rather than substring-sniffing the
/// formatted message.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DispatchLookupError {
    /// No spec with this name was registered. Either init.lua never
    /// declared it, or `nefor plugin <name>` had a typo.
    #[error("plugin '{0}' is not registered")]
    Unknown(String),
    /// The spec exists but is purely a subprocess plugin — no `cli`
    /// function was provided. CLI dispatch can't proceed.
    #[error("plugin '{0}' has no CLI")]
    NoCli(String),
}

/// Resolve `name` against `specs`. Returns Ok if a dispatchable target
/// exists. Tested in isolation so the dispatch path's user-facing
/// error messages stay pinned.
pub fn lookup_dispatch_target<'a>(
    specs: &'a [PluginSpec],
    name: &str,
) -> Result<&'a PluginSpec, DispatchLookupError> {
    let target = specs
        .iter()
        .find(|s| s.name.as_str() == name)
        .ok_or_else(|| DispatchLookupError::Unknown(name.to_owned()))?;
    if !target.has_cli {
        return Err(DispatchLookupError::NoCli(name.to_owned()));
    }
    Ok(target)
}

/// Drain the registry under the lock; returns the contained specs.
fn drain_specs(plugins: &SharedPluginRegistry) -> Vec<PluginSpec> {
    let mut guard = match plugins.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.drain()
}

/// Spawn every spec that carries a `command`, attaching the resulting
/// transport to the broker. Virtual specs (no command) are skipped — they
/// exist only as CLI entry points.
fn spawn_specs(broker: &mut Broker, specs: &[PluginSpec], plugin_root: &ncp::PluginRoot) {
    for spec in specs {
        if spec.command.is_none() {
            tracing::debug!(plugin = %spec.name, "skipping virtual plugin spawn (no command)");
            continue;
        }
        match spawn_plugin(spec, plugin_root) {
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
                // Surface the spawn failure to the bus so step can translate
                // it into a user-visible notification (e.g. a chat.popup).
                let code = match &e {
                    crate::ncp::BrokerError::MissingPluginDir { .. } => "missing_dir",
                    crate::ncp::BrokerError::Spawn { .. } => "spawn_failed",
                    crate::ncp::BrokerError::Io(_) => "io_error",
                };
                broker.queue_engine_envelope(serde_json::json!({
                    "kind":   "engine.plugin_failed",
                    "plugin": spec.name.as_str(),
                    "phase":  "spawn",
                    "reason": e.to_string(),
                    "code":   code,
                }));
            }
        }
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use nefor_protocol::PluginName;

    fn spec(name: &str, has_cli: bool) -> PluginSpec {
        PluginSpec {
            name: PluginName::new(name).expect("valid"),
            command: Some(vec!["echo".into()]),
            has_cli,
        }
    }

    #[test]
    fn lookup_finds_named_target_with_cli() {
        let specs = vec![spec("a", false), spec("b", true)];
        let got = lookup_dispatch_target(&specs, "b").expect("found");
        assert_eq!(got.name.as_str(), "b");
    }

    #[test]
    fn lookup_unknown_name_errors_with_named_variant() {
        let specs = vec![spec("a", true)];
        let err = lookup_dispatch_target(&specs, "ghost").expect_err("must error");
        assert_eq!(err, DispatchLookupError::Unknown("ghost".into()));
        // The display form is what the binary prints to stderr — pin it
        // so users can rely on the wording.
        assert_eq!(err.to_string(), "plugin 'ghost' is not registered");
    }

    #[test]
    fn lookup_existing_without_cli_errors_with_named_variant() {
        let specs = vec![spec("a", false)];
        let err = lookup_dispatch_target(&specs, "a").expect_err("must error");
        assert_eq!(err, DispatchLookupError::NoCli("a".into()));
        assert_eq!(err.to_string(), "plugin 'a' has no CLI");
    }
}
