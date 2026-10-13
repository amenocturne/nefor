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
      -- This test is not about compaction: the actor's policy is explicitly
      -- Disabled, and the provider boundary gets the kernel tests' inert
      -- transport for the canonical receipts every request waits for.
      local transport = dofile(package.searchpath("kinds", package.path):match("^(.*)/")
        .. "/../../../../tests/lua/mag-kernel/async_compaction_fixture.lua").disabled()
      local emitted = {}
      local actor = assert(factory.construct("direct", {
        provider = "mock-provider", tools = {}, output_type = "text-answer-tag",
        tool_approval_policy = { constructor = "Default" },
        compaction_policy = { constructor = "Disabled" },
        error_type = "agent-error-tag", provider_error_type = "provider-error-tag"
      }, function(message) emitted[#emitted + 1] = message end,
        { compaction = transport.facet,
          conversation = { id = "direct:conversation", turn_id = "direct:turn", emit = function(_) end } }))
      transport.wrap(actor)
      actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
        messages = {{ role = "user", content = "answer normally" }}
      }}}})
      local invocation = emitted[#emitted]
      local request = invocation.request
      assert(request.output_schema == nil)
      actor.deliver({ kind = "reply", ref = invocation.ref,
        result = { text = "first line\nsecond line" } })
      local terminal = emitted[#emitted - 1]
      assert(terminal.kind == "nefor.agent.Result")
      assert(terminal.semantic_type_id == nil)
      assert(terminal.constructor_id == nil)
      assert(terminal.value.constructor == "Ok")
      assert(terminal.value.value == "first line\nsecond line")
      assert(#emitted == 4, "direct text must not start a correction round")
    "#).exec().unwrap();
}
