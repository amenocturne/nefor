//! CLI surface for the chatgpt-provider binary.
//!
//! The binary runs in one of two modes:
//!
//! - Plugin mode (default — no subcommand). Engines spawn the binary
//!   directly with `--name <prefix>` and optional `--base-url`. The
//!   binary takes over stdio for NCP. **No `--model` flag**: the model
//!   list is fetched from the backend at runtime and the user picks
//!   via `/model` in the chat surface.
//! - `login` subcommand. Interactive OAuth bootstrap; persists tokens
//!   to `$XDG_DATA_HOME/nefor/chatgpt-auth.json` and exits.

use clap::{Parser, Subcommand};

/// Default plugin identity / event-kind prefix.
pub const DEFAULT_PROVIDER_NAME: &str = "chatgpt";

/// Internal fallback model id used when a `chat.create` arrives with
/// no `model` field set. Not a CLI flag — the user's selection via
/// `/model` overrides this.
pub const DEFAULT_MODEL: &str = "gpt-5-codex";

/// Default base URL for the Responses endpoint on the ChatGPT-
/// subscription path. Overridable via `--base-url` so tests can point
/// at a wiremock instance.
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

#[derive(Debug, Clone, Parser)]
#[command(
    name = "chatgpt-provider",
    about = "NCP plugin: talks to OpenAI's Responses API with ChatGPT-subscription OAuth credentials."
)]
pub struct Cli {
    /// Per-instance identity used as the event-kind prefix. With
    /// `--name chatgpt` the plugin emits `chatgpt.hello`,
    /// `chatgpt.stream.delta`, … and consumes `chatgpt.prompt`.
    #[arg(long = "name", default_value = DEFAULT_PROVIDER_NAME, global = true)]
    pub provider_name: String,

    /// Override the Responses endpoint base URL. The full URL becomes
    /// `{base}/responses` and `{base}/models`.
    #[arg(long = "base-url", default_value = DEFAULT_BASE_URL, value_parser = trim_trailing_slash, global = true)]
    pub base_url: String,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Run the OAuth PKCE login flow and persist tokens to disk.
    Login(LoginArgs),
}

#[derive(Debug, Clone, clap::Args)]
pub struct LoginArgs {
    /// Print the authorize URL instead of opening a browser (useful
    /// over SSH).
    #[arg(long, default_value_t = true)]
    pub open_browser: bool,
}

/// Plugin runtime configuration. Built from [`Cli`] at startup; not
/// itself a clap-parsed struct so that downstream callers (dispatcher,
/// tests) can construct it directly.
#[derive(Debug, Clone)]
pub struct ServeArgs {
    pub provider_name: String,
    pub base_url: String,
    /// Internal default for chats that don't pin their own model. The
    /// real model list is fetched from `/models` at runtime; this is
    /// the fallback used when the user hasn't picked yet.
    pub model: String,
}

impl ServeArgs {
    /// Event-kind prefix derived from `provider_name`, including the
    /// trailing dot. e.g. `provider_name = "chatgpt"` → `"chatgpt."`.
    pub fn event_prefix(&self) -> String {
        format!("{}.", self.provider_name)
    }
}

impl From<&Cli> for ServeArgs {
    fn from(cli: &Cli) -> Self {
        Self {
            provider_name: cli.provider_name.clone(),
            base_url: cli.base_url.clone(),
            model: DEFAULT_MODEL.to_string(),
        }
    }
}

fn trim_trailing_slash(s: &str) -> Result<String, String> {
    Ok(s.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_subcommand_parses_with_defaults() {
        let cli = Cli::try_parse_from(["chatgpt-provider"]).expect("parse");
        assert!(cli.command.is_none());
        assert_eq!(cli.provider_name, DEFAULT_PROVIDER_NAME);
        assert_eq!(cli.base_url, DEFAULT_BASE_URL);
    }

    #[test]
    fn name_flag_overrides_provider_name() {
        let cli = Cli::try_parse_from(["chatgpt-provider", "--name", "alt"]).expect("parse");
        assert_eq!(cli.provider_name, "alt");
    }

    #[test]
    fn base_url_trims_trailing_slash() {
        let cli =
            Cli::try_parse_from(["chatgpt-provider", "--base-url", "https://example.com/api/"])
                .expect("parse");
        assert_eq!(cli.base_url, "https://example.com/api");
    }

    #[test]
    fn login_subcommand_parses() {
        let cli = Cli::try_parse_from(["chatgpt-provider", "login"]).expect("parse");
        assert!(matches!(cli.command, Some(Command::Login(_))));
    }

    #[test]
    fn serve_args_built_from_cli() {
        let cli = Cli::try_parse_from(["chatgpt-provider", "--name", "alt"]).expect("parse");
        let serve: ServeArgs = (&cli).into();
        assert_eq!(serve.provider_name, "alt");
        assert_eq!(serve.model, DEFAULT_MODEL);
        assert_eq!(serve.event_prefix(), "alt.");
    }
}
