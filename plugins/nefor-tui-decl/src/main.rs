//! nefor-tui-decl binary entrypoint.
//!
//! Phase-1 scaffold. Real NCP handshake + render loop lands in commit 4.

use std::process::ExitCode;

use nefor_tui_decl::error::TuiError;

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
