//! Config directory resolution.
//!
//! Per spec §Configuration Model:
//! - `--config <DIR>` wins over everything.
//! - Else if `NEFOR_APPNAME=<name>` is set, use `$XDG_CONFIG_HOME/nefor-<name>/`.
//! - Else use `$XDG_CONFIG_HOME/nefor/`.
//!
//! Resolution is pure: [`resolve_from`] takes the parsed CLI and an
//! [`EnvReader`] and returns a [`ConfigDir`]. I/O (reading `init.lua`, creating
//! the directory) is a later task.

use std::path::PathBuf;

use crate::cli::Cli;
use crate::paths::ConfigDir;

/// Typed errors produced during config-dir resolution.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `dirs::config_dir()` returned `None` — the platform couldn't locate a
    /// home / XDG config root. Rare in practice (missing `$HOME` on Unix).
    #[error("could not determine XDG config directory (is $HOME set?)")]
    NoXdgConfigDir,

    /// `NEFOR_APPNAME` was set but empty — almost certainly a user mistake
    /// (a shell expansion of an unset variable); fail loud rather than
    /// silently falling back to `$XDG_CONFIG_HOME/nefor-/`.
    #[error("NEFOR_APPNAME is set but empty")]
    EmptyAppName,
}

/// Read-only view of the process environment used by [`resolve_from`]. A
/// trait so unit tests can inject a deterministic env without mutating the
/// real one (which would require `serial_test` for race-free concurrent tests).
pub trait EnvReader {
    /// Return the value of `key` if set, else `None`.
    fn get(&self, key: &str) -> Option<String>;
    /// Return the XDG config-home root (`$XDG_CONFIG_HOME` or `~/.config`).
    fn xdg_config_home(&self) -> Option<PathBuf>;
}

/// [`EnvReader`] backed by the real process environment + [`dirs`].
pub struct SystemEnv;

impl EnvReader for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
    // Literal XDG on every platform — spec pins `~/.config/nefor/init.lua` as
    // the default. `dirs::config_dir()` would give `~/Library/Application Support`
    // on macOS, which diverges from the Neovim convention the spec references.
    fn xdg_config_home(&self) -> Option<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }
}

/// Resolve the config directory from CLI args + environment. Pure.
pub fn resolve_from(cli: &Cli, env: &impl EnvReader) -> Result<ConfigDir, ConfigError> {
    if let Some(dir) = &cli.config {
        return Ok(ConfigDir(dir.clone()));
    }

    let xdg = env.xdg_config_home().ok_or(ConfigError::NoXdgConfigDir)?;

    let leaf = match env.get("NEFOR_APPNAME") {
        Some(name) if name.is_empty() => return Err(ConfigError::EmptyAppName),
        Some(name) => format!("nefor-{name}"),
        None => "nefor".to_string(),
    };

    Ok(ConfigDir(xdg.join(leaf)))
}

/// Resolve the config directory using the real process environment.
pub fn resolve(cli: &Cli) -> Result<ConfigDir, ConfigError> {
    resolve_from(cli, &SystemEnv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeEnv {
        vars: HashMap<String, String>,
        xdg: Option<PathBuf>,
    }

    impl FakeEnv {
        fn new(xdg: Option<&str>) -> Self {
            Self {
                vars: HashMap::new(),
                xdg: xdg.map(PathBuf::from),
            }
        }
        fn with(mut self, key: &str, val: &str) -> Self {
            self.vars.insert(key.to_string(), val.to_string());
            self
        }
    }

    impl EnvReader for FakeEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
        fn xdg_config_home(&self) -> Option<PathBuf> {
            self.xdg.clone()
        }
    }

    fn cli_with_config(dir: Option<&str>) -> Cli {
        Cli {
            config: dir.map(PathBuf::from),
            plugin_dir: None,
            command: None,
        }
    }

    #[test]
    fn cli_flag_beats_env_and_default() {
        let cli = cli_with_config(Some("/tmp/my-config"));
        let env = FakeEnv::new(Some("/home/u/.config")).with("NEFOR_APPNAME", "analyst");
        let got = resolve_from(&cli, &env).expect("resolve ok");
        assert_eq!(got, ConfigDir(PathBuf::from("/tmp/my-config")));
    }

    #[test]
    fn env_appname_beats_default() {
        let cli = cli_with_config(None);
        let env = FakeEnv::new(Some("/home/u/.config")).with("NEFOR_APPNAME", "analyst");
        let got = resolve_from(&cli, &env).expect("resolve ok");
        assert_eq!(
            got,
            ConfigDir(PathBuf::from("/home/u/.config/nefor-analyst"))
        );
    }

    #[test]
    fn default_when_neither_set() {
        let cli = cli_with_config(None);
        let env = FakeEnv::new(Some("/home/u/.config"));
        let got = resolve_from(&cli, &env).expect("resolve ok");
        assert_eq!(got, ConfigDir(PathBuf::from("/home/u/.config/nefor")));
    }

    #[test]
    fn empty_appname_is_an_error() {
        let cli = cli_with_config(None);
        let env = FakeEnv::new(Some("/home/u/.config")).with("NEFOR_APPNAME", "");
        let err = resolve_from(&cli, &env).expect_err("must error");
        assert!(matches!(err, ConfigError::EmptyAppName));
    }

    #[test]
    fn missing_xdg_is_an_error_when_no_flag() {
        let cli = cli_with_config(None);
        let env = FakeEnv::new(None);
        let err = resolve_from(&cli, &env).expect_err("must error");
        assert!(matches!(err, ConfigError::NoXdgConfigDir));
    }

    #[test]
    fn missing_xdg_is_fine_when_flag_is_given() {
        let cli = cli_with_config(Some("/tmp/x"));
        let env = FakeEnv::new(None);
        let got = resolve_from(&cli, &env).expect("flag bypasses xdg lookup");
        assert_eq!(got, ConfigDir(PathBuf::from("/tmp/x")));
    }
}
