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
            "provider_schema",
            lua.create_function(|lua, schema: Value| {
                let value: serde_json::Value = lua.from_value(schema)?;
                let schema: TypeSchema =
                    serde_json::from_value(value).map_err(mlua::Error::external)?;
                let provider = schema.to_provider_schema().map_err(mlua::Error::external)?;
                lua.to_value(&provider)
            })
            .unwrap(),
        )
        .unwrap();
    typed
        .set(
            "validate_provider",
            lua.create_function(|lua, (schema, source): (Value, String)| {
                let value: serde_json::Value = lua.from_value(schema)?;
                let schema: TypeSchema =
                    serde_json::from_value(value).map_err(mlua::Error::external)?;
                lua.to_value(&schema.validate_provider_json(&source))
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
        local facts = {}
        local function last_text()
          for i = #facts, 1, -1 do
            local chunk = facts[i].chunk
            if type(chunk) == "table" and type(chunk.data) == "string" then
              return chunk.data
            end
          end
        end
        local function emit(message) emitted[#emitted + 1] = message end
        local schema = {
          version = 1,
          root = { kind = "record", fields = {
            { name = "task", schema = { kind = "string" } }
          }}
        }
        local actor, err = factory.construct("typed", {
          provider = "mock-provider", schema = schema, tools = { "read_file" },
          system = "canonical system",
          max_corrections = 2,
          output_type = "output-type-tag",
          error_type = "agent-error-tag",
          provider_error_type = "provider-error-tag",
          validation_error_type = "validation-error-tag"
        }, emit, { conversation = { id = "typed:conversation", turn_id = "typed:turn",
          emit = function(fact) facts[#facts + 1] = fact end } })
        assert(actor, err)

        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "user", content = "plan" }}
        }}}})
        assert(emitted[#emitted].kind == "capability.invoke")
        local first_request = emitted[#emitted].request
        assert(first_request.output_schema.type == "object")
        assert(first_request.output_schema.properties.task.type == "string")
        assert(first_request.output_schema.required[1] == "task")
        assert(first_request.output_schema.additionalProperties == false)
        assert(first_request.max_corrections == 2)
        assert(first_request.input == nil)
        assert(first_request.system == nil)
        local seeded_system = false
        for _, fact in ipairs(facts) do
          if fact.kind == "message_started" and fact.role == "system" then
            assert(fact.turn_id == nil)
            seeded_system = true
          end
        end
        assert(seeded_system, "authored system is recorded once as a canonical message")

        actor.deliver({ kind = "reply", result = { tool_calls = {{
          id = "call-1", name = "read_file", args = { path = "x" }
        }} }})
        assert(emitted[#emitted - 1].kind == "generic-tool.ToolCalls")

        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "tool", content = "done", tool_call_id = "call-1" }}
        }}}})
        actor.deliver({ kind = "reply", result = { text = "not json" } })
        assert(emitted[#emitted].kind == "capability.invoke")
        assert(emitted[#emitted].request.input == nil)
        local correction = last_text()
        assert(correction:find("malformed_json"), correction)

        actor.deliver({ kind = "reply", result = { text = [[{"task":4}]] } })
        assert(emitted[#emitted].kind == "capability.invoke")
        assert(emitted[#emitted].request.input == nil)
        assert(last_text():find("%$%.task"))

        actor.deliver({ kind = "reply", result = { text = [[{"task":"build"}]] } })
        assert(emitted[#emitted - 1].kind == "nefor.agent.Result")
        assert(emitted[#emitted - 1].semantic_type_id == "output-type-tag")
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
        local facts = {}
        local actor = assert(factory.construct("typed", {
          provider = "mock-provider",
          schema = { version = 1, root = { kind = "string" } },
          max_corrections = 2,
          output_type = "output-type-tag",
          error_type = "agent-error-tag",
          provider_error_type = "provider-error-tag",
          validation_error_type = "validation-error-tag"
        }, function(message) emitted[#emitted + 1] = message end,
          { conversation = { id = "typed:conversation", turn_id = "typed:turn", emit = function(_) end } }))
        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "user", content = "answer" }}
        }}}})
        actor.deliver({ kind = "reply", result = { text = "1" } })
        actor.deliver({ kind = "reply", result = { text = "false" } })
        actor.deliver({ kind = "reply", result = { text = "null" } })
        local terminal = emitted[#emitted - 1]
        assert(terminal.kind == "nefor.agent.Result")
        assert(terminal.semantic_type_id == "agent-error-tag")
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
          output_type = "output-type-tag",
          error_type = "agent-error-tag",
          provider_error_type = "provider-error-tag",
          validation_error_type = "validation-error-tag"
        }, function(message) emitted[#emitted + 1] = message end,
          { conversation = { id = "no-retry:conversation", turn_id = "no-retry:turn", emit = function(_) end } }))
        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "user", content = "answer" }}
        }}}})
        actor.deliver({ kind = "reply", result = { text = "1", raw = "candidate" } })
        local terminal = emitted[#emitted - 1]
        assert(terminal.kind == "nefor.agent.Result")
        assert(terminal.semantic_type_id == "agent-error-tag")
        assert(terminal.value.last_output.raw == "candidate")
        assert(terminal.value.reason.value.violations[1].code == "invalid_provider_envelope")
        for _, message in ipairs(emitted) do
          assert(not (message.kind == "capability.invoke" and message.ref == "no-retry@r2"))
        end
      "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn wrapped_retry_requests_provider_envelope_and_named_union_routes_constructor() {
    let lua = harness();
    lua.load(
        r#"
        local factory = require("factories.structured-output")
        local emitted = {}
        local facts = {}
        local function last_text()
          for i = #facts, 1, -1 do
            local chunk = facts[i].chunk
            if type(chunk) == "table" and type(chunk.data) == "string" then
              return chunk.data
            end
          end
        end
        local actor = assert(factory.construct("named-union", {
          provider = "mock-provider",
          schema = { version = 1, root = { kind = "named", name = "Choice", body = {
            kind = "union", variants = {
              { tag = "left-tag", schema = { kind = "named", name = "Left",
                body = { kind = "record", fields = {{ name = "answer", schema = { kind = "int" } }} }
              }},
              { tag = "right-tag", schema = { kind = "named", name = "Right", body = { kind = "string" } } }
            }
          }}},
          max_corrections = 1,
          output_type = "declared-union-tag",
          error_type = "agent-error-tag",
          provider_error_type = "provider-error-tag",
          validation_error_type = "validation-error-tag"
        }, function(message) emitted[#emitted + 1] = message end,
          { conversation = { id = "named-union:conversation", turn_id = "named-union:turn",
            emit = function(fact) facts[#facts + 1] = fact end } }))
        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "user", content = "answer" }}
        }}}})
        actor.deliver({ kind = "reply", result = { text = [[{"type":"left-tag","value":{"answer":42}}]] } })
        local request = emitted[#emitted].request
        assert(request.input == nil)
        local prompt = last_text()
        assert(prompt:find([[{"value": <corrected value>}]], 1, true), prompt)
        actor.deliver({ kind = "reply", result = { text = [[{"value":{"type":"left-tag","value":{"answer":42}}}]] } })
        local terminal = emitted[#emitted - 1]
        assert(terminal.semantic_type_id == "left-tag")
        assert(terminal.value.answer == 42)
      "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn tagged_result_preserves_the_selected_constructor_without_a_selector() {
    let lua = harness();
    lua.load(
        r#"
        local factory = require("factories.structured-output")
        local emitted = {}
        local actor = assert(factory.construct("tagged", {
          provider = "mock-provider",
          schema = { version = 1, root = { kind = "union", variants = {
            { tag = "left-tag", schema = { kind = "named", name = "Left",
              body = { kind = "record", fields = {
                { name = "answer", schema = { kind = "int" } }
              }}}
            },
            { tag = "right-tag", schema = { kind = "named", name = "Right",
              body = { kind = "string" }}
            }
          }}},
          max_corrections = 0,
          output_type = "declared-union-tag",
          error_type = "agent-error-tag",
          provider_error_type = "provider-error-tag",
          validation_error_type = "validation-error-tag"
        }, function(message) emitted[#emitted + 1] = message end,
          { conversation = { id = "tagged:conversation", turn_id = "tagged:turn", emit = function(_) end } }))
        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut",
          message = { messages = {{ role = "user", content = "answer" }} } }}})
        actor.deliver({ kind = "reply",
          result = { text = [[{"value":{"type":"left-tag","value":{"answer":42}}}]] } })
        local terminal = emitted[#emitted - 1]
        assert(terminal.kind == "nefor.agent.Result")
        assert(terminal.semantic_type_id == "left-tag")
        assert(terminal.value.answer == 42)
        assert(terminal.variant == nil)
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
          output_type = "output-type-tag",
          error_type = "agent-error-tag",
          provider_error_type = "provider-error-tag",
          validation_error_type = "validation-error-tag"
        }, function(message) emitted[#emitted + 1] = message end,
          { conversation = { id = "empty-string:conversation", turn_id = "empty-string:turn", emit = function(_) end } }))
        actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
          messages = {{ role = "user", content = "answer" }}
        }}}})
        actor.deliver({ kind = "reply", result = { text = [[{"value":""}]] } })
        local terminal = emitted[#emitted - 1]
        assert(terminal.kind == "nefor.agent.Result")
        assert(terminal.semantic_type_id == "output-type-tag")
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
            output_type = "output-type-tag",
            error_type = "agent-error-tag",
            provider_error_type = "provider-error-tag",
            validation_error_type = "validation-error-tag"
          }, function(message) emitted[#emitted + 1] = message end,
            { conversation = { id = id .. ":conversation", turn_id = id .. ":turn", emit = function(_) end } }))
          actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
            messages = {{ role = "user", content = "answer" }}
          }}}})
          return actor, emitted
        end

        local correlation, correlation_out = new_actor("correlation")
        correlation.deliver({ kind = "reply", error = "provider unavailable" })
        local correlation_error = correlation_out[#correlation_out - 1]
        assert(correlation_error.kind == "nefor.agent.Result")
        assert(correlation_error.semantic_type_id == "agent-error-tag")
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
        local before_kill = #killed_out
        killed.handle_kill()
        assert(#killed_out == before_kill)

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
          output_type = "output-type-tag",
          error_type = "agent-error-tag",
          provider_error_type = "provider-error-tag",
          validation_error_type = "validation-error-tag"
        }, function(message) emitted[#emitted + 1] = message end,
          { conversation = { id = "repeat:conversation", turn_id = "repeat:turn", emit = function(_) end } }))
        local function activate(content)
          actor.deliver({ messages = {{ tag = "generic-provider.ProviderOut", message = {
            messages = {{ role = "user", content = content }}
          }}}})
        end

        activate("first")
        actor.deliver({ kind = "reply", result = { text = [[{"value":"ok"}]] } })
        assert(emitted[#emitted - 1].semantic_type_id == "output-type-tag")

        activate("second")
        actor.deliver({ kind = "reply", result = { text = "1" } })
        actor.deliver({ kind = "reply", result = { tool_calls = {{
          id = "call", name = "read_file", args = {}
        }} }})
        activate("tool result")
        actor.deliver({ kind = "reply", result = { text = "false" } })
        actor.deliver({ kind = "reply", result = { text = "null" } })
        local terminal = emitted[#emitted - 1]
        assert(terminal.semantic_type_id == "agent-error-tag")
        assert(terminal.value.reason.value.violations ~= nil)
      "#,
    )
    .exec()
    .unwrap();
}
