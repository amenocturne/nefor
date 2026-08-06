//! Provider compositor contracts driven through the production Lua callbacks.

use std::path::PathBuf;

use mlua::{Function, Lua, Table, Value};

fn starter_dir() -> PathBuf {
    repo_root().join("starter")
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
fn chatgpt_direct_terminals_keep_provider_output_on_canonical_completion_events() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    let root = repo_root();
    let openai_lua = root.join("plugins/openai-provider/lua");
    let chatgpt_lua = root.join("plugins/chatgpt-provider/lua");
    lua.load(format!(
        r#"
        package.path = table.concat({{
          "{chatgpt}/?.lua", "{chatgpt}/?/init.lua",
          "{openai}/?.lua", "{openai}/?/init.lua", package.path,
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
    ))
    .exec()
    .expect("translate ChatGPT terminals");
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
          agentic_loop = {}, translator_lib = "chatgpt-provider",
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
              provider = "chatgpt",
              compaction = { request_id = "compact-1" },
            },
          },
        }})
        local delivered = _test.delivered()
        assert(#delivered == 3)
        assert(delivered[1].kind == "chatgpt.chat.create")
        assert(delivered[1].conversation_id == "lead")
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
              compaction = { request_id = "compact-2" },
            },
          },
        }})
        local sent = _test.sent()
        assert(#sent == 1)
        assert(sent[1].kind == "conversation.context.compact.failed")
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
        local spec = op.spawn_spec("ollama", { "/bin/true" }, {
          agentic_loop = {},
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
            kind = "conversation.provider.invoke", provider = "ollama",
            request_id = "request-1", conversation_id = "conversation-1",
            watermark = 11, model = "qwen", tools = { "read_file" },
          },
        }})

        local delivered = _test.delivered()
        assert(#delivered == 1)
        assert(delivered[1].kind == "ollama.completion.request")
        assert(delivered[1].body.request_id == "request-1")
        assert(delivered[1].body.model == "qwen")
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
            kind = "conversation.provider.invoke", provider = "ollama",
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
    // (now wired explicitly by starter/init.lua via
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
        -- starter/provider.lua reaches the plugin lib via
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
