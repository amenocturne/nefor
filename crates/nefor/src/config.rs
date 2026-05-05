//! Config and data directory resolution.
//!
//! Precedence (highest → lowest):
//! - `--config <DIR>` / `--data-dir <DIR>` CLI flags
//! - `NEFOR_CONFIG_DIR` / `NEFOR_DATA_DIR` environment variables
//! - XDG defaults (`~/.config/nefor/` and `~/.local/share/nefor/`)
//!
//! Resolution is pure: [`resolve_config_from`] / [`resolve_data_from`] take
//! the parsed CLI and an [`EnvReader`]. I/O (reading `init.lua`, creating
//! directories) happens later.

use std::path::PathBuf;

use crate::cli::Cli;
use crate::paths::{ConfigDir, DataDir};

/// Typed errors produced during directory resolution.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Neither a CLI flag, env var, nor a usable XDG root was found.
    #[error("could not determine XDG config directory (is $HOME set?)")]
    NoXdgConfigDir,

    #[error("could not determine XDG data directory (is $HOME set?)")]
    NoXdgDataDir,
}

/// Read-only view of the process environment used by the resolve functions. A
/// trait so unit tests can inject a deterministic env without mutating the
/// real one (which would require `serial_test` for race-free concurrent tests).
pub trait EnvReader {
    fn get(&self, key: &str) -> Option<String>;
    /// Return the XDG config-home root (`$XDG_CONFIG_HOME` or `~/.config`).
    fn xdg_config_home(&self) -> Option<PathBuf>;
    /// Return the XDG data-home root (`$XDG_DATA_HOME` or `~/.local/share`).
    fn xdg_data_home(&self) -> Option<PathBuf>;
}

/// [`EnvReader`] backed by the real process environment.
pub struct SystemEnv;

impl EnvReader for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
    // Literal XDG on every platform — spec pins `~/.config/nefor/init.lua` as
    // the default. `dirs::config_dir()` would give `~/Library/Application Support`
    // on macOS, which diverges from the Neovim convention.
    fn xdg_config_home(&self) -> Option<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }
    fn xdg_data_home(&self) -> Option<PathBuf> {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"))
            })
    }
}

/// Resolve the config directory from CLI args + environment. Pure.
pub fn resolve_config_from(cli: &Cli, env: &impl EnvReader) -> Result<ConfigDir, ConfigError> {
    if let Some(dir) = &cli.config {
        return Ok(ConfigDir(dir.clone()));
    }
    if let Some(raw) = env.get("NEFOR_CONFIG_DIR").filter(|s| !s.is_empty()) {
        return Ok(ConfigDir(PathBuf::from(raw)));
    }
    let xdg = env.xdg_config_home().ok_or(ConfigError::NoXdgConfigDir)?;
    Ok(ConfigDir(xdg.join("nefor")))
}

/// Resolve the data directory from CLI args + environment. Pure.
pub fn resolve_data_from(cli: &Cli, env: &impl EnvReader) -> Result<DataDir, ConfigError> {
    if let Some(dir) = &cli.data_dir {
        return Ok(DataDir(dir.clone()));
    }
    if let Some(raw) = env.get("NEFOR_DATA_DIR").filter(|s| !s.is_empty()) {
        return Ok(DataDir(PathBuf::from(raw)));
    }
    let xdg = env.xdg_data_home().ok_or(ConfigError::NoXdgDataDir)?;
    Ok(DataDir(xdg.join("nefor")))
}

/// Resolve the config directory using the real process environment.
pub fn resolve_config(cli: &Cli) -> Result<ConfigDir, ConfigError> {
    resolve_config_from(cli, &SystemEnv)
}

/// Resolve the data directory using the real process environment.
pub fn resolve_data(cli: &Cli) -> Result<DataDir, ConfigError> {
    resolve_data_from(cli, &SystemEnv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeEnv {
        vars: HashMap<String, String>,
        xdg_config: Option<PathBuf>,
        xdg_data: Option<PathBuf>,
    }

    impl FakeEnv {
        fn new(xdg_config: Option<&str>, xdg_data: Option<&str>) -> Self {
            Self {
                vars: HashMap::new(),
                xdg_config: xdg_config.map(PathBuf::from),
                xdg_data: xdg_data.map(PathBuf::from),
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
            self.xdg_config.clone()
        }
        fn xdg_data_home(&self) -> Option<PathBuf> {
            self.xdg_data.clone()
        }
    }

    fn cli_bare() -> Cli {
        Cli {
            config: None,
            data_dir: None,
            plugin_dir: None,
            command: None,
        }
    }
    fn cli_with_config(dir: &str) -> Cli {
        Cli {
            config: Some(PathBuf::from(dir)),
            data_dir: None,
            plugin_dir: None,
            command: None,
        }
    }
    fn cli_with_data(dir: &str) -> Cli {
        Cli {
            config: None,
            data_dir: Some(PathBuf::from(dir)),
            plugin_dir: None,
            command: None,
        }
    }

    // --- config dir ---

    #[test]
    fn config_cli_flag_beats_env_and_default() {
        let cli = cli_with_config("/tmp/my-config");
        let env = FakeEnv::new(Some("/home/u/.config"), None)
            .with("NEFOR_CONFIG_DIR", "/env/config");
        let got = resolve_config_from(&cli, &env).expect("resolve ok");
        assert_eq!(got, ConfigDir(PathBuf::from("/tmp/my-config")));
    }

    #[test]
    fn config_env_var_beats_xdg_default() {
        let cli = cli_bare();
        let env = FakeEnv::new(Some("/home/u/.config"), None)
            .with("NEFOR_CONFIG_DIR", "/env/config");
        let got = resolve_config_from(&cli, &env).expect("resolve ok");
        assert_eq!(got, ConfigDir(PathBuf::from("/env/config")));
    }

    #[test]
    fn config_xdg_default_when_no_flag_or_env() {
        let cli = cli_bare();
        let env = FakeEnv::new(Some("/home/u/.config"), None);
        let got = resolve_config_from(&cli, &env).expect("resolve ok");
        assert_eq!(got, ConfigDir(PathBuf::from("/home/u/.config/nefor")));
    }

    #[test]
    fn config_missing_xdg_errors_without_flag_or_env() {
        let cli = cli_bare();
        let env = FakeEnv::new(None, None);
        let err = resolve_config_from(&cli, &env).expect_err("must error");
        assert!(matches!(err, ConfigError::NoXdgConfigDir));
    }

    #[test]
    fn config_cli_flag_ok_without_xdg() {
        let cli = cli_with_config("/tmp/x");
        let env = FakeEnv::new(None, None);
        let got = resolve_config_from(&cli, &env).expect("flag bypasses xdg");
        assert_eq!(got, ConfigDir(PathBuf::from("/tmp/x")));
    }

    // --- data dir ---

    #[test]
    fn data_cli_flag_beats_env_and_default() {
        let cli = cli_with_data("/tmp/my-data");
        let env = FakeEnv::new(None, Some("/home/u/.local/share"))
            .with("NEFOR_DATA_DIR", "/env/data");
        let got = resolve_data_from(&cli, &env).expect("resolve ok");
        assert_eq!(got, DataDir(PathBuf::from("/tmp/my-data")));
    }

    #[test]
    fn data_env_var_beats_xdg_default() {
        let cli = cli_bare();
        let env = FakeEnv::new(None, Some("/home/u/.local/share"))
            .with("NEFOR_DATA_DIR", "/env/data");
        let got = resolve_data_from(&cli, &env).expect("resolve ok");
        assert_eq!(got, DataDir(PathBuf::from("/env/data")));
    }

    #[test]
    fn data_xdg_default_when_no_flag_or_env() {
        let cli = cli_bare();
        let env = FakeEnv::new(None, Some("/home/u/.local/share"));
        let got = resolve_data_from(&cli, &env).expect("resolve ok");
        assert_eq!(got, DataDir(PathBuf::from("/home/u/.local/share/nefor")));
    }

    #[test]
    fn data_missing_xdg_errors_without_flag_or_env() {
        let cli = cli_bare();
        let env = FakeEnv::new(None, None);
        let err = resolve_data_from(&cli, &env).expect_err("must error");
        assert!(matches!(err, ConfigError::NoXdgDataDir));
    }
}
