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
    nefor.set("typed_json", typed).unwrap();
    lua.globals().set("nefor", nefor).unwrap();

    let package: Table = lua.globals().get("package").unwrap();
    let current: String = package.get("path").unwrap();
    let root = lua_root();
    package
        .set(
            "path",
            format!(
                "{}/?.lua;{}/?/init.lua;{current}",
                root.display(),
                root.display()
            ),
        )
        .unwrap();
    lua
}

#[test]
fn structured_output_retries_and_preserves_tool_rounds() {
    let lua = harness();
    lua.load(
        r#"
        local factory = require("factories.structured-output")
        local emitted = {}
        local function emit(message) emitted[#emitted + 1] = message end
        local schema = {
          version = 1,
          root = { kind = "record", fields = {
            { name = "task", schema = { kind = "string" } }
          }}
        }
        local actor, err = factory.construct("typed", {
          provider = "mock-provider", schema = schema, tools = { "read_file" }
        }, emit)
        assert(actor, err)

        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "user", content = "plan" }}
        }}}})
        assert(emitted[#emitted].kind == "capability.invoke")
        local first_request = emitted[#emitted].request
        assert(first_request.input.messages[#first_request.input.messages].content:find("bare JSON"))

        actor.deliver({ kind = "reply", result = { tool_calls = {{
          id = "call-1", name = "read_file", args = { path = "x" }
        }} }})
        assert(emitted[#emitted - 1].kind == "generic-tool.ToolCalls")

        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "tool", content = "done", tool_call_id = "call-1" }}
        }}}})
        actor.deliver({ kind = "reply", result = { text = "not json" } })
        assert(emitted[#emitted].kind == "capability.invoke")
        local correction = emitted[#emitted].request.input.messages
        correction = correction[#correction].content
        assert(correction:find("malformed_json"), correction)

        actor.deliver({ kind = "reply", result = { text = [[{"task":4}]] } })
        assert(emitted[#emitted].kind == "capability.invoke")
        local messages = emitted[#emitted].request.input.messages
        assert(messages[#messages].content:find("%$%.task"))

        actor.deliver({ kind = "reply", result = { text = [[{"task":"build"}]] } })
        assert(emitted[#emitted - 1].kind == "nefor.structured.Validated")
        assert(emitted[#emitted - 1].tag == "core.validated.Valid")
        assert(emitted[#emitted - 1].value.task == "build")
      "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn three_invalid_final_answers_emit_typed_terminal_error() {
    let lua = harness();
    lua.load(
        r#"
        local factory = require("factories.structured-output")
        local emitted = {}
        local actor = assert(factory.construct("typed", {
          provider = "mock-provider",
          schema = { version = 1, root = { kind = "string" } }
        }, function(message) emitted[#emitted + 1] = message end))
        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "user", content = "answer" }}
        }}}})
        actor.deliver({ kind = "reply", result = { text = "1" } })
        actor.deliver({ kind = "reply", result = { text = "false" } })
        actor.deliver({ kind = "reply", result = { text = "null" } })
        local terminal = emitted[#emitted - 1]
        assert(terminal.kind == "nefor.structured.Validated")
        assert(terminal.tag == "core.validated.Invalid")
        assert(terminal.errors[1].attempts == 3)
        assert(terminal.errors[1].kind == "schema_violation")
      "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn provider_failures_and_retry_signals_preserve_boundary_lifecycle() {
    let lua = harness();
    lua.load(
        r#"
        local factory = require("factories.structured-output")
        local function new_actor(id)
          local emitted = {}
          local actor = assert(factory.construct(id, {
            provider = "mock-provider",
            schema = { version = 1, root = { kind = "string" } }
          }, function(message) emitted[#emitted + 1] = message end))
          actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
            messages = {{ role = "user", content = "answer" }}
          }}}})
          return actor, emitted
        end

        local correlation, correlation_out = new_actor("correlation")
        correlation.deliver({ kind = "reply", error = "provider unavailable" })
        assert(correlation_out[#correlation_out].kind == "mag.failed")
        assert(correlation_out[#correlation_out].value.error == "provider unavailable")

        local in_band, in_band_out = new_actor("in-band")
        in_band.deliver({ kind = "reply", result = {
          finish_reason = "error", error = "upstream refused"
        }})
        assert(in_band_out[#in_band_out].kind == "mag.failed")
        assert(in_band_out[#in_band_out].value.error == "upstream refused")

        for _, bad_detail in ipairs({ "", false, { nested = "not a message" } }) do
          local malformed, malformed_out = new_actor("malformed-detail")
          malformed.deliver({ kind = "reply", result = {
            finish_reason = "error", error = bad_detail
          }})
          assert(malformed_out[#malformed_out].kind == "mag.failed")
          assert(malformed_out[#malformed_out].value.error
            == 'provider returned finish_reason "error" with no detail')
        end

        local killed, killed_out = new_actor("killed")
        killed.deliver({ kind = "reply", result = { text = "1" } })
        assert(killed_out[#killed_out].kind == "capability.invoke")
        killed.handle_kill()
        assert(killed_out[#killed_out].kind == "mock-provider.chat.cancel")

        local drained, drained_out = new_actor("drained")
        drained.deliver({ kind = "reply", result = { text = "1" } })
        drained.handle_drain()
        drained.deliver({ kind = "reply", result = { text = "false" } })
        local terminal = drained_out[#drained_out - 1]
        assert(terminal.kind == "nefor.structured.Validated")
        assert(terminal.tag == "core.validated.Invalid")
        assert(terminal.errors[1].kind == "drained")
        assert(terminal.errors[1].attempts == 2)
      "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn fresh_activation_resets_attempts_but_tool_continuation_does_not() {
    let lua = harness();
    lua.load(
        r#"
        local factory = require("factories.structured-output")
        local emitted = {}
        local actor = assert(factory.construct("repeat", {
          provider = "mock-provider",
          schema = { version = 1, root = { kind = "string" } }
        }, function(message) emitted[#emitted + 1] = message end))
        local function activate(content)
          actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
            messages = {{ role = "user", content = content }}
          }}}})
        end

        activate("first")
        actor.deliver({ kind = "reply", result = { text = [["ok"]] } })
        assert(emitted[#emitted - 1].tag == "core.validated.Valid")

        activate("second")
        actor.deliver({ kind = "reply", result = { text = "1" } })
        actor.deliver({ kind = "reply", result = { tool_calls = {{
          id = "call", name = "read_file", args = {}
        }} }})
        activate("tool result")
        actor.deliver({ kind = "reply", result = { text = "false" } })
        actor.deliver({ kind = "reply", result = { text = "null" } })
        local terminal = emitted[#emitted - 1]
        assert(terminal.tag == "core.validated.Invalid")
        assert(terminal.errors[1].attempts == 3)
      "#,
    )
    .exec()
    .unwrap();
}
