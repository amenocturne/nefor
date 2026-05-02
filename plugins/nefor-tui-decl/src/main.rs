//! nefor-tui-decl — declarative TUI plugin for nefor.
//!
//! Phase-1 scaffold. Compiles, exits clean. Real entrypoint lands in
//! commit 4 along with the lua host + event loop.

mod ansi;
mod desc;
mod engine;
mod error;
mod input;
mod instance;
mod layout;
mod lua_host;
mod ncp;
mod reconciler;
mod render;
mod tty;

use std::process::ExitCode;

use crate::error::TuiError;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "nefor-tui-decl exited with error");
            eprintln!("nefor-tui-decl: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<(), TuiError> {
    tracing::warn!("nefor-tui-decl: phase-1 scaffold; runtime entrypoint lands in commit 4");
    Ok(())
}
