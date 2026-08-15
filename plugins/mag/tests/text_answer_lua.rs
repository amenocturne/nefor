use mlua::{Lua, LuaSerdeExt, Table};
use std::path::PathBuf;

fn harness() -> Lua {
    let lua = Lua::new();
    let nefor = lua.create_table().unwrap();
    let json = lua.create_table().unwrap();
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
    lua.globals().set("nefor", nefor).unwrap();
    let package: Table = lua.globals().get("package").unwrap();
    let current: String = package.get("path").unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel");
    let shared = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../lua");
    package
        .set(
            "path",
            format!(
                "{0}/?.lua;{0}/?/init.lua;{1}/?.lua;{1}/?/init.lua;{current}",
                root.display(),
                shared.display()
            ),
        )
        .unwrap();
    lua
}

#[test]
fn text_answer_consumes_multiline_terminal_text_without_schema_or_correction() {
    harness().load(r#"
      local factory = require("factories.llm")
      local emitted = {}
      local actor = assert(factory.construct("direct", {
        provider = "mock-provider", tools = {}, output_type = "text-answer-tag",
        error_type = "agent-error-tag", provider_error_type = "provider-error-tag"
      }, function(message) emitted[#emitted + 1] = message end,
        { conversation = { id = "direct:conversation", turn_id = "direct:turn", emit = function(_) end } }))
      actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
        messages = {{ role = "user", content = "answer normally" }}
      }}}})
      local request = emitted[#emitted].request
      assert(request.output_schema == nil)
      actor.deliver({ kind = "reply", result = { text = "first line\nsecond line" } })
      local terminal = emitted[#emitted - 1]
      assert(terminal.kind == "nefor.agent.Result")
      assert(terminal.semantic_type_id == "text-answer-tag")
      assert(terminal.value == "first line\nsecond line")
      assert(#emitted == 4, "direct text must not start a correction round")
    "#).exec().unwrap();
}
