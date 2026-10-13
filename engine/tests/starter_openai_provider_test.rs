//! Provider compositor contracts driven through the production Lua callbacks.

use std::path::PathBuf;

use mlua::{Function, Lua, Table, Value};

fn starter_dir() -> PathBuf {
    repo_root().join("examples/nefor-agent")
}

fn lua_dir() -> PathBuf {
    repo_root().join("lua")
}

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

#[test]
fn starter_chatgpt_command_has_no_web_search_bypass_and_compositor_advertises_routed_tools() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(
        r#"
        local command = require("config.provider_command")
        local argv = command.chatgpt("/bin/chatgpt-provider", {
          name = "chatgpt", base_url = "http://localhost",
          web_search = "cached", extra_args = { "--future", "value" },
        })
        local expected = {
          "/bin/chatgpt-provider", "--name", "chatgpt",
          "--base-url", "http://localhost", "--future", "value",
        }
        assert(#argv == #expected)
        for i, value in ipairs(expected) do assert(argv[i] == value) end

        local spec = require("libs.compositors.provider").spawn_spec(
          "chatgpt", { "/bin/true" }, {
            translator_lib = "chatgpt-provider", tool_gate = "selected-gate",
            agentic_loop = {}, conversations = { context = function() return { messages = {} } end },
          })
        spec.to_plugin({{ type = "event", from = "selected-gate", body = {
          kind = "selected-gate.hello",
        } }})
        local sent = _test.sent()
        assert(#sent == 1)
        assert(sent[1].kind == "selected-gate.tools.advertise")
        assert(sent[1].body.source == "chatgpt")
        assert(#sent[1].body.tools == 6)
        local expected_tools = {
          web_search = true, web_open = true, web_click = true,
          web_find = true, web_image_search = true, web_screenshot = true,
        }
        for _, web in ipairs(sent[1].body.tools) do
          assert(expected_tools[web.name])
          expected_tools[web.name] = nil
          assert(web.execution.kind == "routed")
          assert(web.execution.provider == nil)
        end
        assert(next(expected_tools) == nil)
        "#,
    )
    .exec()
    .expect("verify ChatGPT command and provider-owned advertisement");
}

#[test]
fn chatgpt_direct_terminals_keep_provider_output_on_canonical_completion_events() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    let root = repo_root();
    let shared_lua = root.join("lua");
    let openai_lua = root.join("plugins/openai-provider/lua");
    let chatgpt_lua = root.join("plugins/chatgpt-provider/lua");
    lua.load(format!(
        r#"
        package.path = table.concat({{
          "{chatgpt}/?.lua", "{chatgpt}/?/init.lua",
          "{openai}/?.lua", "{openai}/?/init.lua",
          "{shared}/?.lua", "{shared}/?/init.lua",
          package.path,
        }}, ";")
        local t = require("chatgpt-provider").translator("chatgpt-provider")
        local function translate(output)
          return t.outbound({{
            type = "event", from = "chatgpt-provider",
            body = {{
              kind = "chatgpt-provider.chat.complete.result",
              chat_id = "request-1", finish_reason = output.finish_reason,
              output = output,
            }},
          }})
        end
        local text = translate({{ text = "canonical answer", finish_reason = "stop" }})
        assert(text.kind == "chatgpt-provider.completion.event")
        assert(text.request_id == "request-1" and text.chat_id == nil)
        assert(text.event == "completed" and text.output == nil)
        assert(text.result.text == "canonical answer")
        assert(text.result.finish_reason == "stop")

        local tools = translate({{
          text = "", finish_reason = "tool_calls",
          tool_calls = {{{{ id = "call-1", name = "read_file", arguments = {{ path = "x" }} }}}},
        }})
        assert(tools.event == "completed")
        assert(#tools.result.tool_calls == 1)
        assert(tools.result.tool_calls[1].name == "read_file")
        assert(tools.result.tool_calls[1].arguments.path == "x")
        local early_error = t.outbound({{
          type = "event", from = "chatgpt-provider",
          body = {{
            kind = "chatgpt-provider.turn.error", chat_id = "request-1",
            message = "stream failed",
          }},
        }})
        assert(early_error == nil)

        local failed = translate({{
          text = "partial", finish_reason = "error", error = "stream failed",
        }})
        assert(failed.event == "failed")
        assert(failed.error == "stream failed")
        assert(failed.result.text == "partial")
        "#,
        chatgpt = chatgpt_lua.display(),
        openai = openai_lua.display(),
        shared = shared_lua.display(),
    ))
    .exec()
    .expect("translate ChatGPT terminals");
}

#[test]
fn openai_lowering_rejects_non_object_canonical_tool_arguments() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(
        r#"
        local t = require("openai-provider").translator("openai")
        local valid = t.context_message({ role = "assistant", tool_calls = {
          { id = "empty", name = "zero", arguments = {} },
          { id = "required", name = "read", arguments = { path = "x" } },
        }})
        for _, call in ipairs(valid.tool_calls) do
          local decoded = nefor.json.decode(call["function"].arguments)
          assert(type(decoded) == "table" and not nefor.json.is_array(decoded))
        end
        for _, arguments in ipairs({ "", "{bad", '"scalar"', "null", "[]" }) do
          local ok = pcall(t.context_message, { role = "assistant", tool_calls = {
            { id = "bad", name = "read", arguments = arguments },
          }})
          assert(not ok, "strict continuation lowering must reject " .. arguments)
        end
        "#,
    )
    .exec()
    .expect("validate strict OpenAI history lowering");
}

#[test]
fn provider_adapter_owns_universal_compaction_lifecycle() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(
        r#"
        local op = require("libs.compositors.provider")
        local spec = op.spawn_spec("chatgpt", { "/bin/true" }, {
          agentic_loop = {}, translator_lib = "chatgpt-provider", tool_gate = "tool-gate",
          conversations = {
            context = function(_, conversation_id)
              assert(conversation_id == "lead")
              return {
                messages = {{ role = "user", content = "full context" }},
                tail_messages = {}, watermark = 7,
              }
            end,
          },
        })
        spec.to_plugin({{
          type = "event", from = "conversation-manager",
          body = {
            kind = "conversation.projection.delta",
            conversation_id = "lead",
            change = {
              kind = "context_compaction_pending",
              provider = "chatgpt", model = "gpt-5.6-sol",
              context = { messages = {{role="user", content="frozen context"}} },
              compaction = { request_id = "compact-1" },
            },
          },
        }})
        local delivered = _test.delivered()
        assert(#delivered == 3)
        assert(delivered[1].kind == "chatgpt.chat.create")
        assert(delivered[1].conversation_id == "lead")
        assert(delivered[1].body.model == "gpt-5.6-sol")
        assert(delivered[2].kind == "chatgpt.chat.append")
        assert(delivered[3].kind == "chatgpt.chat.compact")

        spec.from_plugin({{
          type = "event", from = "chatgpt",
          body = {
            kind = "chatgpt.chat.compaction.commit",
            chat_id = "conversation-compact:compact-1",
            model = "gpt-5.6-sol",
            model_context_artifact = {
              kind = "responses.compaction",
              items = {{ type = "compaction", encrypted_content = "sealed" }},
            },
          },
        }})
        local sent = _test.sent()
        local cleanup = _test.delivered()
        assert(#sent == 1)
        assert(sent[1].kind == "conversation.context.compact.complete")
        assert(#cleanup == 1 and cleanup[1].kind == "chatgpt.chat.delete")
        "#,
    )
    .exec()
    .expect("drive universal compaction lifecycle");
}

#[test]
fn provider_adapter_normalizes_structured_compaction_errors() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(
        r#"
        package.preload["throwing-provider"] = function()
          return { translator = function(name)
            local t = require("openai-provider").translator(name)
            t.compaction_capability = function() return {kind="supported"} end
            t.compact_context = function()
              error({ code = "request_shape_invalid", message = "bad compaction request",
                detail = { field = "model" } }, 0)
            end
            return t
          end }
        end
        local op = require("libs.compositors.provider")
        local spec = op.spawn_spec("broken", { "/bin/true" }, {
          agentic_loop = {}, translator_lib = "throwing-provider",
          conversations = { context = function()
            return { messages = {{ role = "user", content = "context" }} }
          end },
        })
        spec.to_plugin({{
          type = "event", from = "conversation-manager", body = {
            kind = "conversation.projection.delta", conversation_id = "lead",
            change = { kind = "context_compaction_pending", provider = "broken",
              context = { messages = {{role="user", content="frozen context"}} },
              compaction = { request_id = "compact-table-error" } },
          },
        }})
        local sent = _test.sent()
        assert(#sent == 1)
        assert(sent[1].kind == "conversation.context.compact.failed")
        assert(sent[1].body.error.code == "request_shape_invalid")
        assert(sent[1].body.error.message == "bad compaction request")
        assert(sent[1].body.error.detail.field == "model")
        assert(not sent[1].body.error.message:find("table: 0x", 1, true))
        "#,
    )
    .exec()
    .expect("normalize table-valued compaction error");
}

#[test]
fn provider_adapter_recovers_the_concrete_missing_model_failure() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(
        r#"
        package.preload["returning-provider"] = function()
          return { translator = function(name)
            local t = require("openai-provider").translator(name)
            t.compaction_capability = function() return {kind="supported"} end
            t.compact_context = function()
              return nil, { code = "provider_compaction_failed",
                message = "no model configured: pass `model` in chat.create or set the provider default via model.set (the user picks via `/model` in the chat surface)",
                context = { field = "model", provider = name } }
            end
            return t
          end }
        end
        local spec = require("libs.compositors.provider").spawn_spec("returned", { "/bin/true" }, {
          agentic_loop = {}, translator_lib = "returning-provider",
          conversations = { context = function()
            return { messages = {{ role = "user", content = "context" }} }
          end },
        })
        spec.to_plugin({{ type = "event", from = "conversation-manager", body = {
          kind = "conversation.projection.delta", conversation_id = "lead",
          change = { kind = "context_compaction_pending", provider = "returned",
            context = { messages = {{role="user", content="frozen context"}} },
              compaction = { request_id = "compact-returned-error" } },
        } }})
        local failure = _test.sent()[1].body.error
        assert(failure.code == "provider_compaction_failed")
        assert(failure.message:find("no model configured", 1, true))
        assert(failure.detail.field == "model" and failure.detail.provider == "returned")
        assert(not failure.message:find("table: 0x", 1, true))
        "#,
    )
    .exec()
    .expect("recover concrete missing-model compaction failure");
}

#[test]
fn unsupported_provider_returns_universal_compaction_failure() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(
        r#"
        local op = require("libs.compositors.provider")
        local spec = op.spawn_spec("ollama", { "/bin/true" }, {
          agentic_loop = {},
          conversations = {
            context = function()
              return { messages = {{ role = "user", content = "context" }}, watermark = 3 }
            end,
          },
        })
        spec.to_plugin({{
          type = "event", from = "conversation-manager",
          body = {
            kind = "conversation.projection.delta",
            conversation_id = "lead",
            change = {
              kind = "context_compaction_pending",
              provider = "ollama",
              context = { messages = {{role="user", content="frozen context"}} },
              compaction = { request_id = "compact-2" },
            },
          },
        }})
        local sent = _test.sent()
        assert(#sent == 1)
        assert(sent[1].kind == "conversation.context.compact.failed")
        assert(sent[1].body.error.code == "compaction_unsupported")
        assert(sent[1].body.error.message == "context compaction is not supported by ollama")
        assert(not sent[1].body.error.message:find("table: 0x", 1, true))
        assert(#_test.delivered() == 0)
        "#,
    )
    .exec()
    .expect("unsupported compaction failure");
}

#[test]
fn provider_adapter_keeps_expanded_requests_private_and_reports_generic_events() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(
        r#"
        local op = require("libs.compositors.provider")
        local empty_tools = nefor.json.decode("[]")
        local schema = nefor.json.decode([[{
          "type":"object","required":[],
          "properties":{"tags":{"type":"array","prefixItems":[]}}
        }]])
        local spec = op.spawn_spec("ollama", { "/bin/true" }, {
          agentic_loop = {},
          request_additions = { usage = { include = true } },
          conversations = {
            context = function(_, conversation_id)
              assert(conversation_id == "conversation-1")
              return {
                messages = {
                  { role = "system", content = "private system" },
                  { role = "user", content = "private history" },
                },
                watermark = 11,
              }
            end,
            watermark = function() return 11 end,
          },
        })
        spec.to_plugin({{
          type = "event", from = "conversation-manager",
          body = {
            kind = "conversation.provider.invoke", approval_policy = "Default", provider = "ollama",
            request_id = "request-1", conversation_id = "conversation-1",
            watermark = 11, model = "qwen", tools = empty_tools,
            output_schema = schema,
            tool_specs = {{ name = "write_output", description = "private draft", parameters = schema, execution = {{ kind = "routed" }}, owner = "mag-runtime" }},
          },
        }})

        local delivered = _test.delivered()
        assert(#delivered == 1)
        assert(delivered[1].kind == "ollama.completion.request")
        assert(delivered[1].body.request_id == "request-1")
        assert(delivered[1].body.model == "qwen")
        assert(delivered[1].body.request_additions.usage.include == true)
        assert(nefor.json.is_array(delivered[1].body.tools))
        assert(#delivered[1].body.tools == 0)
        assert(nefor.json.is_array(delivered[1].body.output_schema.required))
        assert(nefor.json.is_array(delivered[1].body.tool_specs[1].parameters.required))
        assert(nefor.json.is_array(
          delivered[1].body.tool_specs[1].parameters.properties.tags.prefixItems))
        assert(#delivered[1].body.messages == 2)
        assert(delivered[1].body.messages[1].content == "private system")
        assert(delivered[1].body.messages[2].content == "private history")
        assert(#_test.sent() == 0)

        -- Even a fully expanded native request arriving on the public bus is
        -- inert. Only the compositor may construct and direct-deliver it.
        spec.to_plugin({{
          type = "event", from = "legacy-producer",
          body = {
            kind = "ollama.completion.request", request_id = "leaked",
            messages = {{ role = "user", content = "must not be delivered" }},
          },
        }})
        assert(#_test.delivered() == 0)

        spec.from_plugin({{
          type = "event", from = "ollama",
          body = {
            kind = "ollama.completion.event", request_id = "request-1",
            event = "text_delta", text = "hello", opaque = "preserved",
          },
        }})
        local sent = _test.sent()
        assert(#sent == 1)
        assert(sent[1].kind == "conversation.provider.event.reported")
        assert(sent[1].body.provider == "ollama")
        assert(sent[1].body.request_id == "request-1")
        assert(sent[1].body.event == "text_delta")
        assert(sent[1].body.text == "hello" and sent[1].body.opaque == "preserved")

        spec.from_plugin({{
          type = "event", from = "ollama",
          body = {
            kind = "ollama.completion.event", request_id = "request-1",
            event = "completed", result = { text = "hello" },
          },
        }})
        assert(#_test.sent() == 1)
        assert(next(spec._internals.pending_requests()) == nil)
        "#,
    )
    .exec()
    .expect("drive private provider request lifecycle");
}

#[test]
fn provider_usage_compositor_preserves_generic_ledger_invariants() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(
        r#"
        local provider = require("libs.compositors.provider")
        local function options()
          return {
            agentic_loop = {},
            conversations = { context = function() return { messages = {} } end,
              watermark = function() return 0 end },
            usage = {
              exposures = {
                { usage_id = "meter/native", initial = { kind = "unknown" } },
                { usage_id = "meter/a", initial = { kind = "unknown" } },
                { usage_id = "meter/b", initial = { kind = "unknown" } },
              },
              subscription = {
                usage_id = "meter/native", request_kind = "meter.native.request",
                updated_kind = "meter.native.updated", error_kind = "meter.native.error",
                extract = function(snapshot)
                  return { kind = "subscription", remaining = snapshot.remaining }
                end,
              },
              contributions = {
                { usage_id = "meter/a", extension = "cost_a", currency = "USD",
                  event_kind = "meter.usage.a.recorded" },
                { usage_id = "meter/b", extension = "cost_b", byok_extension = "byok_b",
                  currency = "EUR", event_kind = "meter.usage.b.recorded" },
              },
            },
          }
        end

        local spec = provider.spawn_spec("meter", { "/bin/true" }, options())

        -- A provider hello remains independently observable when it cannot
        -- produce a model acknowledgement. Usage initialization may follow
        -- on ready and through the provider-native snapshot without consuming
        -- or standing in for that liveness event.
        spec.from_plugin({{ type = "event", from = "meter", body = {
          kind = "meter.hello", name = "meter", version = "test" } }})
        local hello = _test.sent()
        assert(#hello == 1 and hello[1].kind == "meter.hello")
        assert(hello[1].body.name == "meter")
        spec.from_plugin({{ type = "event", from = "meter", body = {
          kind = "meter.ready" } }})
        local startup_usage = _test.sent()
        assert(#startup_usage == 1)
        assert(startup_usage[1].kind == "conversation.usage.expose")
        assert(startup_usage[1].body.usage_ids[1] == "meter/a")
        assert(startup_usage[1].body.usage_ids[3] == "meter/native")
        spec.from_plugin({{ type = "event", from = "meter", body = {
          kind = "meter.native.updated", remaining = 84 } }})
        assert(spec._internals.usage_value("meter/native").remaining == 84)
        assert(#_test.sent() == 0)
        local readiness = require("libs.startup-readiness")._new {
          required_plugins = { "meter" }, required_tools = {},
        }
        readiness.observe(hello[1].body, hello[1].from)
        assert(readiness.snapshot().ready)
        spec.to_plugin({{ type = "event", from = "sessions", body = {
          kind = "sessions.session_start", session_id = "fixture" } }})

        -- Exercise the actual generic completion-event shape emitted by the Rust
        -- provider: token facts and opaque extensions live under `usage`.
        spec.to_plugin({{ type = "event", from = "conversation-manager", body = {
          kind = "conversation.provider.invoke", approval_policy = "Default", provider = "meter",
          request_id = "r1", conversation_id = "c", watermark = 0 } }})
        _test.delivered()
        _test.sent()
        spec.from_plugin({{ type = "event", from = "meter", body = {
          kind = "meter.completion.event", event = "usage", request_id = "r1",
          completion_id = "same-completion", usage = { prompt_tokens = 11, completion_tokens = 7,
            extensions = { cost_a = 1e-7, cost_b = 2e-7, byok_b = false } } } }})
        assert(spec._internals.usage_value("meter/a").amount == "0.0000001")
        assert(spec._internals.usage_value("meter/b").amount == "0.0000002")
        local sent = _test.sent()
        local saw_a, saw_b, saw_context = false, false, false
        for _, event in ipairs(sent) do
          if event.kind == "meter.usage.a.recorded" then
            saw_a = event.body.amount == "0.0000001" and event.body.currency == "USD"
          elseif event.kind == "meter.usage.b.recorded" then
            saw_b = event.body.amount == "0.0000002" and event.body.currency == "EUR"
          elseif event.kind == "conversation.provider.context_usage" then
            saw_context = true
          end
        end
        assert(saw_a and saw_b and not saw_context)
        spec.from_plugin({{ type = "event", from = "meter", body = {
          kind = "meter.completion.event", event = "completed", request_id = "r1",
          model = "one" } }})
        local completed = _test.sent()
        local measured = false
        for _, event in ipairs(completed) do
          if event.kind == "conversation.provider.context_usage" then
            measured = event.body.latest_response_usage.total_tokens == 18
              and event.body.model == "one" and event.body.conversation_id == "c"
          end
        end
        assert(measured)

        -- Generic usage is provisional until a successful response; failure
        -- and late terminals cannot publish it as completed occupancy.
        local occupancy_options = options()
        occupancy_options.usage = nil
        local occupancy = provider.spawn_spec("meter", { "/bin/true" }, occupancy_options)
        occupancy.to_plugin({{ type = "event", from = "conversation-manager", body = {
          kind = "conversation.provider.invoke", approval_policy = "Default", provider = "meter", model = "one",
          request_id = "failed-usage", conversation_id = "c", watermark = 0 } }})
        _test.delivered()
        _test.sent()
        for _, event in ipairs({
          { event = "usage", usage = { input_tokens = 900, output_tokens = 100 } },
          { event = "error", message = "failed response" },
          { event = "completed", usage = { total_tokens = 1000 } },
        }) do
          event.kind = "meter.completion.event"
          event.request_id = "failed-usage"
          occupancy.from_plugin({{ type = "event", from = "meter", body = event }})
          for _, emitted in ipairs(_test.sent()) do
            assert(emitted.kind ~= "conversation.provider.context_usage",
              "failed or stale generic usage must not become occupancy")
          end
        end

        -- A contradiction permanently poisons only its ledger until session reset.
        spec._internals.record_configured_usage({ event = "usage", request_id = "r2",
          completion_id = "same-completion",
          usage = { extensions = { cost_a = 0.5, cost_b = 2e-7, byok_b = false } } })
        assert(spec._internals.usage_value("meter/a").reason == "completion_id_conflict")
        spec._internals.record_configured_usage({ event = "usage", request_id = "r3",
          completion_id = "later-valid",
          usage = { extensions = { cost_a = 1, cost_b = 3e-7, byok_b = false } } })
        assert(spec._internals.usage_value("meter/a").kind == "unknown")
        assert(spec._internals.usage_value("meter/b").amount == "0.0000005")

        -- A same-owner mixed query is one complete snapshot, not partial responses.
        _test.sent()
        spec.to_plugin({{ type = "event", from = "conversation-manager", body = {
          kind = "conversation.usage.query.forwarded", owner = "meter", request_id = "mixed",
          usage_ids = { "meter/native", "meter/b" } } }})
        assert(#_test.sent() == 0)
        spec.from_plugin({{ type = "event", from = "meter", body = {
          kind = "meter.native.updated", remaining = 42 } }})
        local snapshot = _test.sent()[1]
        assert(snapshot.kind == "conversation.usage.snapshot.reported")
        assert(snapshot.body.request_id == "mixed" and #snapshot.body.values == 2)

        -- Pending native correlations do not cross session boundaries.
        spec.from_plugin({{ type = "event", from = "meter", body = {
          kind = "meter.native.error", message = "temporarily unavailable" } }})
        _test.sent()
        spec.to_plugin({{ type = "event", from = "conversation-manager", body = {
          kind = "conversation.usage.query.forwarded", owner = "meter", request_id = "stale",
          usage_ids = { "meter/native" } } }})
        assert(#_test.sent() == 0)
        spec.to_plugin({{ type = "event", from = "sessions", body = {
          kind = "sessions.session_start", session_id = "next" } }})
        _test.sent()
        spec.from_plugin({{ type = "event", from = "meter", body = {
          kind = "meter.native.updated", remaining = 10 } }})
        for _, event in ipairs(_test.sent()) do
          assert(event.body.request_id ~= "stale")
        end
        spec._internals.record_configured_usage({ event = "usage", request_id = "reset",
          completion_id = "valid-after-reset",
          usage = { extensions = { cost_a = 1, cost_b = 2, byok_b = false } } })
        assert(spec._internals.usage_value("meter/a").amount == "1")

        -- Malformed durable data poisons replay and later valid records cannot heal it.
        local replayed = provider.spawn_spec("meter", { "/bin/true" }, options())
        _test.sent()
        replayed.to_plugin({{ type = "event", replay = true, from = "foreign", body = {
          kind = "meter.usage.a.recorded", usage_id = "meter/a",
          contribution_id = "foreign", amount = "9", currency = "USD" } }})
        assert(replayed._internals.usage_value("meter/a").kind == "unknown")
        replayed.to_plugin({{ type = "event", replay = true, from = "meter", body = {
          kind = "meter.usage.a.recorded", usage_id = "meter/a",
          contribution_id = "bad", amount = "1e-7", currency = "USD" } }})
        replayed.to_plugin({{ type = "event", replay = true, from = "meter", body = {
          kind = "meter.usage.a.recorded", usage_id = "meter/a",
          contribution_id = "good", amount = "0.5", currency = "USD" } }})
        assert(replayed._internals.usage_value("meter/a").reason
          == "malformed_contribution_record")
        assert(#_test.sent() == 0)

        -- Config errors are rejected while constructing the compositor.
        local invalid = options()
        invalid.usage.contributions[1].event_kind = nil
        local ok = pcall(provider.spawn_spec, "meter", { "/bin/true" }, invalid)
        assert(not ok)
        local duplicate_ledger = options()
        duplicate_ledger.usage.contributions[2].usage_id = "meter/a"
        ok = pcall(provider.spawn_spec, "meter", { "/bin/true" }, duplicate_ledger)
        assert(not ok)
        "#,
    )
    .exec()
    .expect("provider-owned usage fold");
}

#[test]
fn provider_adapter_keeps_newer_model_selection_pending_after_a_stale_ack() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(
        r#"
        local spec = require("libs.compositors.provider").spawn_spec(
          "openrouter", { "/bin/true" }, {
            agentic_loop = {},
            conversations = { context = function() return { messages = {} } end },
          })
        spec.to_plugin({{ type = "event", from = "chat", body = {
          kind = "chat.model.set", provider = "openrouter", model = "older",
        } }})
        spec.to_plugin({{ type = "event", from = "chat", body = {
          kind = "chat.model.set", provider = "openrouter", model = "newer",
        } }})
        _test.delivered()

        spec.from_plugin({{ type = "event", from = "openrouter", body = {
          kind = "openrouter.model.set_ack", model = "older",
        } }})
        _test.sent()
        spec.from_plugin({{ type = "event", from = "openrouter", body = {
          kind = "openrouter.turn.error", message = "newer was rejected",
        } }})
        local sent = _test.sent()
        assert(#sent == 1)
        assert(sent[1].kind == "chat.model.set_failed")
        assert(sent[1].body.provider == "openrouter")
        assert(sent[1].body.model == "newer")
        assert(sent[1].body.error == "newer was rejected")
        "#,
    )
    .exec()
    .expect("correlate stale model acknowledgement");
}

#[test]
fn provider_adapter_cancels_private_request_without_publishing_native_control() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(
        r#"
        local op = require("libs.compositors.provider")
        local spec = op.spawn_spec("ollama", { "/bin/true" }, {
          agentic_loop = {},
          conversations = {
            context = function()
              return { messages = {{ role = "user", content = "hello" }}, watermark = 1 }
            end,
          },
        })
        spec.to_plugin({{
          type = "event", from = "conversation-manager",
          body = {
            kind = "conversation.provider.invoke", approval_policy = "Default", provider = "ollama",
            request_id = "request-2", conversation_id = "conversation-1", watermark = 1,
          },
        }})
        assert(#_test.delivered() == 1)
        spec.to_plugin({{
          type = "event", from = "conversation-manager",
          body = { kind = "conversation.provider.cancel", request_id = "request-2" },
        }})
        local delivered = _test.delivered()
        assert(#delivered == 1)
        assert(delivered[1].kind == "ollama.completion.cancel")
        assert(delivered[1].body.request_id == "request-2")
        assert(#_test.sent() == 0)
        assert(next(spec._internals.pending_requests()) == nil)
        "#,
    )
    .exec()
    .expect("cancel private provider request");
}

fn install_stub_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    nefor::lua::bindings::install_json(lua, &nefor)?;

    // log.* — no-op
    let log_tbl = lua.create_table()?;
    let no_op: Function = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    log_tbl.set("info", no_op.clone())?;
    log_tbl.set("warn", no_op.clone())?;
    log_tbl.set("error", no_op.clone())?;
    log_tbl.set("debug", no_op.clone())?;
    nefor.set("log", log_tbl)?;

    // bus.on_event — accept handler registrations as a no-op. The
    // replay_window module no longer self-subscribes at require-time
    // (now wired explicitly by examples/nefor-agent/init.lua via
    // replay_window.install()); these tests drive the replay-window
    // flag via the module's public set() helper instead.
    let bus_tbl = lua.create_table()?;
    let on_event = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    bus_tbl.set("on_event", on_event)?;
    nefor.set("bus", bus_tbl)?;

    // engine — record `deliver(peer, payload)` and `send(payload)` calls
    // into globals that tests inspect via `_test`.
    let engine_tbl = lua.create_table()?;
    let sent_log = lua.create_table()?;
    lua.globals().set("_sent_log", sent_log)?;
    let send_fn = lua.create_function(|lua, args: mlua::Variadic<Value>| {
        let payload = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            _ => return Ok(()),
        };
        let json: Table = lua.globals().get::<Table>("nefor")?.get::<Table>("json")?;
        let decode: Function = json.get("decode")?;
        let decoded: Value = decode.call(lua.create_string(&payload)?)?;
        let env: Table = match decoded {
            Value::Table(t) => t,
            _ => return Ok(()),
        };
        let body: Table = env
            .get::<Value>("body")?
            .as_table()
            .cloned()
            .unwrap_or(lua.create_table()?);
        let message_role: Value = match body.get::<Value>("message").ok() {
            Some(Value::Table(m)) => m.get::<Value>("role").unwrap_or(Value::Nil),
            _ => Value::Nil,
        };
        let log: Table = lua.globals().get("_sent_log")?;
        let row = lua.create_table()?;
        row.set("from", env.get::<Value>("from")?)?;
        row.set("kind", body.get::<Value>("kind").unwrap_or(Value::Nil))?;
        row.set("body", body.clone())?;
        row.set(
            "chat_id",
            body.get::<Value>("chat_id").unwrap_or(Value::Nil),
        )?;
        row.set("role", message_role)?;
        let n = log.len()?;
        log.set(n + 1, row)?;
        Ok(())
    })?;
    engine_tbl.set("send", send_fn)?;
    let now_fn = lua.create_function(|_, _: ()| Ok("2026-05-07T00:00:00.000Z".to_owned()))?;
    engine_tbl.set("now", now_fn)?;

    let delivered_log = lua.create_table()?;
    lua.globals().set("_delivered_log", delivered_log)?;
    let deliver_fn = lua.create_function(|lua, args: mlua::Variadic<Value>| {
        let peer = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            _ => return Ok(()),
        };
        let payload = match args.get(1) {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            _ => return Ok(()),
        };
        // Decode the payload to surface the body fields tests assert on.
        let json: Table = lua.globals().get::<Table>("nefor")?.get::<Table>("json")?;
        let decode: Function = json.get("decode")?;
        let decoded: Value = decode.call(lua.create_string(&payload)?)?;
        let body: Table = match decoded {
            Value::Table(t) => t.get::<Value>("body")?,
            _ => Value::Nil,
        }
        .as_table()
        .cloned()
        .unwrap_or(lua.create_table()?);
        let kind: String = body
            .get::<Value>("kind")
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
        let chat_id: Value = body.get("chat_id").unwrap_or(Value::Nil);
        let conversation_id: Value = body.get("conversation_id").unwrap_or(Value::Nil);
        let message_role: Value = match body.get::<Value>("message").ok() {
            Some(Value::Table(m)) => m.get::<Value>("role").unwrap_or(Value::Nil),
            _ => Value::Nil,
        };
        let message_content: Value = match body.get::<Value>("message").ok() {
            Some(Value::Table(m)) => m.get::<Value>("content").unwrap_or(Value::Nil),
            _ => Value::Nil,
        };
        let log: Table = lua.globals().get("_delivered_log")?;
        let row = lua.create_table()?;
        row.set("peer", lua.create_string(&peer)?)?;
        row.set("kind", lua.create_string(&kind)?)?;
        row.set("chat_id", chat_id)?;
        row.set("conversation_id", conversation_id)?;
        row.set("role", message_role)?;
        row.set("content", message_content)?;
        row.set("body", body)?;
        let n = log.len()?;
        log.set(n + 1, row)?;
        Ok(())
    })?;
    engine_tbl.set("deliver", deliver_fn)?;
    let plugins_fn = lua.create_function(|lua, _: ()| {
        let arr: Table = lua.create_table()?;
        Ok(arr)
    })?;
    engine_tbl.set("plugins", plugins_fn)?;
    nefor.set("engine", engine_tbl)?;

    // plugins.spawn — no-op; tests drive to_plugin directly.
    let plugins_tbl = lua.create_table()?;
    let spawn_fn = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    plugins_tbl.set("spawn", spawn_fn)?;
    nefor.set("plugins", plugins_tbl)?;

    lua.globals().set("nefor", nefor)?;

    // _test surface
    let test_tbl = lua.create_table()?;
    let delivered_fn = lua.create_function(|lua, _: ()| {
        let log: Table = lua.globals().get("_delivered_log")?;
        // Snapshot — wipe the log so subsequent calls in the same test
        // see only new deliveries.
        let snap = lua.create_table()?;
        let len = log.len()?;
        for i in 1..=len {
            let row: Value = log.get(i)?;
            snap.set(i, row)?;
        }
        let fresh = lua.create_table()?;
        lua.globals().set("_delivered_log", fresh)?;
        Ok(snap)
    })?;
    test_tbl.set("delivered", delivered_fn)?;
    let sent_fn = lua.create_function(|lua, _: ()| {
        let log: Table = lua.globals().get("_sent_log")?;
        let snap = lua.create_table()?;
        let len = log.len()?;
        for i in 1..=len {
            let row: Value = log.get(i)?;
            snap.set(i, row)?;
        }
        let fresh = lua.create_table()?;
        lua.globals().set("_sent_log", fresh)?;
        Ok(snap)
    })?;
    test_tbl.set("sent", sent_fn)?;
    lua.globals().set("_test", test_tbl)?;

    Ok(())
}

fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let starter = starter_dir();
    let starter_str = starter.display().to_string();
    let lua_root = lua_dir();
    let lua_root_str = lua_root.display().to_string();
    let plugin_lua = repo_root()
        .join("plugins")
        .join("openai-provider")
        .join("lua");
    let plugin_lua_str = plugin_lua.display().to_string();
    let chatgpt_lua = repo_root()
        .join("plugins")
        .join("chatgpt-provider")
        .join("lua");
    let chatgpt_lua_str = chatgpt_lua.display().to_string();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          "{plugin_lua}/?.lua",
          "{plugin_lua}/?/init.lua",
          "{chatgpt_lua}/?.lua",
          "{chatgpt_lua}/?/init.lua",
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")
        -- examples/nefor-agent/provider.lua reaches the plugin lib via
        -- `require("openai-provider")`. The plugin's `lua/` parent is on
        -- package.path above so that resolves to
        -- plugins/openai-provider/lua/openai-provider/init.lua.
        NEFOR_CONFIG_DIR = "{starter}"
        "#,
        starter = starter_str,
        lua_root = lua_root_str,
        plugin_lua = plugin_lua_str,
        chatgpt_lua = chatgpt_lua_str,
    );
    lua.load(&script).exec()
}

#[test]
fn provider_output_errors_and_capability_loss_release_exact_requests_once() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(r#"
      local spec = require("libs.compositors.provider").spawn_spec("owner", {"/bin/true"}, {
        agentic_loop = {}, conversations = {
          context = function() return {messages = {}, watermark = 1} end,
          watermark = function() return 1 end,
        },
      })
      local function invoke(id)
        spec.to_plugin({{type="event", from="conversation-manager", body={
          kind = "conversation.provider.invoke", approval_policy = "Default", provider="owner", request_id=id,
          conversation_id="conversation", watermark=1,
        }}})
      end
      invoke("rejected-output")
      spec.from_plugin({{type="event", from="owner", body={
        kind="owner.completion.event", request_id="rejected-output", event="error",
        message="output admission failed", error_category="output_admission_failed", effects="unknown",
      }}})
      local sent = _test.sent()
      assert(#sent == 1 and sent[1].body.request_id == "rejected-output")
      assert(sent[1].body.event == "error" and sent[1].body.effects == "unknown")
      assert(next(spec._internals.pending_requests()) == nil)
      spec.from_plugin({{type="event", from="owner", body={
        kind="owner.completion.event", request_id="rejected-output", event="completed",
      }}})
      assert(#_test.sent() == 0, "stale success is inert")
      invoke("none")
      spec.to_plugin({{type="event", from="engine", body={kind="ncp.delivery_failed",
        peer="owner", extent="none", correlations={{kind="owner.completion.request", request_id="none"}},
      }}})
      sent = _test.sent()
      assert(#sent == 1 and sent[1].body.error.outcome == "rejected")
      invoke("unknown")
      spec.to_plugin({{type="event", from="engine", body={kind="ncp.delivery_failed",
        peer="owner", extent="none", correlations={{kind="owner.completion.request", request_id="none"}},
      }}})
      assert(#_test.sent() == 0 and spec._internals.pending_requests().unknown ~= nil,
        "stale rejection must not fail an unrelated request")
      spec.to_plugin({{type="event", from="engine", body={kind="engine.plugin_transport_failed", plugin="owner"}}})
      sent = _test.sent()
      assert(#sent == 1 and sent[1].body.request_id == "unknown")
      assert(sent[1].body.error.outcome == "unknown")
      assert(next(spec._internals.pending_requests()) == nil)
      invoke("later")
      assert(spec._internals.pending_requests().later ~= nil)
    "#).exec().expect("provider owner settlement");
}

#[test]
fn routed_provider_egress_failure_settles_completion_compaction_and_model_locally() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(r#"
      local log, observers, cursor = {}, {}, 0
      nefor.bus.on_event = function(_, cb) observers[#observers + 1] = cb end
      nefor.engine.plugins = function() return {"owner"} end
      local send = nefor.engine.send
      nefor.engine.send = function(payload)
        send(payload)
        log[#log + 1] = {origin="step", payload=payload}
      end
      local actor, ncp = require("core.actor"), require("core.ncp")
      actor.install()
      local spec = require("libs.compositors.provider").spawn_spec("owner", {"/bin/true"}, {
        translator_lib="chatgpt-provider", tool_gate="tool-gate", agentic_loop={}, conversations={
          context=function() return {messages={{role="user", content="context"}}, watermark=1} end,
          watermark=function() return 1 end,
        },
      })
      actor.spawn(spec)
      ncp._test_set_ready("owner", true)
      local function drain()
        while cursor < #log do
          ncp.dispatch(log)
          local stop = #log
          for i=cursor+1,stop do for _,cb in ipairs(observers) do cb(log[i]) end end
          cursor = stop
        end
      end
      local function publish(body)
        nefor.engine.send(nefor.json.encode({type="event", from="consumer", body=body}))
        drain()
      end
      publish {kind = "conversation.provider.invoke", approval_policy = "Default", provider="owner", request_id="completion",
        conversation_id="lead", watermark=1}
      publish {kind="conversation.projection.delta", conversation_id="lead", change={
        kind="context_compaction_pending", provider="owner", model="model", context={messages={{role="user",content="frozen context"}}}, compaction={request_id="compact"}}}
      local deliver = nefor.engine.deliver
      nefor.engine.deliver = function(peer,payload)
        if nefor.json.decode(payload).body.kind == "owner.model.set" then error("egress failed after possible effects") end
        return deliver(peer,payload)
      end
      publish {kind="chat.model.set", provider="owner", model="model"}
      nefor.engine.deliver = deliver
      local counts = {}
      for _,entry in ipairs(log) do
        local body=nefor.json.decode(entry.payload).body
        counts[body.kind]=(counts[body.kind] or 0)+1
        if body.kind=="conversation.provider.event.reported" then
          assert(body.request_id=="completion" and body.event=="error" and body.error.outcome=="unknown")
        elseif body.kind=="conversation.context.compact.failed" then
          assert(body.request_id=="compact" and body.error.outcome=="unknown")
        elseif body.kind=="chat.model.set_failed" then
          assert(body.model=="model" and body.outcome=="unknown")
        end
      end
      assert(counts["ncp.delivery_failed"]>=1)
      assert(counts["conversation.provider.event.reported"]==1)
      assert(counts["conversation.context.compact.failed"]==1)
      assert(counts["chat.model.set_failed"]==1)
      assert(next(spec._internals.pending_requests())==nil)
      for _,row in ipairs(_test.delivered()) do assert(row.kind~="ncp.delivery_failed") end
      local before=#log
      spec.from_plugin({{type="event",from="owner",body={kind="owner.model.set_ack",model="model"}}})
      spec.from_plugin({{type="event",from="owner",body={kind="owner.completion.event",request_id="completion",event="completed"}}})
      spec.from_plugin({{type="event",from="owner",body={kind="owner.chat.compaction.commit",
        chat_id="conversation-compact:compact",model="model",model_context_artifact={}}}})
      assert(#log==before,"late acknowledgments are inert")
      publish {kind="engine.plugin_delivery_lost",plugin="owner"}
      local function count(kind)
        local n=0
        for _,entry in ipairs(log) do if nefor.json.decode(entry.payload).body.kind==kind then n=n+1 end end
        return n
      end
      assert(count("chat.model.set_failed")==1 and count("conversation.context.compact.failed")==1,
        "duplicate scoped loss is inert")
      publish {kind="chat.model.set",provider="owner",model="next"}
      publish {kind="ncp.delivery_failed",peer="owner",extent="none",correlations={{kind="owner.model.set",model="old"}}}
      assert(count("chat.model.set_failed")==1,"stale rejection cannot settle newer model")
      publish {kind="ncp.delivery_failed",peer="owner",extent="none",correlations={{kind="owner.model.set",model="next"}}}
      assert(count("chat.model.set_failed")==2)
      publish {kind="conversation.projection.delta",conversation_id="lead",change={
        kind="context_compaction_pending",provider="owner",model="model",context={messages={{role="user",content="frozen context"}}}, compaction={request_id="scoped"}}}
      publish {kind="engine.plugin_delivery_lost",plugin="owner"}
      assert(count("conversation.context.compact.failed")==2,"surviving queue loss settles compaction")
      publish {kind = "conversation.provider.invoke", approval_policy = "Default",provider="owner",request_id="later",conversation_id="lead",watermark=1}
      assert(spec._internals.pending_requests().later~=nil,"surviving provider accepts later work")
    "#).exec().expect("routed provider local failure observation");
}

#[test]
fn replaceable_compaction_uses_frozen_prefix_and_settles_before_cleanup() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(r#"
      local prefix_seen
      package.preload["alternate-compactor"] = function()
        return {translator=function(name)
          local t = require("openai-provider").translator(name)
          t.compaction_capability = function() return {kind="supported"} end
          t.compact_context = function(input)
            prefix_seen = input.context.messages[1].content
            input.context.messages[1].content = "adapter mutation"
            return {chat_id="temporary:"..input.request.request_id,
              create={kind=name..".chat.create",chat_id="temporary:"..input.request.request_id},
              compact={kind=name..".chat.compact",chat_id="temporary:"..input.request.request_id},messages={}}
          end
          t.compaction_outcome = function(body, request)
            return {kind="completed",request_id=request.request_id,conversation_id=request.conversation_id,
              checkpoint={provider=name,model=request.model,format="alternate.scalar.v1",artifact="opaque scalar"},
              compatibility={provider=name,model=request.model,format="alternate.scalar.v1"}}
          end
          t.checkpoint_compatible = function(cp, provider, model)
            return cp.provider==provider and cp.model==model and cp.format=="alternate.scalar.v1"
          end
          t.cleanup_compaction = function() error("cleanup refused") end
          t.cancel_compaction = function(plan) return {{kind=name..".chat.cancel",chat_id=plan.chat_id}} end
          return t
        end}
      end
      local spec=require("libs.compositors.provider").spawn_spec("alternate",{"/bin/true"},{
        translator_lib="alternate-compactor",agentic_loop={},conversations={
          context=function() error("must not reread mutable head") end}})
      local input={request={request_id="r",conversation_id="lead",provider="alternate",model="m",
        epoch_id="epoch",history_id={1},trigger="self"},context={messages={{role="user",content="frozen prefix"}}}}
      local env={type="event",from="conversation-manager",body={kind="conversation.projection.delta",
        conversation_id="lead",change={kind="context_compaction_pending",input=input}}}
      spec.to_plugin({env,env})
      assert(prefix_seen=="frozen prefix" and input.context.messages[1].content=="frozen prefix")
      assert(#_test.delivered()==2,"duplicate pending must not dispatch")
      local terminal={type="event",from="alternate",body={kind="alternate.chat.compaction.commit",chat_id="temporary:r"}}
      terminal.body.request_id="wrong"
      spec.from_plugin({terminal})
      assert(#_test.sent()==0,"wrong request must not settle")
      terminal.body.request_id="r"
      spec.from_plugin({terminal,terminal})
      local sent=_test.sent()
      assert(#sent==2)
      assert(sent[1].kind=="conversation.context.compact.complete")
      assert(sent[1].body.checkpoint.artifact=="opaque scalar")
      assert(sent[1].body.epoch_id=="epoch" and sent[1].body.history_id[1]==1)
      assert(sent[2].kind=="conversation.context.compact.cleanup_failed")
      assert(sent[2].body.request_id=="r" and sent[2].body.resource_id=="temporary:r")
      spec.to_plugin({env})
      assert(#_test.delivered()==0,"duplicate accepted request after settlement is inert")
    "#).exec().expect("alternate opaque compactor and cleanup settlement");
}

#[test]
fn compaction_cancellation_releases_only_correlated_resource_and_late_terminal_is_inert() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(r#"
      local spec=require("libs.compositors.provider").spawn_spec("chatgpt",{"/bin/true"},{
        translator_lib="chatgpt-provider",tool_gate="gate",agentic_loop={},conversations={context=function() error("no reread") end}})
      local env={type="event",from="conversation-manager",body={kind="conversation.projection.delta",conversation_id="lead",
        change={kind="context_compaction_pending",input={request={request_id="cancel",conversation_id="lead",provider="chatgpt",model="m"},
          context={messages={{role="user",content="prefix"}}}}}}}
      env.replay=true
      spec.to_plugin({env})
      assert(#_test.delivered()==0)
      env.replay=nil
      spec.to_plugin({env})
      assert(#_test.delivered()==3)
      spec.to_plugin({{type="event",from="consumer",body={kind="conversation.context.compact.cancel",
        conversation_id="foreign",request_id="cancel"}}})
      assert(#_test.sent()==0 and #_test.delivered()==0)
      spec.to_plugin({{type="event",from="consumer",body={kind="conversation.context.compact.cancel",
        conversation_id="lead",request_id="cancel"}}})
      local sent=_test.sent(); local delivered=_test.delivered()
      assert(#sent==1 and sent[1].body.error.code=="compaction_interrupted")
      assert(#delivered==2 and delivered[1].kind=="chatgpt.chat.cancel" and delivered[2].kind=="chatgpt.chat.delete")
      spec.from_plugin({{type="event",from="chatgpt",body={kind="chatgpt.chat.compaction.commit",
        chat_id="conversation-compact:cancel",model="m",model_context_artifact={items={{}}}}}})
      assert(#_test.sent()==0)
    "#).exec().expect("correlated cancellation and replay safety");
}

#[test]
fn adapter_preparation_waits_for_observed_regular_capacity_without_polling() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(r#"
      local op=require("libs.compositors.provider")
      local opts={translator_lib="chatgpt-provider",tool_gate="gate",agentic_loop={},conversations={context=function() return {} end}}
      local spec=op.spawn_spec("chatgpt",{"/bin/true"},opts)
      local input={provider="chatgpt",model="m",policy={soft_percent=60,step_percent=10,hard_percent=90}}
      local first=spec.compaction.prepare(input)
      assert(first.kind=="Waiting")
      assert(spec.compaction.prepare(input).request_id==first.request_id)
      local delivered=_test.delivered()
      assert(#delivered==1 and delivered[1].kind=="chatgpt.models.list_requested")
      spec.from_plugin({{type="event",from="chatgpt",body={kind="chatgpt.models.listed",models={"m","bad"},
        context_windows={m=123456,bad=0},max_context_windows={bad=999999,m=999999}}}})
      local sent=_test.sent();local prepared
      for _,e in ipairs(sent) do if e.kind=="conversation.compaction.prepared" then prepared=e.body end end
      assert(prepared.request_id==first.request_id and prepared.outcome.kind=="Ready")
      assert(prepared.outcome.regular_window==123456 and prepared.outcome.capability.kind=="supported")
      assert(spec.compaction.prepare(input).regular_window==123456)
      input.model="bad"
      assert(spec.compaction.prepare(input).error.code=="compaction_capacity_missing")
      assert(#_test.delivered()==0)
      local unsupported=op.spawn_spec("openai",{"/bin/true"},{agentic_loop={},conversations={context=function() return {} end}})
      input.provider="openai"
      assert(unsupported.compaction.prepare(input).error.code=="compaction_unsupported")
      input.policy=nil
      assert(unsupported.compaction.prepare(input).capability.kind=="unsupported")
      assert(#_test.delivered()==0)
    "#).exec().expect("metadata preparation without guessed capacity or polling");
}
