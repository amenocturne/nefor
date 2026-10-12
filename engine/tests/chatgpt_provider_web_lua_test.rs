//! Focused contract tests for the ChatGPT routed-web Lua adapter.

use std::path::PathBuf;

use mlua::{Lua, Table};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

fn lua_with_adapter() -> Lua {
    let lua = Lua::new();
    let nefor = lua.create_table().expect("nefor table");
    nefor::lua::bindings::install_json(&lua, &nefor).expect("install json binding");
    lua.globals().set("nefor", nefor).expect("set nefor");
    // Source-emitted results are recorded so tests can read what reached the bus.
    lua.load(
        r#"
        _G.emitted = {}
        local function no_op() end
        nefor.log = { info = no_op, warn = no_op, error = no_op, debug = no_op }
        nefor.engine = {
          now = function() return 0 end,
          send = function(payload)
            local decoded = nefor.json.decode(payload)
            emitted[#emitted + 1] = decoded
          end,
          deliver = function() end,
        }
        function _G.last_result(id)
          for index = #emitted, 1, -1 do
            local envelope = emitted[index]
            if envelope.body.kind == "tool.result" and envelope.body.id == id then
              return envelope.body, envelope.from
            end
          end
        end
        "#,
    )
    .exec()
    .expect("install engine recorder");

    let root = repo_root();
    let shared = root.join("lua");
    let openai = root.join("plugins/openai-provider/lua");
    let chatgpt = root.join("plugins/chatgpt-provider/lua");
    let tool_gate = root.join("plugins/tool-gate/lua");
    lua.load(format!(
        r#"
        package.path = table.concat({{
          "{chatgpt}/?.lua", "{chatgpt}/?/init.lua",
          "{openai}/?.lua", "{openai}/?/init.lua",
          "{tool_gate}/?.lua", "{tool_gate}/?/init.lua",
          "{shared}/?.lua", "{shared}/?/init.lua",
          package.path,
        }}, ";")
        "#,
        chatgpt = chatgpt.display(),
        openai = openai.display(),
        tool_gate = tool_gate.display(),
        shared = shared.display(),
    ))
    .exec()
    .expect("set package.path");
    lua
}

#[test]
fn advertises_six_routed_read_tools_with_closed_schemas_and_content_results() {
    let lua = lua_with_adapter();
    lua.load(
        r#"
        local t = require("chatgpt-provider").translator("chatgpt", { tool_gate = "tool-gate" })
        local tools = t.tool_source.advertisement("tool-gate").tools
        local display = require("libs.chat.tool_display")
        assert(#tools == 6)
        local expected = {
          web_search = true, web_open = true, web_click = true,
          web_find = true, web_image_search = true, web_screenshot = true,
        }
        for _, tool in ipairs(tools) do
          assert(expected[tool.name], "unexpected tool " .. tostring(tool.name))
          expected[tool.name] = nil
          assert(tool.access == nil)
          assert(tool.execution.kind == "routed" and tool.execution.provider == nil)
          assert(tool.parameters.type == "object")
          assert(tool.parameters.additionalProperties == false)
          assert(type(tool.parameters.required) == "table")
          assert(tool.display.result.kind == "content")
          assert(tool.display.result.text == nil)
          assert(tool.display.lifecycle == "delayed")
          assert(tool.description:find("use regular Markdown links with URLs returned by these tools", 1, true))
          assert(tool.description:find("Raw OpenAI citation markers are not rendered in this interface", 1, true))
          assert(tool.description:find("do not use them as citations", 1, true))
          assert(tool.description:find("Keep opaque source IDs for tool navigation", 1, true))
          assert(display.validate(tool.display))
        end
        assert(next(expected) == nil)
        local projected = {}
        for _, tool in ipairs(tools) do
          projected[tool.name] = assert(display.project(tool.display, ({
            web_search = { query = "lua" },
            web_open = { url = "turn0search0" },
            web_click = { url = "turn0open0", link = 4 },
            web_find = { url = "turn0open0", pattern = "needle" },
            web_image_search = { query = "moon" },
            web_screenshot = { url = "turn0pdf0", page = 0 },
          })[tool.name], nil, false))
        end
        assert(projected.web_search.label == "web search" and projected.web_search.primary == "lua")
        assert(projected.web_open.label == "web open page" and projected.web_open.primary == "turn0search0")
        assert(projected.web_click.label == "web click" and projected.web_click.primary == "turn0open0 link 4")
        assert(projected.web_find.label == "web find" and projected.web_find.primary == "“needle” in turn0open0")
        assert(projected.web_image_search.label == "web image search" and projected.web_image_search.primary == "moon")
        assert(projected.web_screenshot.label == "web screenshot" and projected.web_screenshot.primary == "turn0pdf0 page 0")
        assert(tools[6].parameters.properties.page.description:find("Zero%-indexed"))
        assert(tools[6].description:find("web_open", 1, true))
        assert(tools[6].description:find("provider-issued PDF reference", 1, true))
        assert(tools[6].display.expanded.fields[2].label == "page (zero-indexed)")
        "#,
    )
    .exec()
    .expect("validate routed descriptors");
}

#[test]
fn maps_each_tool_to_exactly_one_private_command_family() {
    let lua = lua_with_adapter();
    lua.load(
        r#"
        local t = require("chatgpt-provider").translator("chatgpt", { tool_gate = "tool-gate" })
        local cases = {
          { "web_search", { query = "lua", recency_days = 7, domains = nefor.json.decode('["lua.org"]') },
            "search_query", { q = "lua", recency = 7 } },
          { "web_image_search", { query = "moon" }, "image_query", { q = "moon" } },
          { "web_open", { url = "turn0search0", line = 12 }, "open", { ref_id = "turn0search0", lineno = 12 } },
          { "web_click", { url = "turn0open0", link = 4 }, "click", { ref_id = "turn0open0", id = 4 } },
          { "web_find", { url = "turn0open0", pattern = "needle" }, "find", { ref_id = "turn0open0", pattern = "needle" } },
          { "web_screenshot", { url = "turn0pdf0", page = 0 }, "screenshot", { ref_id = "turn0pdf0", pageno = 0 } },
        }
        for index, case in ipairs(cases) do
          local request = assert(t.inbound({ type = "event", from = "tool-gate", body = {
            kind = "chatgpt.tool.invoke", id = "gate-" .. index,
            caller_id = "r1/cap-" .. index, from = "worker.run-tool",
            name = case[1], args = case[2],
            model = "gpt-test", invocation = {
              provider = "chatgpt", model = "gpt-test", session_id = "session-1",
              run_id = "run-1", run_scope = "r1", actor_id = "worker.run-tool",
              capability_id = "r1/cap-" .. index, principal = "lead", future_provenance = { version = 2 },
              conversation_id = "actor-conversation", root_conversation_id = "lead",
            },
          }}))
          assert(request.kind == "chatgpt.web.request")
          assert(request.id == "gate-" .. index and request.caller_id == "r1/cap-" .. index)
          assert(request.invoking_from == "worker.run-tool")
          assert(request.model == "gpt-test")
          assert(request.scope.conversation_id == "actor-conversation")
          assert(request.scope.root_conversation_id == "lead")
          assert(request.scope.run_scope == "r1" and request.scope.session_id == "session-1")
          assert(request.invocation.conversation_id == "actor-conversation")
          assert(request.invocation.root_conversation_id == "lead")
          assert(request.invocation.session_id == "session-1" and request.invocation.run_id == "run-1")
          assert(request.invocation.run_scope == "r1" and request.invocation.actor_id == "worker.run-tool")
          assert(request.invocation.capability_id == "r1/cap-" .. index and request.invocation.principal == "lead")
          assert(request.invocation.future_provenance.version == 2)
          assert(request.invocation.provider == "chatgpt" and request.invocation.model == "gpt-test")
          assert(request.settings.external_web_access == true)
          assert(#request.settings.allowed_callers == 1 and request.settings.allowed_callers[1] == "direct")
          local command_count, command
          command_count = 0
          for family, operations in pairs(request.commands) do
            command_count = command_count + 1
            assert(family == case[3] and #operations == 1)
            command = operations[1]
          end
          assert(command_count == 1)
          for key, value in pairs(case[4]) do assert(command[key] == value) end
          if case[3] == "search_query" then
            assert(command.domains[1] == "lua.org")
          end
          if case[3] == "image_query" then assert(request.commands.search_query == nil) end
        end
        assert(t.inbound({ type = "event", from = "attacker", body = {
          kind = "chatgpt.tool.invoke", id = "gate-forged", caller_id = "r1/cap-forged",
          from = "worker.run-tool", name = "web_search", args = { query = "blocked" },
          model = "gpt-test", invocation = {
            provider = "chatgpt", model = "gpt-test", session_id = "session-1",
            run_id = "run-1", run_scope = "r1", actor_id = "worker.run-tool",
            capability_id = "r1/cap-forged", conversation_id = "actor-conversation",
          },
        }}) == nil)
        "#,
    )
    .exec()
    .expect("map web commands");
}

#[test]
fn malformed_arguments_settle_uniformly_without_reaching_the_provider_process() {
    let lua = lua_with_adapter();
    lua.load(
        r#"
        local t = require("chatgpt-provider").translator("chatgpt", { tool_gate = "tool-gate" })
        local invalid = {
          { "web_search", {}, "web_search: missing required argument `query`" },
          { "web_search", { query = "ok", extra = true },
            "web_search: unknown argument `extra`; accepted arguments: query, domains, recency_days" },
          { "web_open", { url = "x", foo = 1 },
            "web_open: unknown argument `foo`; accepted arguments: url, line" },
          { "web_open", { url = "x", line = -1 }, "web_open: argument `line` must be at least 0, got -1" },
          { "web_click", { url = "x" }, "web_click: missing required argument `link`" },
          { "web_click", { url = "x", link = 1.5 }, "web_click: argument `link` must be an integer, got number" },
          { "web_find", { url = "x", pattern = "" },
            "web_find: argument `pattern` must be at least 1 character long" },
          { "web_screenshot", { url = "x", page = -1 },
            "web_screenshot: argument `page` must be at least 0, got -1" },
          { "web_search", { query = "ok", domains = { "" } },
            "web_search: argument `domains[0]` must be at least 1 character long" },
        }
        for index, case in ipairs(invalid) do
          local id = "bad-" .. index
          local request = t.inbound({ type = "event", from = "tool-gate", body = {
            kind = "chatgpt.tool.invoke", id = id, name = case[1], args = case[2],
          }})
          assert(request == nil, "invalid arguments never reach the provider process")
          local result, from = last_result(id)
          assert(from == "chatgpt", "result is emitted as the provider source")
          assert(result.error == case[3], "expected " .. case[3] .. ", got " .. tostring(result.error))
        end
        assert(t.inbound({ type = "event", from = "tool-gate", body = {
          kind = "chatgpt.tool.invoke", id = "unknown", name = "weather", args = {},
        }}) == nil)
        assert(last_result("unknown").error == "unknown tool `weather`; available tools: "
          .. "web_search, web_open, web_click, web_find, web_image_search, web_screenshot")
        local wrong_provider = t.inbound({ type = "event", from = "tool-gate", body = {
          kind = "chatgpt.tool.invoke", id = "wrong-provider", name = "web_search", args = { query = "test" },
          invocation = { provider = "openrouter", model = "other-model",
            conversation_id = "conversation" },
        }})
        assert(wrong_provider == nil)
        assert(last_result("wrong-provider").error:find("openrouter", 1, true))
        "#,
    )
    .exec()
    .expect("validate malformed arguments");
}

#[test]
fn cancel_and_result_translation_preserve_correlation_content_and_opaque_state() {
    let lua = lua_with_adapter();
    lua.load(
        r#"
        local t = require("chatgpt-provider").translator("chatgpt", { tool_gate = "tool-gate" })
        local invocation = {
          provider = "chatgpt", model = "gpt-test", session_id = "session-1",
          run_id = "run-1", run_scope = "r1", actor_id = "worker.run-tool",
          capability_id = "r1/cap-9", conversation_id = "actor-conversation",
          root_conversation_id = "lead",
        }
        local request = assert(t.inbound({ type = "event", from = "tool-gate", body = {
          kind = "chatgpt.tool.invoke", id = "gate-9", caller_id = "r1/cap-9",
          from = "worker.run-tool", name = "web_search", args = { query = "lua" },
          model = "gpt-test", invocation = invocation,
        }}))
        assert(request.kind == "chatgpt.web.request")
        assert(t.inbound({ type = "event", from = "other", body = {
          kind = "chatgpt.tool.cancel", id = "gate-9",
        }}) == nil)
        local cancel = assert(t.inbound({ type = "event", from = "tool-gate", body = {
          kind = "chatgpt.tool.cancel", id = "gate-9",
        }}))
        assert(cancel.kind == "chatgpt.web.cancel" and cancel.id == "gate-9")
        assert(cancel.caller_id == "r1/cap-9" and cancel.invoking_from == "worker.run-tool")
        assert(cancel.model == "gpt-test" and cancel.invocation.capability_id == "r1/cap-9")
        assert(t.inbound({ type = "event", from = "tool-gate", body = {
          kind = "chatgpt.tool.cancel", id = "gate-9",
        }}) == nil, "a cancelled invocation is not cancelled twice")

        assert(t.outbound({ type = "event", from = "chatgpt", body = {
          kind = "chatgpt.web.result", id = "gate-9",
          output = { text = "", results = nefor.json.decode('[{"type":"future_result","future":{"x":1}}]') },
          provider_state = { encrypted_output = "opaque" },
        }}) == nil, "web results settle through the tool source")
        local success, from = last_result("gate-9")
        assert(from == "chatgpt")
        assert(success.kind == "tool.result" and success.id == "gate-9")
        assert(success.output.text == "")
        assert(success.output.results[1].type == "future_result")
        assert(success.output.results[1].future.x == 1)
        assert(success.provider_state.encrypted_output == "opaque")

        t.outbound({ type = "event", from = "chatgpt", body = {
          kind = "chatgpt.web.result", id = "gate-10",
          error = "web screenshot failed: provider could not resolve the screenshot call",
          output = {
            text = "Internal Error ()\nciteturn11view0 Unable to resolve screenshot call: exact provider evidence",
            results = nefor.json.decode('[{"type":"text_result","ref_id":"turn11view0","title":"Internal Error","snippet":"Unable to resolve screenshot call: exact provider evidence","unknown":{"x":1}}]'),
          },
          provider_state = { encrypted_output = "opaque-screenshot-state" },
        }})
        local semantic_failure = last_result("gate-10")
        assert(semantic_failure.kind == "tool.result" and semantic_failure.id == "gate-10")
        assert(semantic_failure.error == "web screenshot failed: provider could not resolve the screenshot call")
        assert(semantic_failure.output.text:find("turn11view0", 1, true))
        assert(semantic_failure.output.results[1].ref_id == "turn11view0")
        assert(semantic_failure.output.results[1].unknown.x == 1)
        assert(semantic_failure.provider_state.encrypted_output == "opaque-screenshot-state")

        t.outbound({ type = "event", from = "chatgpt", body = {
          kind = "chatgpt.web.result", id = "gate-11", error = "HTTP 403",
        }})
        local transport_failure = last_result("gate-11")
        t.outbound({ type = "event", from = "chatgpt", body = {
          kind = "chatgpt.web.result", id = "gate-12",
        }})
        assert(last_result("gate-12").error == "web result missing output")
        assert(transport_failure.kind == "tool.result" and transport_failure.id == "gate-11")
        assert(transport_failure.error == "HTTP 403" and transport_failure.output == nil)
        "#,
    )
    .exec()
    .expect("translate cancel and results");
}

#[test]
fn large_web_result_uses_generic_gate_persistence_with_retrievable_full_payload() {
    let temp = tempfile::tempdir().expect("tempdir");
    let lua = lua_with_adapter();
    let nefor: Table = lua.globals().get("nefor").expect("nefor table");
    let fs = lua.create_table().expect("fs table");
    let data_root = temp.path().display().to_string();
    fs.set(
        "data_root",
        lua.create_function(move |_, ()| Ok(data_root.clone()))
            .expect("data_root function"),
    )
    .expect("set data_root");
    fs.set(
        "mkdir_p",
        lua.create_function(|_, path: String| {
            std::fs::create_dir_all(path).map_err(mlua::Error::external)?;
            Ok(true)
        })
        .expect("mkdir_p function"),
    )
    .expect("set mkdir_p");
    nefor.set("fs", fs).expect("set fs");

    let rewritten: Table = lua
        .load(
            r#"
            local t = require("chatgpt-provider").translator("chatgpt")
            t.outbound({ type = "event", from = "chatgpt", body = {
              kind = "chatgpt.web.result", id = "gate-large",
              output = { text = "exact text", results = {
                { type = "future_result", opaque = string.rep("Z", 60000) },
              } },
            }})
            local result = assert(last_result("gate-large"))
            result.name = "web_search"
            local persisted, path = require("tool-gate").maybe_dump_output(result, "conversation")
            return { summary = persisted.output, output_path = persisted.output_path, path = path }
            "#,
        )
        .eval()
        .expect("persist web output");
    let path: String = rewritten.get("path").expect("dump path");
    let output_path: String = rewritten.get("output_path").expect("output path");
    let summary: String = rewritten.get("summary").expect("summary");
    assert_eq!(path, output_path);
    assert!(summary.contains("Output written to"));
    assert!(summary.contains(&path));

    let full = std::fs::read_to_string(&path).expect("read full persisted output");
    let decoded: serde_json::Value = serde_json::from_str(&full).expect("persisted JSON");
    assert_eq!(decoded["text"], "exact text");
    assert_eq!(
        decoded["results"][0]["opaque"].as_str().map(str::len),
        Some(60_000)
    );
}

#[test]
fn private_request_does_not_invent_missing_model_or_scope() {
    let lua = lua_with_adapter();
    let request: Table = lua
        .load(
            r#"
            local t = require("chatgpt-provider").translator("chatgpt", { tool_gate = "tool-gate" })
            return assert(t.inbound({ type = "event", from = "tool-gate", body = {
              kind = "chatgpt.tool.invoke", id = "gate-1", name = "web_search",
              args = { query = "test" }, invocation = { actor_id = "worker" } } }))
            "#,
        )
        .eval()
        .expect("lower request");
    assert!(request.get::<Option<String>>("model").unwrap().is_none());
    assert!(request.get::<Option<Table>>("scope").unwrap().is_none());
}
