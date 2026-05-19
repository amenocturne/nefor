//! `nefor.plugins.spawn` — register a plugin to be spawned.
//!
//! The engine knows which plugin binaries to launch from the shared
//! [`PluginRegistry`]. `init.lua` fills that registry via this binding;
//! the actual spawn happens after `init.lua` returns, when the engine
//! enters its run phase (see `crate::ncp::runner`).
//!
//! A spawn entry must declare at least one of:
//! - `command` — array of strings; an OS subprocess to launch and broker.
//! - `cli` — Lua function; reachable as `nefor plugin <name>` (no
//!   subprocess; the function IS the plugin).
//!
//! Both fields together are allowed: a normal subprocess plugin that
//! also exposes a CLI entry point. Neither field is rejected as a
//! pointless entry per D-10.
//!
//! ```lua
//! -- Subprocess plugin (the common case):
//! nefor.plugins.spawn {
//!   name    = "mock-plugin",
//!   command = { "./target/release/mock-plugin", "--script", "scenarios/cc-like.lua" },
//! }
//!
//! -- Virtual plugin — CLI-only, no subprocess:
//! nefor.plugins.spawn {
//!   name = "say",
//!   cli  = function(args) print(args[1] or "(empty)") end,
//! }
//! ```
//!
//! Plugins that need shell features, env variables, or a custom working
//! directory wrap themselves in a user-chosen wrapper script and expose
//! that as their `command`. See `docs/plugin-authoring.md`.
//!
//! ## CLI registry
//!
//! When a spawn includes a `cli` field, the binding stashes the function
//! into a Lua-resident table at `_NEFOR_CLI[name]`. The engine, after
//! booting, looks up the plugin name in this table to dispatch
//! `nefor plugin <name> [args...]`. Storing the function on the Lua side
//! avoids round-tripping through Rust types and keeps registry-key
//! lifetimes managed by the VM.

use mlua::{Lua, Table, Value};
use nefor_protocol::PluginName;

use crate::ncp::{PluginSpec, SharedPluginRegistry};

/// Lua global holding the per-plugin CLI dispatch table. Populated by
/// `nefor.plugins.spawn` and read by the engine when running
/// `nefor plugin <name> ...`. Public so the dispatch entry-point in
/// `main.rs` can locate it without re-stating the literal.
pub const CLI_REGISTRY_GLOBAL: &str = "_NEFOR_CLI";

/// Install `nefor.plugins.spawn` onto `nefor_tbl`.
pub fn install_plugins(
    lua: &Lua,
    nefor_tbl: &Table,
    plugins: SharedPluginRegistry,
) -> mlua::Result<()> {
    let tbl = lua.create_table()?;

    // Pre-create the CLI registry table so the spawn function can write to
    // it directly. Stored as a global rather than under `nefor.plugins.*`
    // because the engine's dispatch path looks it up by a stable name and
    // we don't want a user to accidentally clear it via `nefor.plugins =
    // ...` in init.lua.
    let cli_registry = lua.create_table()?;
    lua.globals().set(CLI_REGISTRY_GLOBAL, cli_registry)?;

    let spawn_fn = lua.create_function(move |lua, opts: Table| {
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
                    // Safety: the `for` loop iterates a fixed 3-element array;
                    // every element is handled by the branches above.
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

        let command: Option<Vec<String>> = match opts.get::<Value>("command")? {
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
                Some(out)
            }
            Value::Nil => None,
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.plugins.spawn: 'command' must be an array of strings (got {})",
                    other.type_name(),
                )));
            }
        };

        let cli_fn: Option<mlua::Function> = match opts.get::<Value>("cli")? {
            Value::Function(f) => Some(f),
            Value::Nil => None,
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.plugins.spawn: 'cli' must be a function (got {})",
                    other.type_name(),
                )));
            }
        };

        let spec = PluginSpec {
            name: name.clone(),
            command,
            has_cli: cli_fn.is_some(),
        };

        // Validate via the registry first — `register` enforces the
        // pointless-entry rule (D-10) so we don't leave a CLI function
        // dangling in the registry when the spec is rejected.
        let mut guard = match plugins.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .register(spec)
            .map_err(|e| mlua::Error::runtime(e.to_string()))?;
        drop(guard);

        // Stash the cli function under `_NEFOR_CLI[name]` so the engine's
        // dispatch path can find it later. Done after registry success
        // so a duplicate-name rejection doesn't leak the function.
        if let Some(f) = cli_fn {
            let registry: Table = lua.globals().get(CLI_REGISTRY_GLOBAL)?;
            registry.set(name.as_str(), f)?;
        }
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
        lua.load(r#"nefor.plugins.spawn { name = "demo", command = { "demo-bin" } }"#)
            .exec()
            .expect("ok");
        let guard = plugins.lock().unwrap();
        let specs = guard.list();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name.as_str(), "demo");
        assert_eq!(
            specs[0].command.as_deref(),
            Some(&["demo-bin".to_string()][..])
        );
        assert!(!specs[0].has_cli);
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
            specs[0].command.as_deref(),
            Some(&["bin".to_string(), "--flag".to_string(), "x".to_string()][..])
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
    fn spawn_rejects_pointless_entry() {
        // Neither command nor cli: pointless, must error with the named
        // RegisterError variant's message.
        let (lua, _) = setup();
        let err = lua
            .load(r#"nefor.plugins.spawn { name = "p" }"#)
            .exec()
            .expect_err("must error");
        let msg = err.to_string();
        assert!(
            msg.contains("neither command nor cli"),
            "expected pointless-entry message, got: {msg}",
        );
    }

    #[test]
    fn spawn_with_cli_only_registers_virtual_plugin() {
        // cli without command: virtual plugin. Registers in the spec list
        // with has_cli=true and command=None, and the function lands in
        // _NEFOR_CLI[name].
        let (lua, plugins) = setup();
        lua.load(
            r#"nefor.plugins.spawn {
                name = "virtual",
                cli = function(args) return args end,
            }"#,
        )
        .exec()
        .expect("ok");
        {
            let guard = plugins.lock().unwrap();
            let specs = guard.list();
            assert_eq!(specs.len(), 1);
            assert_eq!(specs[0].name.as_str(), "virtual");
            assert!(specs[0].command.is_none());
            assert!(specs[0].has_cli);
        }
        // Registry table holds a function under "virtual".
        let f: mlua::Function = lua
            .load(r#"return _NEFOR_CLI["virtual"]"#)
            .eval()
            .expect("registry lookup");
        let _ = f;
    }

    #[test]
    fn spawn_with_command_and_cli_succeeds() {
        let (lua, plugins) = setup();
        lua.load(
            r#"nefor.plugins.spawn {
                name = "both",
                command = { "bin" },
                cli = function() end,
            }"#,
        )
        .exec()
        .expect("ok");
        let guard = plugins.lock().unwrap();
        let specs = guard.list();
        assert_eq!(specs.len(), 1);
        assert!(specs[0].command.is_some());
        assert!(specs[0].has_cli);
    }

    #[test]
    fn spawn_rejects_non_function_cli() {
        let (lua, _) = setup();
        let err = lua
            .load(r#"nefor.plugins.spawn { name = "p", command = { "x" }, cli = 42 }"#)
            .exec()
            .expect_err("non-function cli");
        assert!(err.to_string().contains("'cli' must be a function"));
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
