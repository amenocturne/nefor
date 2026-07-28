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
          provider = "mock-provider", schema = schema, tools = { "read_file" },
          max_corrections = 2,
          provider_error_type = "provider-error-tag",
          validation_error_type = "validation-error-tag"
        }, emit)
        assert(actor, err)

        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "user", content = "plan" }}
        }}}})
        assert(emitted[#emitted].kind == "capability.invoke")
        local first_request = emitted[#emitted].request
        assert(first_request.output_schema.version == 1)
        assert(first_request.max_corrections == 2)

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
        assert(emitted[#emitted - 1].kind == "nefor.agent.Result")
        assert(emitted[#emitted - 1].variant == "success")
        assert(emitted[#emitted - 1].value.task == "build")
      "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn exhausted_corrections_emit_agent_error_without_attempt_count() {
    let lua = harness();
    lua.load(
        r#"
        local factory = require("factories.structured-output")
        local emitted = {}
        local actor = assert(factory.construct("typed", {
          provider = "mock-provider",
          schema = { version = 1, root = { kind = "string" } },
          max_corrections = 2,
          provider_error_type = "provider-error-tag",
          validation_error_type = "validation-error-tag"
        }, function(message) emitted[#emitted + 1] = message end))
        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "user", content = "answer" }}
        }}}})
        actor.deliver({ kind = "reply", result = { text = "1" } })
        actor.deliver({ kind = "reply", result = { text = "false" } })
        actor.deliver({ kind = "reply", result = { text = "null" } })
        local terminal = emitted[#emitted - 1]
        assert(terminal.kind == "nefor.agent.Result")
        assert(terminal.variant == "error")
        assert(terminal.value.reason.type == "validation-error-tag")
        assert(terminal.value.reason.value.violations[1].path == "$")
        assert(terminal.value.reason.value.attempts == nil)
        assert(terminal.value.last_output.text == "null")
      "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn zero_corrections_rejects_the_initial_invalid_candidate() {
    let lua = harness();
    lua.load(
        r#"
        local factory = require("factories.structured-output")
        local emitted = {}
        local actor = assert(factory.construct("no-retry", {
          provider = "mock-provider",
          schema = { version = 1, root = { kind = "string" } },
          max_corrections = 0,
          provider_error_type = "provider-error-tag",
          validation_error_type = "validation-error-tag"
        }, function(message) emitted[#emitted + 1] = message end))
        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "user", content = "answer" }}
        }}}})
        actor.deliver({ kind = "reply", result = { text = "1", raw = "candidate" } })
        local terminal = emitted[#emitted - 1]
        assert(terminal.kind == "nefor.agent.Result")
        assert(terminal.variant == "error")
        assert(terminal.value.last_output.raw == "candidate")
        assert(terminal.value.reason.value.violations[1].code == "wrong_type")
        for _, message in ipairs(emitted) do
          assert(not (message.kind == "capability.invoke" and message.ref == "no-retry@r2"))
        end
      "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn result_selector_preserves_values_and_rejects_unknown_variants() {
    let lua = harness();
    lua.load(
        r#"
        local factory = require("factories.select-agent-result")
        local emitted = {}
        local actor = assert(factory.construct("select", {},
          function(message) emitted[#emitted + 1] = message end))
        local success = actor.deliver({ messages = {{ message = {
          variant = "success", value = { answer = 42 }
        }}}})
        assert(success.status == "ok")
        assert(emitted[#emitted].kind == "nefor.agent.Output")
        assert(emitted[#emitted].value.answer == 42)
        local failure = actor.deliver({ messages = {{ message = {
          variant = "error", value = { last_output = "draft" }
        }}}})
        assert(failure.status == "ok")
        assert(emitted[#emitted].kind == "nefor.agent.Error")
        assert(emitted[#emitted].value.last_output == "draft")
        local malformed = actor.deliver({ messages = {{ message = { value = 1 } }}})
        assert(malformed.status == "failed")
      "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn ordinary_string_output_accepts_the_empty_string() {
    let lua = harness();
    lua.load(
        r#"
        local factory = require("factories.structured-output")
        local emitted = {}
        local actor = assert(factory.construct("empty-string", {
          provider = "mock-provider",
          schema = { version = 1, root = { kind = "string" } },
          max_corrections = 0,
          provider_error_type = "provider-error-tag",
          validation_error_type = "validation-error-tag"
        }, function(message) emitted[#emitted + 1] = message end))
        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "user", content = "answer" }}
        }}}})
        actor.deliver({ kind = "reply", result = { text = [[""]] } })
        local terminal = emitted[#emitted - 1]
        assert(terminal.kind == "nefor.agent.Result")
        assert(terminal.variant == "success")
        assert(terminal.value == "")
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
            schema = { version = 1, root = { kind = "string" } },
            max_corrections = 1,
            provider_error_type = "provider-error-tag",
            validation_error_type = "validation-error-tag"
          }, function(message) emitted[#emitted + 1] = message end))
          actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
            messages = {{ role = "user", content = "answer" }}
          }}}})
          return actor, emitted
        end

        local correlation, correlation_out = new_actor("correlation")
        correlation.deliver({ kind = "reply", error = "provider unavailable" })
        local correlation_error = correlation_out[#correlation_out - 1]
        assert(correlation_error.kind == "nefor.agent.Result")
        assert(correlation_error.variant == "error")
        assert(correlation_error.value.reason.type == "provider-error-tag")
        assert(correlation_error.value.reason.value.message == "provider unavailable")
        assert(correlation_error.value.reason.value.detail.present == false)
        assert(nefor.json.encode(correlation_error.value.last_output) == "null")

        local detailed, detailed_out = new_actor("detailed")
        detailed.deliver({ kind = "reply", error = {
          message = "provider message", detail = "provider-owned detail"
        }})
        local detailed_error = detailed_out[#detailed_out - 1].value.reason.value
        assert(detailed_error.message == "provider message")
        assert(detailed_error.detail.present == true)
        assert(detailed_error.detail.value == "provider-owned detail")

        local in_band, in_band_out = new_actor("in-band")
        in_band.deliver({ kind = "reply", result = {
          finish_reason = "error", error = "upstream refused"
        }})
        assert(in_band_out[#in_band_out - 1].value.reason.value.message == "upstream refused")

        for _, bad_detail in ipairs({ "", false, { nested = "not a message" } }) do
          local malformed, malformed_out = new_actor("malformed-detail")
          malformed.deliver({ kind = "reply", result = {
            finish_reason = "error", error = bad_detail
          }})
          assert(malformed_out[#malformed_out - 1].kind == "nefor.agent.Result")
          assert(malformed_out[#malformed_out - 1].value.reason.value.message
            == 'provider returned finish_reason "error" with no detail')
        end

        local killed, killed_out = new_actor("killed")
        killed.deliver({ kind = "reply", result = { text = "1" } })
        assert(killed_out[#killed_out].kind == "capability.invoke")
        killed.handle_kill()
        assert(killed_out[#killed_out].kind == "mock-provider.chat.cancel")

        local retained, retained_out = new_actor("retained")
        retained.deliver({ kind = "reply", result = { text = "1", marker = "first" } })
        retained.deliver({ kind = "reply", error = "correction transport failed" })
        local retained_error = retained_out[#retained_out - 1]
        assert(retained_error.value.reason.value.message == "correction transport failed")
        assert(retained_error.value.last_output.marker == "first")

        local after_tool, after_tool_out = new_actor("after-tool")
        after_tool.deliver({ kind = "reply", result = { marker = "tool-round", tool_calls = {{
          id = "call-1", name = "read_file", args = { path = "x" }
        }} }})
        after_tool.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "tool", content = "done", tool_call_id = "call-1" }}
        }}}})
        after_tool.deliver({ kind = "reply", error = "post-tool provider failure" })
        local tool_error = after_tool_out[#after_tool_out - 1]
        assert(tool_error.value.last_output.marker == "tool-round")

        local drained, drained_out = new_actor("drained")
        drained.deliver({ kind = "reply", result = { text = "1" } })
        drained.handle_drain()
        drained.deliver({ kind = "reply", result = { text = "false" } })
        assert(drained_out[#drained_out].kind == "mag.failed")
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
          schema = { version = 1, root = { kind = "string" } },
          max_corrections = 2,
          provider_error_type = "provider-error-tag",
          validation_error_type = "validation-error-tag"
        }, function(message) emitted[#emitted + 1] = message end))
        local function activate(content)
          actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
            messages = {{ role = "user", content = content }}
          }}}})
        end

        activate("first")
        actor.deliver({ kind = "reply", result = { text = [["ok"]] } })
        assert(emitted[#emitted - 1].variant == "success")

        activate("second")
        actor.deliver({ kind = "reply", result = { text = "1" } })
        actor.deliver({ kind = "reply", result = { tool_calls = {{
          id = "call", name = "read_file", args = {}
        }} }})
        activate("tool result")
        actor.deliver({ kind = "reply", result = { text = "false" } })
        actor.deliver({ kind = "reply", result = { text = "null" } })
        local terminal = emitted[#emitted - 1]
        assert(terminal.variant == "error")
        assert(terminal.value.reason.value.violations ~= nil)
      "#,
    )
    .exec()
    .unwrap();
}
