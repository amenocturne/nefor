use std::path::Path;

use mlua::{Function, Lua, LuaSerdeExt, Table, Value};
use serde_json::Value as JsonValue;

use crate::error::MagError;

/// Immutable factory/type contracts exported by the same Lua registry file the
/// runtime kernel uses. Loading this surface constructs no run context and
/// exposes no execution operation.
pub fn load_registry_contracts(path: &Path) -> Result<JsonValue, MagError> {
    let source = std::fs::read_to_string(path).map_err(|error| {
        MagError::Eval(format!("cannot read registry {}: {error}", path.display()))
    })?;
    let lua = Lua::new();
    install_registry_host(&lua)?;
    set_registry_path(&lua, path)?;
    let value: Value = lua
        .load(&source)
        .set_name(path.display().to_string())
        .eval()
        .map_err(|error| MagError::Eval(format!("load registry {}: {error}", path.display())))?;
    let table = match value {
        Value::Table(table) => table,
        other => {
            return Err(MagError::Eval(format!(
                "registry {} must return a table, got {}",
                path.display(),
                other.type_name()
            )))
        }
    };
    let contracts: Function = table.get("registry_contracts").map_err(|error| {
        MagError::Eval(format!(
            "registry {} does not expose registry_contracts: {error}",
            path.display()
        ))
    })?;
    let value: Value = contracts
        .call(lua.array_metatable())
        .map_err(|error| MagError::Eval(format!("read registry contracts: {error}")))?;
    lua.from_value(value)
        .map_err(|error| MagError::Eval(format!("serialize registry contracts: {error}")))
}

fn set_registry_path(lua: &Lua, path: &Path) -> Result<(), MagError> {
    let Some(directory) = path.parent() else {
        return Ok(());
    };
    let patterns = [
        directory.join("?.lua"),
        directory.join("?/init.lua"),
        directory.join("../../../../lua/?.lua"),
        directory.join("../../../../lua/?/init.lua"),
    ];
    let package: Table = lua
        .globals()
        .get("package")
        .map_err(|error| MagError::Eval(format!("load Lua package table: {error}")))?;
    let current: String = package
        .get("path")
        .map_err(|error| MagError::Eval(format!("load Lua package path: {error}")))?;
    let prefix = patterns
        .iter()
        .map(|pattern| pattern.display().to_string())
        .collect::<Vec<_>>()
        .join(";");
    package
        .set("path", format!("{prefix};{current}"))
        .map_err(|error| MagError::Eval(format!("set Lua package path: {error}")))?;
    Ok(())
}

fn install_registry_host(lua: &Lua) -> Result<(), MagError> {
    let nefor = lua
        .create_table()
        .map_err(|error| MagError::Eval(format!("create registry host: {error}")))?;
    nefor
        .set(
            "log",
            lua.create_function(|_, _: String| Ok(()))
                .map_err(|error| MagError::Eval(format!("create registry log binding: {error}")))?,
        )
        .map_err(|error| MagError::Eval(format!("install registry log binding: {error}")))?;
    let semantic = lua
        .create_table()
        .map_err(|error| MagError::Eval(format!("create semantic host: {error}")))?;
    semantic
        .set(
            "id",
            lua.create_function(|lua, descriptor: Value| {
                let descriptor: JsonValue = lua.from_value(descriptor)?;
                let descriptor = crate::json::concrete_type_from_json(&descriptor)
                    .map_err(|error| mlua::Error::runtime(error.to_string()))?;
                Ok(descriptor.stable_id().to_string())
            })
            .map_err(|error| MagError::Eval(format!("create semantic id binding: {error}")))?,
        )
        .map_err(|error| MagError::Eval(format!("install semantic id binding: {error}")))?;
    nefor
        .set("semantic_type", semantic)
        .map_err(|error| MagError::Eval(format!("install semantic host: {error}")))?;
    lua.globals()
        .set("nefor", nefor)
        .map_err(|error| MagError::Eval(format!("install registry host: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::load_registry_contracts;

    #[test]
    fn loads_shipped_kernel_registry_with_shared_lua_modules() {
        let registry = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/mag/lua/mag-kernel/init.lua");

        let contracts = load_registry_contracts(&registry)
            .expect("shipped kernel registry should resolve the shared Lua tree");

        assert!(
            contracts
                .as_array()
                .is_some_and(|entries| !entries.is_empty()),
            "shipped kernel registry should export foreign contracts"
        );
    }
}
