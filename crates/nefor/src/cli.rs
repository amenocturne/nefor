//! Command-line interface — clap derive.
//!
//! Two modes:
//!
//! - **TUI / standard mode** — `nefor` (no subcommand). Engine boots, spawns
//!   plugins per `init.lua`, runs the broker until shutdown.
//! - **CLI dispatch mode** — `nefor plugin [<name> [args...]]`. Engine still
//!   boots normally (so the spawn registry populates), then either lists the
//!   plugins that registered a `cli` field or invokes the named plugin's
//!   `cli` function with the leftover argv.
//!
//! `--config` / `--plugin-dir` are global flags applicable to both modes.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Parsed command-line arguments.
#[derive(Debug, Parser)]
#[command(
    name = "nefor",
    version,
    about = "nefor — TUI agent runtime (Lua plugin host on top of nefor-combinators).",
    long_about = "nefor is a TUI/GUI agent runtime. The binary ships voiceless — \
                  providers, harnesses, DAG orchestration, personas, and statusline \
                  content all live in plugins loaded from the user's init.lua.\n\n\
                  Config lives at $XDG_CONFIG_HOME/nefor/ by default; set \
                  NEFOR_APPNAME=<name> for a parallel profile at \
                  $XDG_CONFIG_HOME/nefor-<name>/, or pass --config <DIR> to \
                  override for one invocation."
)]
pub struct Cli {
    /// Override the config directory (highest precedence; beats `NEFOR_APPNAME`).
    #[arg(long, value_name = "DIR", global = true)]
    pub config: Option<PathBuf>,

    /// Override the plugin root directory (highest precedence; beats
    /// `NEFOR_PLUGIN_DIR` and the XDG / dev fallbacks). Each registered
    /// plugin gets `<this-dir>/<name>/` as its working directory.
    #[arg(long, value_name = "DIR", global = true)]
    pub plugin_dir: Option<PathBuf>,

    /// Optional subcommand. When omitted, the engine runs in TUI / standard
    /// mode (boot init.lua, spawn plugins, broker). When present, see
    /// [`Command`].
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Subcommand selection.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Dispatch to a plugin's `cli` entry point. With no plugin name, lists
    /// plugins that registered a `cli` field. Trailing args are forwarded
    /// verbatim to the plugin's CLI function.
    Plugin {
        /// Name of the plugin to dispatch to. Omitted → list mode.
        name: Option<String>,

        /// Positional args forwarded to the plugin's `cli` function as a
        /// 1-indexed Lua table. `--` is consumed by clap and not preserved.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

// EngineMode lives in `crate::lua::mode` so it can be referenced by the
// binding modules (which are part of the lib surface) without having
// `cli` (binary-only) on their import path.
pub use crate::lua::mode::EngineMode;

/// Derive the engine mode from a parsed [`Cli`].
pub fn engine_mode_from_cli(cli: &Cli) -> EngineMode {
    match &cli.command {
        None => EngineMode::Tui,
        Some(Command::Plugin { name: None, .. }) => EngineMode::PluginList,
        Some(Command::Plugin {
            name: Some(name),
            args,
        }) => EngineMode::PluginDispatch {
            name: name.clone(),
            args: args.clone(),
        },
    }
}

/// Parse CLI arguments from the current process' `argv`.
pub fn parse() -> Cli {
    Cli::parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn no_subcommand_is_tui_mode() {
        let cli = Cli::try_parse_from(["nefor"]).expect("parse ok");
        assert!(cli.command.is_none());
        assert!(matches!(engine_mode_from_cli(&cli), EngineMode::Tui));
    }

    #[test]
    fn plugin_with_no_name_is_list_mode() {
        let cli = Cli::try_parse_from(["nefor", "plugin"]).expect("parse ok");
        assert!(matches!(engine_mode_from_cli(&cli), EngineMode::PluginList));
    }

    #[test]
    fn plugin_with_name_and_args_parses() {
        let cli = Cli::try_parse_from(["nefor", "plugin", "foo", "--bar", "baz", "qux"])
            .expect("parse ok");
        match engine_mode_from_cli(&cli) {
            EngineMode::PluginDispatch { name, args } => {
                assert_eq!(name, "foo");
                assert_eq!(args, vec!["--bar", "baz", "qux"]);
            }
            other => panic!("expected PluginDispatch, got {other:?}"),
        }
    }

    #[test]
    fn plugin_with_name_no_args_parses() {
        let cli = Cli::try_parse_from(["nefor", "plugin", "foo"]).expect("parse ok");
        match engine_mode_from_cli(&cli) {
            EngineMode::PluginDispatch { name, args } => {
                assert_eq!(name, "foo");
                assert!(args.is_empty());
            }
            other => panic!("expected PluginDispatch, got {other:?}"),
        }
    }

    #[test]
    fn global_flags_work_with_subcommand() {
        let cli = Cli::try_parse_from(["nefor", "--config", "/tmp/foo", "plugin", "x"])
            .expect("parse ok");
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/foo"))
        );
        assert!(matches!(
            engine_mode_from_cli(&cli),
            EngineMode::PluginDispatch { .. }
        ));
    }
}
