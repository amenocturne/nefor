//! fake-engine — developer harness that impersonates a nefor engine over
//! NCP stdio.
//!
//! Spawns a plugin binary, performs the `attach` → `attach_ok` handshake,
//! then either stays passive (logging every message the plugin emits) or
//! plays back a `.jsonl` script of engine-to-plugin messages. See the
//! top-level README for usage.

mod harness;
mod log;
mod script;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;

use crate::script::parse_script;

/// Developer harness that impersonates a nefor engine over NCP stdio.
///
/// Spawns the given plugin binary, performs the attach handshake, and
/// either stays passive (logging every message the plugin emits) or plays
/// back a .jsonl script of engine-to-plugin messages.
#[derive(Debug, Parser)]
#[command(name = "fake-engine", version, about, long_about)]
struct Args {
    /// Path to the plugin binary to run.
    plugin: PathBuf,

    /// Optional path to a .jsonl script of engine-authored messages to
    /// stream to the plugin after the handshake. If omitted, the harness
    /// just logs whatever the plugin emits and stays connected until the
    /// plugin exits or ctrl-c fires.
    #[arg(long, value_name = "PATH")]
    script: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1)),
        Err(e) => {
            eprintln!("fake-engine: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<i32> {
    let args = Args::parse();

    let script = match &args.script {
        Some(path) => {
            let source = std::fs::read_to_string(path)
                .with_context(|| format!("reading script file {path:?}"))?;
            Some(parse_script(&source).with_context(|| format!("parsing script {path:?}"))?)
        }
        None => None,
    };

    harness::run(&args.plugin, script).await
}
