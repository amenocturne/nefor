use mlua::{Lua, LuaSerdeExt, Table, Value as LuaValue};
use serde_json::{Map, Value};
use std::path::PathBuf;

pub fn install(lua: &Lua) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let package: Table = lua.globals().get("package").unwrap();
    let current: String = package.get("path").unwrap();
    package.set("path", format!("{0}/lua/?.lua;{0}/lua/?/init.lua;{0}/plugins/openai-provider/lua/?.lua;{0}/plugins/openai-provider/lua/?/init.lua;{0}/plugins/chatgpt-provider/lua/?.lua;{0}/plugins/chatgpt-provider/lua/?/init.lua;{current}", root.display())).unwrap();
    lua.load(include_str!("canonical_conversation.lua"))
        .exec()
        .unwrap();
}

pub fn handle(lua: &Lua, body: &Map<String, Value>, translator: &str) -> Value {
    let f: mlua::Function = lua.globals().get("canonical_test_handle").unwrap();
    let output: LuaValue = f
        .call(("mag", lua.to_value(body).unwrap(), translator))
        .unwrap();
    lua.from_value(output).unwrap()
}

#[allow(dead_code)]
pub struct CanonicalConversation {
    lua: Lua,
    translator: &'static str,
    pub native_request: Option<Map<String, Value>>,
}

#[allow(dead_code)]
impl CanonicalConversation {
    pub fn new(translator: &'static str) -> Self {
        let lua = Lua::new();
        let nefor = lua.create_table().unwrap();
        let json = lua.create_table().unwrap();
        json.set(
            "encode",
            lua.create_function(|lua, value: LuaValue| {
                let value: Value = lua.from_value(value)?;
                serde_json::to_string(&value).map_err(mlua::Error::external)
            })
            .unwrap(),
        )
        .unwrap();
        json.set(
            "decode",
            lua.create_function(|lua, source: String| {
                let value: Value = serde_json::from_str(&source).map_err(mlua::Error::external)?;
                lua.to_value(&value)
            })
            .unwrap(),
        )
        .unwrap();
        json.set("null", lua.null()).unwrap();
        json.set(
            "mark_array",
            lua.create_function(|lua, value: Table| {
                value.set_metatable(Some(lua.array_metatable()));
                Ok(value)
            })
            .unwrap(),
        )
        .unwrap();
        json.set(
            "is_array",
            lua.create_function(|lua, value: Table| {
                Ok(value
                    .metatable()
                    .is_some_and(|meta| meta == lua.array_metatable()))
            })
            .unwrap(),
        )
        .unwrap();
        nefor.set("json", json).unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        install(&lua);
        Self {
            lua,
            translator,
            native_request: None,
        }
    }

    pub async fn service(
        &mut self,
        body: &Map<String, Value>,
        stdin: &mut tokio::process::ChildStdin,
    ) {
        use tokio::io::AsyncWriteExt;
        let output = handle(&self.lua, body, self.translator);
        for event in output["events"].as_array().unwrap() {
            let line = nefor_protocol::Envelope::event(
                nefor_protocol::PluginName::new(event["from"].as_str().unwrap()).unwrap(),
                nefor_protocol::Timestamp::now(),
                event["body"].as_object().unwrap().clone(),
            )
            .to_line();
            stdin.write_all(line.as_bytes()).await.unwrap();
            stdin.write_all(b"\n").await.unwrap();
        }
        stdin.flush().await.unwrap();
        for request in output["delivered"].as_array().unwrap() {
            self.native_request = Some(request.as_object().unwrap().clone());
        }
    }
}
