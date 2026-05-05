//! Plugin configuration.
//!
//! Resolved at startup from CLI flags. The chat-completions endpoint URL
//! is derived from `base_url` by appending `/v1/chat/completions` — this
//! lets the same plugin point at any OpenAI-compatible backend (Ollama,
//! Groq, OpenRouter, OpenAI, vLLM) by changing two flags.
//!
//! `provider_name` is the per-instance identity. NCP can spawn the same
//! binary N times under different plugin names; each spawn passes a
//! distinct `--name` so the emitted/consumed event kinds
//! (`<name>.prompt`, `<name>.stream.delta`, …) don't collide on the bus.
//!
//! Why CLI flags and not env vars: the engine's `nefor.plugins.spawn`
//! intentionally does not propagate per-instance env to child processes,
//! so per-instance config has to ride on the command line. `--api-key`
//! still falls back to `OPENAI_PROVIDER_API_KEY` so users can keep
//! secrets out of init.lua.
//!
//! See `starter/openai-providers-example.lua` for the multi-instance
//! recipe.
//!
//! # Example
//!
//! ```ignore
//! openai-provider \
//!   --name ollama \
//!   --base-url http://localhost:11434 \
//!   --model phi4-mini:latest
//! ```

use clap::Parser;

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(
    name = "openai-provider",
    about = "NCP v0.1 plugin: streams OpenAI-compatible chat completions back as chat-contract events.",
    long_about = "\
NCP v0.1 plugin that talks to any OpenAI-compatible chat-completions \
endpoint (Ollama, Groq, OpenRouter, OpenAI, vLLM, …), streaming text \
deltas back as chat-contract events.

Spawn the same binary N times under different `--name` values to wire up \
multiple providers from one binary. The name is used as the per-instance \
event-kind prefix (`<name>.prompt`, `<name>.stream.delta`, …) so spawns \
don't collide on the bus.\
"
)]
pub struct Config {
    /// Per-instance identity used as the event-kind prefix. With
    /// `--name ollama` the plugin emits `ollama.hello`,
    /// `ollama.stream.delta`, … and consumes `ollama.prompt`.
    #[arg(long = "name", default_value = "openai")]
    pub provider_name: String,

    /// Base URL for the OpenAI-compatible server. Defaults to Ollama's
    /// local listen address. Trailing slash trimmed automatically.
    #[arg(long = "base-url", default_value = "http://localhost:11434", value_parser = trim_trailing_slash)]
    pub base_url: String,

    /// Default model id to ask for on each request. Optional: if unset,
    /// each `chat.create` must carry its own `model` field, or the turn
    /// errors with a clear message instead of dispatching against a
    /// hard-coded fallback the user may not have pulled.
    #[arg(long)]
    pub model: Option<String>,

    /// Optional bearer token for hosted OpenAI-compatible providers.
    /// Ollama does not require one. Falls back to the
    /// `OPENAI_PROVIDER_API_KEY` env var so secrets can stay out of
    /// init.lua.
    #[arg(long = "api-key", env = "OPENAI_PROVIDER_API_KEY")]
    pub api_key: Option<String>,
}

/// Map `--name` to the public `provider_name` flag. Clap's derive uses
/// the field name (with underscores swapped for dashes) for the long
/// form by default; we want the user-facing flag to be `--name`.
impl Config {
    pub fn from_args() -> Self {
        let mut cfg = Self::parse();
        cfg.normalize();
        cfg
    }

    /// Normalize fields after parsing — drop empty api_key strings,
    /// trim a trailing slash from base_url if a custom value-parser was
    /// bypassed (e.g. `try_parse_from`).
    fn normalize(&mut self) {
        if self
            .api_key
            .as_deref()
            .map(|s| s.is_empty())
            .unwrap_or(false)
        {
            self.api_key = None;
        }
        self.base_url = self.base_url.trim_end_matches('/').to_string();
    }

    /// Full endpoint URL for chat completions.
    pub fn chat_endpoint(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url)
    }

    /// Event-kind prefix derived from `provider_name`, including the
    /// trailing dot. e.g. `provider_name = "ollama"` → `"ollama."`.
    pub fn event_prefix(&self) -> String {
        format!("{}.", self.provider_name)
    }
}

fn trim_trailing_slash(s: &str) -> Result<String, String> {
    Ok(s.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str) -> Config {
        Config {
            provider_name: name.into(),
            base_url: "http://localhost:11434".into(),
            model: Some("qwen2.5-coder:7b".into()),
            api_key: None,
        }
    }

    #[test]
    fn chat_endpoint_appends_path() {
        let c = cfg("ollama");
        assert_eq!(
            c.chat_endpoint(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn chat_endpoint_no_double_slash() {
        let c = cfg("ollama");
        // base_url has no trailing slash by construction.
        assert!(!c.chat_endpoint().contains("//v1"));
    }

    #[test]
    fn event_prefix_includes_trailing_dot() {
        assert_eq!(cfg("ollama").event_prefix(), "ollama.");
        assert_eq!(cfg("groq").event_prefix(), "groq.");
    }

    /// Tests touch the process env (via clap's env-fallback for `--api-key`),
    /// so every parse-from-args test guards with this mutex to keep them
    /// from racing when cargo runs the suite in parallel.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn clear_env() {
        std::env::remove_var("OPENAI_PROVIDER_API_KEY");
    }

    /// Parse with the env lock held + the OPENAI_PROVIDER_API_KEY env var
    /// cleared, so default-case assertions about `api_key` aren't racing
    /// the env-fallback tests.
    fn parse_clean(args: &[&str]) -> (std::sync::MutexGuard<'static, ()>, Config) {
        let g = env_lock();
        clear_env();
        let mut c = Config::try_parse_from(args).expect("parse args");
        c.normalize();
        (g, c)
    }

    #[test]
    fn defaults_have_no_model() {
        let (_g, c) = parse_clean(&["openai-provider"]);
        assert_eq!(c.provider_name, "openai");
        assert_eq!(c.base_url, "http://localhost:11434");
        assert!(c.model.is_none(), "model is opt-in; no compiled-in fallback");
        assert!(c.api_key.is_none());
    }

    #[test]
    fn name_flag_overrides_event_prefix() {
        let (_g, c) = parse_clean(&["openai-provider", "--name", "groq"]);
        assert_eq!(c.provider_name, "groq");
        assert_eq!(c.event_prefix(), "groq.");
    }

    #[test]
    fn base_url_flag_trims_trailing_slash() {
        let (_g, c) = parse_clean(&[
            "openai-provider",
            "--base-url",
            "https://api.groq.com/openai/",
        ]);
        assert_eq!(c.base_url, "https://api.groq.com/openai");
    }

    #[test]
    fn model_flag_passes_through_verbatim() {
        let (_g, c) = parse_clean(&["openai-provider", "--model", "llama-3.3-70b-versatile"]);
        assert_eq!(c.model.as_deref(), Some("llama-3.3-70b-versatile"));
    }

    #[test]
    fn api_key_flag_explicit_value() {
        let (_g, c) = parse_clean(&["openai-provider", "--api-key", "sk-fake"]);
        assert_eq!(c.api_key.as_deref(), Some("sk-fake"));
    }

    #[test]
    fn full_recipe_parses_cleanly() {
        let (_g, c) = parse_clean(&[
            "openai-provider",
            "--name",
            "ollama",
            "--base-url",
            "http://localhost:11434",
            "--model",
            "phi4-mini:latest",
        ]);
        assert_eq!(c.provider_name, "ollama");
        assert_eq!(c.base_url, "http://localhost:11434");
        assert_eq!(c.model.as_deref(), Some("phi4-mini:latest"));
        assert_eq!(c.event_prefix(), "ollama.");
        assert!(c.api_key.is_none());
    }

    #[test]
    fn api_key_falls_back_to_env_var() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("OPENAI_PROVIDER_API_KEY", "env-secret");
        let mut c = Config::try_parse_from(["openai-provider"]).expect("parse args");
        c.normalize();
        assert_eq!(c.api_key.as_deref(), Some("env-secret"));
        clear_env();
    }

    #[test]
    fn explicit_flag_beats_env_var() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("OPENAI_PROVIDER_API_KEY", "env-secret");
        let mut c = Config::try_parse_from(["openai-provider", "--api-key", "flag-secret"])
            .expect("parse args");
        c.normalize();
        assert_eq!(c.api_key.as_deref(), Some("flag-secret"));
        clear_env();
    }
}
