//! `nefor.plugins.spawn` — register a plugin to be spawned.
//!
//! The engine knows which plugin binaries to launch from the shared
//! [`PluginRegistry`]. `init.lua` fills that registry via this binding;
//! the actual spawn happens after `init.lua` returns, when the engine
//! enters its run phase (see `crate::ncp::runner`).
//!
//! Minimum surface: a plugin name and a command array.
//!
//! ```lua
//! nefor.plugins.spawn {
//!   name    = "mock-plugin",
//!   command = { "./target/release/mock-plugin", "--script", "scenarios/cc-like.lua" },
//! }
//! ```
//!
//! Plugins that need shell features, env variables, or a custom working
//! directory wrap themselves in a user-chosen wrapper script and expose
//! that as their `command`. See `docs/plugin-authoring.md`.

use mlua::{Lua, Table, Value};
use nefor_protocol::PluginName;

use crate::ncp::{PluginSpec, SharedPluginRegistry};

/// Install `nefor.plugins.spawn` onto `nefor_tbl`.
pub fn install_plugins(
    lua: &Lua,
    nefor_tbl: &Table,
    plugins: SharedPluginRegistry,
) -> mlua::Result<()> {
    let tbl = lua.create_table()?;

    let spawn_fn = lua.create_function(move |_, opts: Table| {
        // Reject the fields that were removed in the runner-broker split
        // with a specific, actionable error message. Better than a
        // generic "unknown field" — we can tell users exactly what to do
        // instead.
        for removed in ["args", "env", "cwd"] {
            let v: Value = opts.get(removed)?;
            if !matches!(v, Value::Nil) {
                let hint = match removed {
                    "args" => "put args inside the command array, e.g. command = { binary, \"--flag\", \"value\" }",
                    "env" => "set env vars in a wrapper script and invoke that script as the command",
                    "cwd" => "the engine always uses <plugin-dir>/<name>/ as cwd; use a wrapper script if you need a different one",
                    _ => unreachable!(),
                };
                return Err(mlua::Error::runtime(format!(
                    "nefor.plugins.spawn: unknown field '{removed}'; {hint}"
                )));
            }
        }

        let name_raw: String = match opts.get::<Value>("name")? {
            Value::String(s) => s.to_str()?.to_owned(),
            Value::Nil => {
                return Err(mlua::Error::runtime(
                    "nefor.plugins.spawn: 'name' is required",
                ));
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.plugins.spawn: 'name' must be a string (got {})",
                    other.type_name(),
                )));
            }
        };
        let name = PluginName::new(name_raw).map_err(|e| {
            mlua::Error::runtime(format!("nefor.plugins.spawn: {e}"))
        })?;

        let command: Vec<String> = match opts.get::<Value>("command")? {
            Value::Table(t) => {
                let mut out = Vec::new();
                for pair in t.pairs::<i64, Value>() {
                    let (_idx, v) = pair?;
                    match v {
                        Value::String(s) => out.push(s.to_str()?.to_owned()),
                        other => {
                            return Err(mlua::Error::runtime(format!(
                                "nefor.plugins.spawn: 'command' entries must be strings (got {})",
                                other.type_name(),
                            )));
                        }
                    }
                }
                if out.is_empty() {
                    return Err(mlua::Error::runtime(
                        "nefor.plugins.spawn: 'command' must be a non-empty array of strings",
                    ));
                }
                if out.iter().any(String::is_empty) {
                    return Err(mlua::Error::runtime(
                        "nefor.plugins.spawn: 'command' entries must be non-empty strings",
                    ));
                }
                out
            }
            Value::Nil => {
                return Err(mlua::Error::runtime(
                    "nefor.plugins.spawn: 'command' is required (an array of strings, first is the binary)",
                ));
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.plugins.spawn: 'command' must be an array of strings (got {})",
                    other.type_name(),
                )));
            }
        };

        let spec = PluginSpec { name, command };

        let mut guard = match plugins.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .register(spec)
            .map_err(|e| mlua::Error::runtime(e.to_string()))?;
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
        lua.load(r#"nefor.plugins.spawn { name = "tui", command = { "nefor-tui" } }"#)
            .exec()
            .expect("ok");
        let guard = plugins.lock().unwrap();
        let specs = guard.list();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name.as_str(), "tui");
        assert_eq!(specs[0].command, vec!["nefor-tui".to_string()]);
    }

    #[test]
    fn spawn_registers_with_multi_element_command() {
        let (lua, plugins) = setup();
        lua.load(
            r#"nefor.plugins.spawn {
                name = "p",
                command = { "bin", "--flag", "x" },
            }"#,
        )
        .exec()
        .expect("ok");
        let guard = plugins.lock().unwrap();
        let specs = guard.list();
        assert_eq!(specs.len(), 1);
        assert_eq!(
            specs[0].command,
            vec!["bin".to_string(), "--flag".to_string(), "x".to_string()]
        );
    }

    #[test]
    fn spawn_rejects_missing_name() {
        let (lua, _) = setup();
        let err = lua
            .load(r#"nefor.plugins.spawn { command = { "x" } }"#)
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
    fn spawn_rejects_empty_command_array() {
        let (lua, _) = setup();
        let err = lua
            .load(r#"nefor.plugins.spawn { name = "p", command = {} }"#)
            .exec()
            .expect_err("empty command");
        assert!(err.to_string().contains("command"));
    }

    #[test]
    fn spawn_rejects_reserved_name() {
        let (lua, _) = setup();
        let err = lua
            .load(r#"nefor.plugins.spawn { name = "engine", command = { "x" } }"#)
            .exec()
            .expect_err("reserved");
        assert!(err.to_string().contains("engine"));
    }

    #[test]
    fn spawn_rejects_duplicate_name() {
        let (lua, _) = setup();
        lua.load(r#"nefor.plugins.spawn { name = "p", command = { "a" } }"#)
            .exec()
            .expect("first ok");
        let err = lua
            .load(r#"nefor.plugins.spawn { name = "p", command = { "b" } }"#)
            .exec()
            .expect_err("dup");
        assert!(err.to_string().contains("already registered"));
    }

    #[test]
    fn spawn_rejects_removed_args_field() {
        let (lua, _) = setup();
        let err = lua
            .load(
                r#"nefor.plugins.spawn {
                    name = "p",
                    command = { "bin" },
                    args = { "--flag" },
                }"#,
            )
            .exec()
            .expect_err("args field removed");
        let msg = err.to_string();
        assert!(msg.contains("args"), "message should name the field: {msg}");
    }

    #[test]
    fn spawn_rejects_removed_env_field() {
        let (lua, _) = setup();
        let err = lua
            .load(
                r#"nefor.plugins.spawn {
                    name = "p",
                    command = { "bin" },
                    env = { K = "V" },
                }"#,
            )
            .exec()
            .expect_err("env field removed");
        assert!(err.to_string().contains("env"));
    }

    #[test]
    fn spawn_rejects_removed_cwd_field() {
        let (lua, _) = setup();
        let err = lua
            .load(
                r#"nefor.plugins.spawn {
                    name = "p",
                    command = { "bin" },
                    cwd = "/tmp",
                }"#,
            )
            .exec()
            .expect_err("cwd field removed");
        assert!(err.to_string().contains("cwd"));
    }
}
