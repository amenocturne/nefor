//! Command-line interface — clap derive.
//!
//! Intentionally minimal: a single `--config <DIR>` flag for overriding the
//! config directory. Subcommands (e.g. `nefor init`) land in follow-up commits.

use std::path::PathBuf;

use clap::Parser;

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
    #[arg(long, value_name = "DIR")]
    pub config: Option<PathBuf>,

    /// Override the plugin root directory (highest precedence; beats
    /// `NEFOR_PLUGIN_DIR` and the XDG / dev fallbacks). Each registered
    /// plugin gets `<this-dir>/<name>/` as its working directory.
    #[arg(long, value_name = "DIR")]
    pub plugin_dir: Option<PathBuf>,
}

/// Parse CLI arguments from the current process' `argv`.
pub fn parse() -> Cli {
    Cli::parse()
}
