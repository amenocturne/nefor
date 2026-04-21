//! mock-plugin — Lua-scriptable NCP v0.1 peer plugin.
//!
//! A test/dev plugin that speaks [NCP v0.1](../../../protocol/v0.1/spec.md)
//! over stdio and drives its behaviour from a user-supplied Lua script.
//! Useful for exercising the engine and other plugins without having to
//! ship real functionality.

mod error;
mod lua_host;
mod ncp;
mod state;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use crate::error::MockError;

/// Hardcoded plugin identity. Exposed to Lua as `nefor.name`. If callers
/// need multiple `mock-plugin` instances under distinct wire identities,
/// extend this with a `--name` flag.
pub const PLUGIN_NAME: &str = "mock-plugin";

/// NCP version this plugin speaks.
pub const PROTOCOL_VERSION: &str = "0.1";

/// Lua-scriptable NCP v0.1 peer plugin for test/dev.
#[derive(Debug, Parser)]
#[command(name = "mock-plugin", version, about, long_about)]
struct Args {
    /// Path to the Lua script that defines this plugin's handlers.
    #[arg(long, value_name = "PATH")]
    script: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    // Logs go to stderr — stdout is the NCP channel. The default filter
    // is "info" so operators see handshake/shutdown lifecycle without
    // needing to set RUST_LOG.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "mock-plugin exited with error");
            eprintln!("mock-plugin: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<(), MockError> {
    let args = Args::parse();

    if !args.script.exists() {
        return Err(MockError::ScriptNotFound(args.script));
    }

    let script_src = std::fs::read_to_string(&args.script).map_err(MockError::ScriptRead)?;

    // Rest of the binary (ncp plumbing, lua host) lands in the next commit.
    // For now, prove the skeleton builds end-to-end and surfaces errors.
    let _ = script_src;
    Ok(())
}
