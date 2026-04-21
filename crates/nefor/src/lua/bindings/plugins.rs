//! `nefor.plugins.spawn` — register a plugin to be spawned.
//!
//! The engine knows which plugin binaries to launch from the shared
//! [`PluginRegistry`]. `init.lua` fills that registry via this binding; the
//! actual spawn happens after `init.lua` returns, when the engine enters its
//! run phase (see [`crate::ncp::Broker::spawn`]).
//!
//! Only the minimum surface for wiring a plugin name to an OS command:
//!
//! ```lua
//! nefor.plugins.spawn {
//!   name = "nefor-tui",
//!   command = "nefor-tui",      -- required; from PATH or absolute
//!   args = { "--foo", "bar" },  -- optional
//!   env  = { KEY = "VAL" },     -- optional
//!   cwd  = "/some/path",        -- optional
//! }
//! ```

use std::collections::HashMap;

use mlua::{Lua, Table, Value};

use crate::ncp::{PluginSpec, SharedPluginRegistry};

/// Install `nefor.plugins.spawn` onto `nefor_tbl`.
pub fn install_plugins(
    lua: &Lua,
    nefor_tbl: &Table,
    plugins: SharedPluginRegistry,
) -> mlua::Result<()> {
    let tbl = lua.create_table()?;

    let spawn_fn = lua.create_function(move |_, opts: Table| {
        let name: String = match opts.get::<Value>("name")? {
            Value::String(s) => {
                let s = s.to_str()?.to_owned();
                if s.is_empty() {
                    return Err(mlua::Error::runtime(
                        "nefor.plugins.spawn: 'name' must be a non-empty string",
                    ));
                }
                if s == "engine" {
                    return Err(mlua::Error::runtime(
                        "nefor.plugins.spawn: 'name' \"engine\" is reserved",
                    ));
                }
                s
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.plugins.spawn: 'name' must be a string (got {})",
                    other.type_name(),
                )));
            }
        };

        let command: String = match opts.get::<Value>("command")? {
            Value::String(s) => {
                let s = s.to_str()?.to_owned();
                if s.is_empty() {
                    return Err(mlua::Error::runtime(
                        "nefor.plugins.spawn: 'command' must be a non-empty string",
                    ));
                }
                s
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.plugins.spawn: 'command' must be a string (got {})",
                    other.type_name(),
                )));
            }
        };

        let args: Vec<String> = match opts.get::<Value>("args")? {
            Value::Nil => Vec::new(),
            Value::Table(t) => {
                let mut out = Vec::new();
                for pair in t.pairs::<i64, Value>() {
                    let (_idx, v) = pair?;
                    match v {
                        Value::String(s) => out.push(s.to_str()?.to_owned()),
                        other => {
                            return Err(mlua::Error::runtime(format!(
                                "nefor.plugins.spawn: 'args' entries must be strings (got {})",
                                other.type_name(),
                            )));
                        }
                    }
                }
                out
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.plugins.spawn: 'args' must be a table of strings or nil (got {})",
                    other.type_name(),
                )));
            }
        };

        let env: HashMap<String, String> = match opts.get::<Value>("env")? {
            Value::Nil => HashMap::new(),
            Value::Table(t) => {
                let mut out = HashMap::new();
                for pair in t.pairs::<String, String>() {
                    let (k, v) = pair?;
                    out.insert(k, v);
                }
                out
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.plugins.spawn: 'env' must be a table of string→string or nil (got {})",
                    other.type_name(),
                )));
            }
        };

        let cwd: Option<String> = match opts.get::<Value>("cwd")? {
            Value::Nil => None,
            Value::String(s) => Some(s.to_str()?.to_owned()),
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.plugins.spawn: 'cwd' must be a string or nil (got {})",
                    other.type_name(),
                )));
            }
        };

        let spec = PluginSpec {
            name,
            command,
            args,
            env,
            cwd,
        };

        let mut guard = match plugins.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.register(spec).map_err(mlua::Error::runtime)?;
        Ok(())
    })?;
    tbl.set("spawn", spawn_fn)?;

    nefor_tbl.set("plugins", tbl)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ncp::PluginRegistry;
    use std::sync::{Arc, Mutex};

    fn setup() -> (Lua, SharedPluginRegistry) {
        let lua = Lua::new();
        let plugins: SharedPluginRegistry = Arc::new(Mutex::new(PluginRegistry::new()));
        let nefor = lua.create_table().unwrap();
        install_plugins(&lua, &nefor, Arc::clone(&plugins)).unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        (lua, plugins)
    }

    #[test]
    fn spawn_registers_minimal_plugin() {
        let (lua, plugins) = setup();
        lua.load(r#"nefor.plugins.spawn { name = "tui", command = "nefor-tui" }"#)
            .exec()
            .expect("ok");
        let guard = plugins.lock().unwrap();
        let specs = guard.list();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "tui");
        assert_eq!(specs[0].command, "nefor-tui");
        assert!(specs[0].args.is_empty());
    }

    #[test]
    fn spawn_registers_with_args_env_cwd() {
        let (lua, plugins) = setup();
        lua.load(
            r#"nefor.plugins.spawn {
                name = "p",
                command = "bin",
                args = { "--flag", "x" },
                env = { KEY = "VAL" },
                cwd = "/tmp",
            }"#,
        )
        .exec()
        .expect("ok");
        let guard = plugins.lock().unwrap();
        let specs = guard.list();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].args, vec!["--flag".to_string(), "x".to_string()]);
        assert_eq!(specs[0].env.get("KEY").map(String::as_str), Some("VAL"));
        assert_eq!(specs[0].cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn spawn_rejects_missing_name() {
        let (lua, _) = setup();
        let err = lua
            .load(r#"nefor.plugins.spawn { command = "x" }"#)
            .exec()
            .expect_err("missing name");
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn spawn_rejects_missing_command() {
        let (lua, _) = setup();
        let err = lua
            .load(r#"nefor.plugins.spawn { name = "p" }"#)
            .exec()
            .expect_err("missing command");
        assert!(err.to_string().contains("command"));
    }

    #[test]
    fn spawn_rejects_reserved_name() {
        let (lua, _) = setup();
        let err = lua
            .load(r#"nefor.plugins.spawn { name = "engine", command = "x" }"#)
            .exec()
            .expect_err("reserved");
        assert!(err.to_string().contains("reserved"));
    }

    #[test]
    fn spawn_rejects_duplicate_name() {
        let (lua, _) = setup();
        lua.load(r#"nefor.plugins.spawn { name = "p", command = "a" }"#)
            .exec()
            .expect("first ok");
        let err = lua
            .load(r#"nefor.plugins.spawn { name = "p", command = "b" }"#)
            .exec()
            .expect_err("dup");
        assert!(err.to_string().contains("already registered"));
    }
}
