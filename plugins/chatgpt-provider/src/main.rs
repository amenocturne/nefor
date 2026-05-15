//! chatgpt-provider CLI entry.
//!
//! Two operating modes:
//!
//! - `chatgpt-provider login [--no-open-browser]` — interactive OAuth
//!   bootstrap; persists tokens to disk so subsequent `serve` runs
//!   start in Connected state.
//! - `chatgpt-provider [serve] [--name …] [--model …] [--base-url …]` —
//!   NCP runtime over stdio. The default subcommand: engines spawn the
//!   binary without args and the runtime takes over.

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use nefor_protocol::Envelope;
use tokio::sync::mpsc;

use chatgpt_provider::auth::oauth::run_login;
use chatgpt_provider::auth::store::default_auth_path;
use chatgpt_provider::auth::AuthStore;
use chatgpt_provider::broker::ToolBroker;
use chatgpt_provider::catalog::ToolCatalog;
use chatgpt_provider::config::{Cli, Command, ServeArgs};
use chatgpt_provider::dispatcher::{
    emit_goodbye, emit_startup_events, run_dispatch_loop, send_ready,
};
use chatgpt_provider::error::ChatgptError;
use chatgpt_provider::installation::{default_installation_path, read_or_generate};
use chatgpt_provider::ncp::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, CHANNEL_CAP};
use chatgpt_provider::responses::{ResponsesClient, DEFAULT_ORIGINATOR};
use chatgpt_provider::state::Chats;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Some(Command::Login(args)) => {
            let path = default_auth_path().context("resolving auth file path")?;
            let td = run_login(args.open_browser, &path)
                .await
                .context("OAuth login flow failed")?;
            eprintln!("login complete; tokens at {}", path.display());
            if let Some(acc) = &td.account_id {
                eprintln!("chatgpt_account_id: {acc}");
            }
            Ok(())
        }
        None => {
            // No subcommand → plugin mode. Engines spawn the binary
            // with `--name`/`--base-url` and we take over stdio.
            run_serve(ServeArgs::from(&cli))
                .await
                .map_err(|e| anyhow::Error::new(e).context("serve loop failed"))
        }
    }
}

async fn run_serve(args: ServeArgs) -> Result<(), ChatgptError> {
    let args = Arc::new(args);

    let (out_tx, _writer_handle) = spawn_stdout_writer();
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, ChatgptError>>(CHANNEL_CAP);
    let _reader_handle = spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = await_ready_ok(&mut in_rx).await?;
    tracing::info!(
        engine_version = %engine_version,
        provider = %args.provider_name,
        base_url = %args.base_url,
        "ready"
    );

    // Resolve / generate the per-machine installation id once at
    // startup; passed on every Responses request as
    // `x-codex-installation-id`.
    let inst_path = default_installation_path()?;
    let installation_id = read_or_generate(&inst_path)?;

    let auth_path = default_auth_path()?;
    let auth = Arc::new(AuthStore::load_from_disk(&auth_path).await?);

    // No baked-in default model — the user picks via `/model` (or the
    // engine sends `model.set` on startup); `chat.create` envelopes
    // typically include `model` explicitly, so this default is only
    // consulted for the legacy `prompt` compat path.
    let chats = Arc::new(Chats::with_default_model(None));
    let catalog = Arc::new(ToolCatalog::new());
    let broker = Arc::new(ToolBroker::new());
    let responses_client = Arc::new(ResponsesClient::new(
        args.base_url.clone(),
        installation_id,
        DEFAULT_ORIGINATOR.to_string(),
    ));

    emit_startup_events(&args, &auth, &chats, &responses_client, &out_tx).await?;

    run_dispatch_loop(
        args.clone(),
        chats,
        auth,
        catalog,
        broker,
        responses_client,
        out_tx.clone(),
        in_rx,
    )
    .await?;

    emit_goodbye(&args, &out_tx, "stream closed").await;
    Ok(())
}
