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
//! The starter assembles explicit per-instance arguments in `config.provider_command`.
//!
//! # Example
//!
//! ```console
//! openai-provider \
//!   --name ollama \
//!   --base-url http://localhost:11434 \
//!   --model phi4-mini:latest \
//!   --retry-policy '{"max_attempts":8,"max_rounds":6,"initial_delay_ms":500,"max_delay_ms":30000,"round_cooldown_ms":60000,"jitter_bps":1000,"response_headers_timeout_ms":120000}'
//! ```

use clap::Parser;

/// Explicit per-instance transport budget, checked once at the CLI boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub(crate) max_attempts: usize,
    pub(crate) max_rounds: usize,
    pub(crate) initial_delay_ms: u64,
    pub(crate) max_delay_ms: u64,
    pub(crate) round_cooldown_ms: u64,
    pub(crate) jitter_bps: u64,
    pub(crate) response_headers_timeout_ms: u64,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRetryPolicy {
    max_attempts: usize,
    max_rounds: usize,
    initial_delay_ms: u64,
    max_delay_ms: u64,
    round_cooldown_ms: u64,
    jitter_bps: u64,
    response_headers_timeout_ms: u64,
}

impl RetryPolicy {
    pub fn from_json(json: &str) -> Result<Self, String> {
        let raw: RawRetryPolicy = serde_json::from_str(json).map_err(|error| error.to_string())?;
        let policy = Self {
            max_attempts: raw.max_attempts,
            max_rounds: raw.max_rounds,
            initial_delay_ms: raw.initial_delay_ms,
            max_delay_ms: raw.max_delay_ms,
            round_cooldown_ms: raw.round_cooldown_ms,
            jitter_bps: raw.jitter_bps,
            response_headers_timeout_ms: raw.response_headers_timeout_ms,
        };
        if policy.max_attempts == 0
            || policy.max_rounds == 0
            || policy.response_headers_timeout_ms == 0
            || policy.initial_delay_ms > policy.max_delay_ms
            || policy.jitter_bps > 10000
        {
            return Err(
                "invalid retry policy counts, delay bounds, jitter or header timeout".into(),
            );
        }
        policy
            .max_attempts
            .checked_mul(policy.max_rounds)
            .ok_or("retry attempt product overflows")?;
        let cooldown = policy
            .round_cooldown_ms
            .checked_add(
                ((u128::from(policy.round_cooldown_ms) * u128::from(policy.jitter_bps)) / 10000)
                    .try_into()
                    .map_err(|_| "retry cooldown jitter overflows")?,
            )
            .ok_or("retry cooldown overflows")?;
        for ms in [
            policy.max_delay_ms,
            cooldown,
            policy.response_headers_timeout_ms,
        ] {
            std::time::Instant::now()
                .checked_add(std::time::Duration::from_millis(ms))
                .ok_or("retry timer is not representable")?;
        }
        Ok(policy)
    }
}

impl std::str::FromStr for RetryPolicy {
    type Err = String;
    fn from_str(json: &str) -> Result<Self, Self::Err> {
        Self::from_json(json)
    }
}

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
    /// Required closed JSON transport retry policy; Lua configuration owns all values.
    #[arg(long)]
    pub retry_policy: RetryPolicy,

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

    /// HTTP header to carry the API key. Default `Authorization` uses
    /// the standard `Authorization: Bearer <key>` form. A custom value
    /// (e.g. `Nestor-Token`) sends the key raw under that header name —
    /// no `Bearer` prefix. Required for OpenAI-compatible backends that
    /// gate on a non-standard header name.
    #[arg(long = "auth-header", default_value = "Authorization")]
    pub auth_header: String,
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

    const POLICY: &str = r#"{"max_attempts":8,"max_rounds":1,"initial_delay_ms":500,"max_delay_ms":30000,"round_cooldown_ms":0,"jitter_bps":0,"response_headers_timeout_ms":120000}"#;

    fn cfg(name: &str) -> Config {
        Config {
            retry_policy: RetryPolicy::from_json(POLICY).unwrap(),
            provider_name: name.into(),
            base_url: "http://localhost:11434".into(),
            model: Some("qwen2.5-coder:7b".into()),
            api_key: None,
            auth_header: "Authorization".into(),
        }
    }

    #[test]
    fn starter_lua_argv_crosses_the_native_checked_parser() {
        use mlua::LuaSerdeExt;
        let lua = mlua::Lua::new();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let package: mlua::Table = lua.globals().get("package").unwrap();
        package.set("path", format!("{0}/examples/nefor-agent/?.lua;{0}/examples/nefor-agent/?/init.lua;{0}/lua/?.lua;{0}/lua/?/init.lua;{0}/plugins/openai-provider/lua/?/init.lua", root.display())).unwrap();
        let json = lua.create_table().unwrap();
        json.set(
            "encode",
            lua.create_function(|lua, value: mlua::Value| {
                let value: serde_json::Value = lua.from_value(value)?;
                serde_json::to_string(&value).map_err(mlua::Error::external)
            })
            .unwrap(),
        )
        .unwrap();
        let nefor = lua.create_table().unwrap();
        nefor.set("json", json).unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        let commands: Vec<Vec<String>> = lua.load(r#"
            package.loaded["config.distribution"] = {binary=function(name) return name end}
            local getenv = os.getenv
            os.getenv = function(key)
                if key == "NEFOR_ENABLE_OPENROUTER" or key == "NEFOR_ENABLE_OLLAMA" then return "1" end
                return getenv(key)
            end
            local commands = {}
            local command = require("config.provider_command")
            for _, p in ipairs(require("config").active.providers) do
                if p.kind == "openai" then commands[#commands+1] = command.openai("openai-provider", p) end
            end
            commands[#commands+1] = command.openai("openai-provider", {
                name="custom", base_url="http://local", transport_retry={max_attempts=2,max_rounds=3,
                    initial_delay_ms=0,max_delay_ms=0,round_cooldown_ms=9,jitter_bps=0,response_headers_timeout_ms=30}})
            return commands
        "#).eval().and_then(|value| lua.from_value(value)).unwrap();
        assert_eq!(commands.len(), 3);
        for (index, command) in commands.iter().enumerate() {
            let parsed = Config::try_parse_from(command).unwrap();
            assert_eq!(
                parsed.retry_policy.max_attempts,
                if index == 2 { 2 } else { 8 }
            );
            assert_eq!(
                parsed.retry_policy.max_rounds,
                if index == 2 { 3 } else { 6 }
            );
            assert_eq!(
                command
                    .iter()
                    .filter(|value| *value == "--retry-policy")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn retry_policy_is_required_and_closed() {
        assert!(Config::try_parse_from(["openai-provider"]).is_err());
        let valid: serde_json::Value = serde_json::from_str(POLICY).unwrap();
        for field in valid.as_object().unwrap().keys() {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                RetryPolicy::from_json(&missing.to_string()).is_err(),
                "missing {field}"
            );
            for invalid in [
                serde_json::Value::Null,
                serde_json::json!(true),
                serde_json::json!("1"),
                serde_json::json!(-1),
                serde_json::json!(1.5),
                serde_json::from_str("18446744073709551616").unwrap(),
            ] {
                let mut malformed = valid.clone();
                malformed[field] = invalid;
                assert!(
                    RetryPolicy::from_json(&malformed.to_string()).is_err(),
                    "field {field}: {malformed}"
                );
            }
        }
        let mut extra = valid.clone();
        extra["unknown"] = serde_json::json!(1);
        assert!(RetryPolicy::from_json(&extra.to_string()).is_err());
        for (field, invalid) in [
            ("max_attempts", 0),
            ("max_rounds", 0),
            ("response_headers_timeout_ms", 0),
            ("jitter_bps", 10001),
            ("initial_delay_ms", 30001),
        ] {
            let mut malformed = valid.clone();
            malformed[field] = serde_json::json!(invalid);
            assert!(RetryPolicy::from_json(&malformed.to_string()).is_err());
        }
        let mut overflow = valid.clone();
        overflow["max_attempts"] = serde_json::json!(usize::MAX);
        overflow["max_rounds"] = serde_json::json!(2);
        assert!(RetryPolicy::from_json(&overflow.to_string()).is_err());
        overflow = valid.clone();
        overflow["round_cooldown_ms"] = serde_json::json!(u64::MAX);
        overflow["jitter_bps"] = serde_json::json!(10000);
        assert!(RetryPolicy::from_json(&overflow.to_string()).is_err());
        assert_eq!(RetryPolicy::from_json(POLICY).unwrap().max_attempts, 8);
    }

    #[test]
    fn retry_policy_admits_explicit_zero_delay_and_custom_budgets() {
        let policy = RetryPolicy::from_json(r#"{"max_attempts":2,"max_rounds":3,"initial_delay_ms":0,"max_delay_ms":0,"round_cooldown_ms":0,"jitter_bps":10000,"response_headers_timeout_ms":1}"#).unwrap();
        assert_eq!(
            (
                policy.max_attempts,
                policy.max_rounds,
                policy.initial_delay_ms
            ),
            (2, 3, 0)
        );
        let args = ["openai-provider", "--retry-policy", POLICY];
        assert_eq!(
            Config::try_parse_from(args).unwrap().retry_policy,
            RetryPolicy::from_json(POLICY).unwrap()
        );
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

    struct ScopedEnvVar {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl ScopedEnvVar {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let lock = env_lock();
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self {
                key,
                previous,
                _lock: lock,
            }
        }

        fn remove(key: &'static str) -> Self {
            let lock = env_lock();
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self {
                key,
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for ScopedEnvVar {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// Parse with the env lock held + the OPENAI_PROVIDER_API_KEY env var
    /// cleared, so default-case assertions about `api_key` aren't racing
    /// the env-fallback tests.
    fn parse_clean(args: &[&str]) -> (ScopedEnvVar, Config) {
        let env = ScopedEnvVar::remove("OPENAI_PROVIDER_API_KEY");
        let mut c = Config::try_parse_from(args.iter().copied().chain(["--retry-policy", POLICY]))
            .expect("parse args");
        c.normalize();
        (env, c)
    }

    #[test]
    fn defaults_have_no_model() {
        let (_g, c) = parse_clean(&["openai-provider"]);
        assert_eq!(c.provider_name, "openai");
        assert_eq!(c.base_url, "http://localhost:11434");
        assert!(
            c.model.is_none(),
            "model is opt-in; no compiled-in fallback"
        );
        assert!(c.api_key.is_none());
        assert_eq!(c.auth_header, "Authorization");
    }

    #[test]
    fn auth_header_flag_overrides_default() {
        let (_g, c) = parse_clean(&["openai-provider", "--auth-header", "Nestor-Token"]);
        assert_eq!(c.auth_header, "Nestor-Token");
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
        let _env = ScopedEnvVar::set("OPENAI_PROVIDER_API_KEY", "env-secret");
        let mut c = Config::try_parse_from(["openai-provider", "--retry-policy", POLICY])
            .expect("parse args");
        c.normalize();
        assert_eq!(c.api_key.as_deref(), Some("env-secret"));
    }

    #[test]
    fn explicit_flag_beats_env_var() {
        let _env = ScopedEnvVar::set("OPENAI_PROVIDER_API_KEY", "env-secret");
        let mut c = Config::try_parse_from([
            "openai-provider",
            "--api-key",
            "flag-secret",
            "--retry-policy",
            POLICY,
        ])
        .expect("parse args");
        c.normalize();
        assert_eq!(c.api_key.as_deref(), Some("flag-secret"));
    }

    #[test]
    fn scoped_env_restores_during_unwind() {
        const KEY: &str = "NEFOR_TEST_OPENAI_SCOPED_ENV_UNWIND";
        std::env::set_var(KEY, "original");
        let result = std::panic::catch_unwind(|| {
            let _env = ScopedEnvVar::set(KEY, "temporary");
            panic!("deliberate unwind");
        });
        assert!(result.is_err());
        assert_eq!(
            std::env::var_os(KEY).as_deref(),
            Some(std::ffi::OsStr::new("original"))
        );
        std::env::remove_var(KEY);
    }
}
