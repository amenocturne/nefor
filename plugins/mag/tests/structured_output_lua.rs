use mlua::{Lua, LuaSerdeExt, Table, Value};
use nefor_mag::schema::TypeSchema;
use std::path::PathBuf;

fn lua_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel")
}

fn harness() -> Lua {
    let lua = Lua::new();
    let nefor = lua.create_table().unwrap();
    let json = lua.create_table().unwrap();
    json.set(
        "encode",
        lua.create_function(|lua, value: Value| {
            let value: serde_json::Value = lua.from_value(value)?;
            serde_json::to_string(&value).map_err(mlua::Error::external)
        })
        .unwrap(),
    )
    .unwrap();
    json.set(
        "decode",
        lua.create_function(|lua, source: String| {
            let value: serde_json::Value =
                serde_json::from_str(&source).map_err(mlua::Error::external)?;
            lua.to_value(&value)
        })
        .unwrap(),
    )
    .unwrap();
    nefor.set("json", json).unwrap();

    let typed = lua.create_table().unwrap();
    typed
        .set(
            "validate",
            lua.create_function(|lua, (schema, source): (Value, String)| {
                let value: serde_json::Value = lua.from_value(schema)?;
                let schema: TypeSchema =
                    serde_json::from_value(value).map_err(mlua::Error::external)?;
                lua.to_value(&schema.validate_json(&source))
            })
            .unwrap(),
        )
        .unwrap();
    typed
        .set(
            "schema",
            lua.create_function(|lua, schema: Value| {
                let value: serde_json::Value = lua.from_value(schema)?;
                let schema: TypeSchema =
                    serde_json::from_value(value).map_err(mlua::Error::external)?;
                lua.to_value(&schema.to_json_schema())
            })
            .unwrap(),
        )
        .unwrap();
    nefor.set("typed_json", typed).unwrap();
    lua.globals().set("nefor", nefor).unwrap();

    let package: Table = lua.globals().get("package").unwrap();
    let current: String = package.get("path").unwrap();
    let root = lua_root();
    let shared_lua = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../lua");
    package
        .set(
            "path",
            format!(
                "{0}/?.lua;{0}/?/init.lua;{1}/?.lua;{1}/?/init.lua;{current}",
                root.display(),
                shared_lua.display()
            ),
        )
        .unwrap();
    lua
}

#[test]
fn typed_draft_submission_contract() {
    let lua = harness();
    lua.load(include_str!(
        "../../../tests/lua/mag-kernel/structured_output_test.lua"
    ))
    .exec()
    .unwrap();
}
