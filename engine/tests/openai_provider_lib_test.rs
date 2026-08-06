//! Unit tests for the plugin lib at
//! `plugins/openai-provider/lua/openai-provider/init.lua`.
//!
//! The lib exposes pure provider-boundary translation primitives.
//!
//! Coverage:
//!
//! - translator.outbound — every kind rename direction; nil drops for
//!   ready/goodbye/hello-without-model; provider tagging on auth.status,
//!   models.listed, model.set_ack; turn.error → chat.message.append
//!   (interrupted vs error); chat.complete.result + chat.error
//!   pass-through (lib leaves the prefixed kind alone so consumers can
//!   do the agentic-loop coupling themselves).
//! - translator.inbound — chat.input.submit / chat.interrupt_all /
//!   chat.compaction.request drop;
//!   canonical → prefixed renames; provider-target filter on
//!   chat.auth.set, chat.login_requested, chat.logout_requested,
//!   chat.model.list_requested, chat.model.set; env.from == name
//!   self-echo drop.
//! - translator.maybe_inject_static_token — fires once on the first
//!   ready when opts.static_token is set; idempotent thereafter; no-op
//!   when token absent or kind isn't ready.

use std::path::PathBuf;

use mlua::{Function, Lua, Table, Value};

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

fn lua_dir() -> PathBuf {
    repo_root().join("lua")
}

fn plugin_lua_dir() -> PathBuf {
    repo_root().join("plugins/openai-provider/lua")
}

fn chatgpt_plugin_lua_dir() -> PathBuf {
    repo_root().join("plugins/chatgpt-provider/lua")
}

// ---------------------------------------------------------------------
// Harness: minimal `nefor.*` surface + package.path covering the plugin
// lib's parent dir and `core` / `libs`.
// ---------------------------------------------------------------------

fn lua_with_lib() -> Lua {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");
    lua.load(
        r#"
        provider = require("openai-provider")
        provider._reset()
        "#,
    )
    .exec()
    .expect("require lib");
    lua
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

    // bus.on_event — no-op (no consumers of the bus in these tests).
    let bus_tbl = lua.create_table()?;
    bus_tbl.set("on_event", no_op.clone())?;
    nefor.set("bus", bus_tbl)?;

    // engine — record deliver / send in _delivered / _sent globals.
    let engine_tbl = lua.create_table()?;
    let delivered_log = lua.create_table()?;
    lua.globals().set("_delivered_log", delivered_log)?;
    let sent_log = lua.create_table()?;
    lua.globals().set("_sent_log", sent_log)?;

    let deliver_fn = lua.create_function(|lua, args: mlua::Variadic<Value>| {
        let peer = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            _ => return Ok(()),
        };
        let payload = match args.get(1) {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            _ => return Ok(()),
        };
        let json: Table = lua.globals().get::<Table>("nefor")?.get::<Table>("json")?;
        let decode: Function = json.get("decode")?;
        let decoded: Value = decode.call(lua.create_string(&payload)?)?;
        let body = match decoded {
            Value::Table(t) => t.get::<Value>("body")?,
            _ => Value::Nil,
        };
        let log: Table = lua.globals().get("_delivered_log")?;
        let row = lua.create_table()?;
        row.set("peer", lua.create_string(&peer)?)?;
        row.set("body", body)?;
        let n = log.len()?;
        log.set(n + 1, row)?;
        Ok(())
    })?;
    engine_tbl.set("deliver", deliver_fn)?;

    let send_fn = lua.create_function(|lua, args: mlua::Variadic<Value>| {
        let payload = match args.first() {
            Some(Value::String(s)) => s.to_str()?.to_owned(),
            _ => return Ok(()),
        };
        let json: Table = lua.globals().get::<Table>("nefor")?.get::<Table>("json")?;
        let decode: Function = json.get("decode")?;
        let decoded: Value = decode.call(lua.create_string(&payload)?)?;
        let env = match decoded {
            Value::Table(t) => t,
            _ => return Ok(()),
        };
        let log: Table = lua.globals().get("_sent_log")?;
        let row = lua.create_table()?;
        row.set("from", env.get::<Value>("from")?)?;
        row.set("body", env.get::<Value>("body")?)?;
        let n = log.len()?;
        log.set(n + 1, row)?;
        Ok(())
    })?;
    engine_tbl.set("send", send_fn)?;

    let now_fn = lua.create_function(|_, _: ()| Ok("2026-05-12T00:00:00.000Z".to_owned()))?;
    engine_tbl.set("now", now_fn)?;

    let plugins_fn = lua.create_function(|lua, _: ()| {
        let arr: Table = lua.create_table()?;
        Ok(arr)
    })?;
    engine_tbl.set("plugins", plugins_fn)?;
    nefor.set("engine", engine_tbl)?;

    lua.globals().set("nefor", nefor)?;
    Ok(())
}

fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let plugin = plugin_lua_dir();
    let chatgpt_plugin = chatgpt_plugin_lua_dir();
    let core = lua_dir();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{plugin}/?.lua",
          "{plugin}/?/init.lua",
          "{chatgpt_plugin}/?.lua",
          "{chatgpt_plugin}/?/init.lua",
          "{core}/?.lua",
          "{core}/?/init.lua",
          package.path,
        }}, ";")
        "#,
        plugin = plugin.display(),
        chatgpt_plugin = chatgpt_plugin.display(),
        core = core.display(),
    );
    lua.load(&script).exec()
}

// ---------------------------------------------------------------------
// outbound — kind renames
// ---------------------------------------------------------------------

#[test]
fn outbound_renames_stream_delta() {
    let lua = lua_with_lib();
    let kind: String = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local b = t.outbound({
                type = "event", from = "ollama",
                body = { kind = "ollama.stream.delta", chat_id = "c1", text = "hi" },
            })
            return b.kind
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(kind, "chat.stream.delta");
}

#[test]
fn outbound_renames_stream_end_drops_finish_reason() {
    let lua = lua_with_lib();
    let (kind, finish): (String, Value) = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local b = t.outbound({
                type = "event", from = "ollama",
                body = { kind = "ollama.stream.end", chat_id = "c1", finish_reason = "stop" },
            })
            return b.kind, b.finish_reason
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(kind, "chat.stream.end");
    assert!(
        matches!(finish, Value::Nil),
        "finish_reason must be cleared"
    );
}

#[test]
fn outbound_tags_auth_status_with_provider() {
    let lua = lua_with_lib();
    let (kind, provider_name): (String, String) = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local b = t.outbound({
                type = "event", from = "ollama",
                body = { kind = "ollama.auth.status", status = "connected" },
            })
            return b.kind, b.provider
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(kind, "chat.auth.status");
    assert_eq!(provider_name, "ollama");
}

#[test]
fn outbound_turn_error_interrupted_synthesizes_system_message() {
    let lua = lua_with_lib();
    let (kind, role, text): (String, String, String) = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local b = t.outbound({
                type = "event", from = "ollama",
                body = { kind = "ollama.turn.error", message = "interrupted" },
            })
            return b.kind, b.role, b.text
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(kind, "chat.message.append");
    assert_eq!(role, "system");
    assert_eq!(text, "[interrupted]");
}

#[test]
fn outbound_turn_error_other_synthesizes_error_system_message() {
    let lua = lua_with_lib();
    let text: String = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local b = t.outbound({
                type = "event", from = "ollama",
                body = { kind = "ollama.turn.error", message = "boom" },
            })
            return b.text
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(text, "Error: boom");
}

#[test]
fn outbound_hello_with_model_synthesizes_model_set_ack() {
    let lua = lua_with_lib();
    let (kind, provider_name, model): (String, String, String) = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local b = t.outbound({
                type = "event", from = "ollama",
                body = { kind = "ollama.hello", model = "qwen3" },
            })
            return b.kind, b.provider, b.model
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(kind, "chat.model.set_ack");
    assert_eq!(provider_name, "ollama");
    assert_eq!(model, "qwen3");
}

#[test]
fn outbound_hello_without_model_drops() {
    let lua = lua_with_lib();
    let v: Value = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            return t.outbound({
                type = "event", from = "ollama",
                body = { kind = "ollama.hello" },
            })
            "#,
        )
        .eval()
        .expect("eval");
    assert!(matches!(v, Value::Nil));
}

#[test]
fn outbound_ready_and_goodbye_drop() {
    let lua = lua_with_lib();
    let (ready, goodbye): (Value, Value) = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local r = t.outbound({
                type = "event", from = "ollama",
                body = { kind = "ollama.ready" },
            })
            local g = t.outbound({
                type = "event", from = "ollama",
                body = { kind = "ollama.goodbye" },
            })
            return r, g
            "#,
        )
        .eval()
        .expect("eval");
    assert!(matches!(ready, Value::Nil));
    assert!(matches!(goodbye, Value::Nil));
}

#[test]
fn outbound_passes_through_chat_complete_result_kind_unchanged() {
    // The lib doesn't touch chat.complete.result — the consumer does
    // the agentic-loop coupling.
    let lua = lua_with_lib();
    let kind: String = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local b = t.outbound({
                type = "event", from = "ollama",
                body = {
                    kind = "ollama.chat.complete.result",
                    chat_id = "c1",
                    output = { text = "hi", finish_reason = "stop" },
                },
            })
            return b.kind
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(kind, "ollama.chat.complete.result");
}

#[test]
fn outbound_passes_through_chat_error_kind_unchanged() {
    let lua = lua_with_lib();
    let kind: String = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local b = t.outbound({
                type = "event", from = "ollama",
                body = { kind = "ollama.chat.error", chat_id = "c1", message = "boom" },
            })
            return b.kind
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(kind, "ollama.chat.error");
}

#[test]
fn outbound_does_not_mutate_caller_body() {
    let lua = lua_with_lib();
    let (orig_kind, returned_kind): (String, String) = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local src = { kind = "ollama.stream.delta", chat_id = "c1" }
            local b = t.outbound({ type = "event", from = "ollama", body = src })
            return src.kind, b.kind
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(
        orig_kind, "ollama.stream.delta",
        "caller body must not mutate"
    );
    assert_eq!(returned_kind, "chat.stream.delta");
}

// ---------------------------------------------------------------------
// inbound — drops + renames
// ---------------------------------------------------------------------

#[test]
fn inbound_drops_chat_input_submit() {
    let lua = lua_with_lib();
    let v: Value = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            return t.inbound({
                type = "event", from = "tui",
                body = { kind = "chat.input.submit", text = "hi" },
            })
            "#,
        )
        .eval()
        .expect("eval");
    assert!(matches!(v, Value::Nil));
}

#[test]
fn inbound_drops_canonical_compaction_request() {
    let lua = lua_with_lib();
    let v: Value = lua
        .load(
            r#"
            local t = provider.translator("chatgpt")
            return t.inbound({
                type = "event", from = "nefor-tui",
                body = {
                    kind = "chat.compaction.request",
                    provider = "chatgpt",
                    trigger = "manual",
                },
            })
            "#,
        )
        .eval()
        .expect("eval");
    assert!(
        matches!(v, Value::Nil),
        "agentic-loop owns canonical history materialization and sends the prefixed chat.compact"
    );
}

#[test]
fn inbound_drops_chat_interrupt_all() {
    let lua = lua_with_lib();
    let v: Value = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            return t.inbound({
                type = "event", from = "tui",
                body = { kind = "chat.interrupt_all" },
            })
            "#,
        )
        .eval()
        .expect("eval");
    assert!(matches!(v, Value::Nil));
}

#[test]
fn inbound_renames_chat_interrupt() {
    let lua = lua_with_lib();
    let kind: String = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local b = t.inbound({
                type = "event", from = "tui",
                body = { kind = "chat.interrupt", chat_id = "c1" },
            })
            return b.kind
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(kind, "ollama.interrupt");
}

#[test]
fn inbound_chat_auth_set_filters_by_provider() {
    let lua = lua_with_lib();
    // For matching provider, returns the prefixed body.
    let (matched_kind, matched_token): (String, String) = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local b = t.inbound({
                type = "event", from = "tui",
                body = { kind = "chat.auth.set", provider = "ollama", token = "tok-1" },
            })
            return b.kind, b.token
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(matched_kind, "ollama.auth.set");
    assert_eq!(matched_token, "tok-1");

    // For non-matching provider, returns nil.
    let unmatched: Value = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            return t.inbound({
                type = "event", from = "tui",
                body = { kind = "chat.auth.set", provider = "openai", token = "tok-2" },
            })
            "#,
        )
        .eval()
        .expect("eval");
    assert!(
        matches!(unmatched, Value::Nil),
        "non-matching provider drops"
    );
}

#[test]
fn inbound_chat_model_set_returns_bare_body() {
    // Lib returns body WITHOUT chat_id — orchestrator state lives in
    // the consumer.
    let lua = lua_with_lib();
    let (kind, model, chat_id): (String, String, Value) = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local b = t.inbound({
                type = "event", from = "tui",
                body = { kind = "chat.model.set", provider = "ollama", model = "qwen3" },
            })
            return b.kind, b.model, b.chat_id
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(kind, "ollama.model.set");
    assert_eq!(model, "qwen3");
    assert!(
        matches!(chat_id, Value::Nil),
        "chat_id must NOT be set by the lib"
    );
}

#[test]
fn inbound_self_from_drops() {
    let lua = lua_with_lib();
    let v: Value = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            return t.inbound({
                type = "event", from = "ollama",
                body = { kind = "ollama.chat.create", chat_id = "c1" },
            })
            "#,
        )
        .eval()
        .expect("eval");
    assert!(
        matches!(v, Value::Nil),
        "envelopes the lib itself published must not echo back"
    );
}

// ---------------------------------------------------------------------
// maybe_inject_static_token
// ---------------------------------------------------------------------

#[test]
fn static_token_injection_fires_once_on_ready() {
    let lua = lua_with_lib();
    let (first, second, peer, kind, token): (bool, bool, String, String, String) = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local first = t.maybe_inject_static_token(
                { type = "event", from = "ollama", body = { kind = "ollama.ready" } },
                { static_token = "ollama-local" }
            )
            local second = t.maybe_inject_static_token(
                { type = "event", from = "ollama", body = { kind = "ollama.ready" } },
                { static_token = "ollama-local" }
            )
            local row = _delivered_log[1]
            return first, second, row.peer, row.body.kind, row.body.token
            "#,
        )
        .eval()
        .expect("eval");
    assert!(first, "first ready must inject");
    assert!(!second, "second ready must be a no-op (idempotent)");
    assert_eq!(peer, "ollama");
    assert_eq!(kind, "ollama.auth.set");
    assert_eq!(token, "ollama-local");
}

#[test]
fn static_token_no_op_when_token_absent() {
    let lua = lua_with_lib();
    let (fired, delivered): (bool, i64) = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local fired = t.maybe_inject_static_token(
                { type = "event", from = "ollama", body = { kind = "ollama.ready" } },
                {}
            )
            return fired, #_delivered_log
            "#,
        )
        .eval()
        .expect("eval");
    assert!(!fired);
    assert_eq!(delivered, 0);
}

#[test]
fn static_token_no_op_when_kind_not_ready() {
    let lua = lua_with_lib();
    let fired: bool = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            return t.maybe_inject_static_token(
                { type = "event", from = "ollama", body = { kind = "ollama.hello" } },
                { static_token = "x" }
            )
            "#,
        )
        .eval()
        .expect("eval");
    assert!(!fired);
}

#[test]
fn chatgpt_usage_outbound_becomes_provider_neutral() {
    let lua = lua_with_lib();
    let (kind, provider_name, used): (String, String, f64) = lua
        .load(
            r#"
            local chatgpt = require("chatgpt-provider")
            local t = chatgpt.translator("chatgpt")
            local b = t.outbound({
                type = "event", from = "chatgpt",
                body = {
                    kind = "chatgpt.usage.updated",
                    rate_limit = { primary_window = { used_percent = 66 } },
                },
            })
            return b.kind, b.provider, b.rate_limit.primary_window.used_percent
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(kind, "chat.usage.updated");
    assert_eq!(provider_name, "chatgpt");
    assert_eq!(used, 66.0);
}

#[test]
fn chatgpt_usage_request_filters_by_provider() {
    let lua = lua_with_lib();
    let (accepted_kind, rejected): (String, Value) = lua
        .load(
            r#"
            local chatgpt = require("chatgpt-provider")
            local t = chatgpt.translator("chatgpt")
            local accepted = t.inbound({
                type = "event", from = "engine",
                body = { kind = "chat.usage.requested", provider = "chatgpt" },
            })
            local rejected = t.inbound({
                type = "event", from = "engine",
                body = { kind = "chat.usage.requested", provider = "other" },
            })
            return accepted.kind, rejected
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(accepted_kind, "chatgpt.usage.requested");
    assert!(matches!(rejected, Value::Nil));
}

#[test]
fn universal_tool_calls_are_lowered_only_at_provider_boundary() {
    let lua = lua_with_lib();
    let (name, arguments): (String, String) = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local message = t.context_message({
                role = "assistant",
                content = "",
                tool_calls = {{ id = "call-1", name = "read_file", arguments = { path = "x" } }},
            })
            return message.tool_calls[1]["function"].name,
                   message.tool_calls[1]["function"].arguments
            "#,
        )
        .eval()
        .expect("lower universal tool call");
    assert_eq!(name, "read_file");
    assert_eq!(arguments, r#"{"path":"x"}"#);
}

#[test]
fn chatgpt_direct_completion_lowers_manager_history_without_dropping_run_input() {
    let lua = lua_with_lib();
    let (kind, input, tail_input, name, arguments): (String, String, String, String, String) = lua
        .load(
            r#"
            local chatgpt = require("chatgpt-provider")
            local t = chatgpt.translator("chatgpt")
            local request = t.inbound({
                type = "event", from = "mag",
                body = {
                    kind = "chatgpt.completion.request",
                    request_id = "request-1",
                    messages = {
                        { role = "user", content = "earlier input" },
                        {
                            role = "assistant", content = "",
                            tool_calls = {{
                                id = "call-1", name = "read_file",
                                arguments = { path = "x" }, status = "call_completed",
                            }},
                        },
                        { role = "user", content = "current input" },
                    },
                    conversation_context = {
                        messages = {
                            { role = "user", content = "earlier input" },
                            {
                                role = "assistant", content = "",
                                tool_calls = {{
                                    id = "call-1", name = "read_file",
                                    arguments = { path = "x" }, status = "call_completed",
                                }},
                            },
                        },
                        tail_messages = {},
                    },
                },
            })
            local call = request.conversation_context.messages[2].tool_calls[1]
            return request.kind, request.conversation_context.messages[3].content,
                   request.conversation_context.tail_messages[1].content,
                   call["function"].name, call["function"].arguments
            "#,
        )
        .eval()
        .expect("lower direct ChatGPT completion request");
    assert_eq!(kind, "chatgpt.completion.request");
    assert_eq!(input, "current input");
    assert_eq!(tail_input, "current input");
    assert_eq!(name, "read_file");
    assert_eq!(arguments, r#"{"path":"x"}"#);
}

#[test]
fn chatgpt_compaction_plan_restores_only_its_opaque_checkpoint() {
    let lua = lua_with_lib();
    let (restored, count, first_role): (bool, i64, String) = lua
        .load(
            r#"
            local chatgpt = require("chatgpt-provider")
            local t = chatgpt.translator("chatgpt")
            local plan = assert(t.compact_context({
                compaction = { request_id = "compact-1" },
                context = {
                    messages = {{ role = "user", content = "old" }},
                    tail_messages = {{ role = "user", content = "new" }},
                    compaction = { checkpoint = {
                        provider = "chatgpt",
                        format = "chatgpt.responses.compaction.v1",
                        artifact = { items = {{ type = "compaction", encrypted_content = "sealed" }} },
                    }},
                },
            }))
            return plan.restore ~= nil, #plan.messages, plan.messages[1].role
            "#,
        )
        .eval()
        .expect("plan compaction");
    assert!(restored);
    assert_eq!(count, 1);
    assert_eq!(first_role, "user");
}

#[test]
fn chatgpt_compaction_plan_falls_back_to_full_context_for_foreign_checkpoint() {
    let lua = lua_with_lib();
    let (restored, content): (bool, String) = lua
        .load(
            r#"
            local chatgpt = require("chatgpt-provider")
            local t = chatgpt.translator("chatgpt")
            local plan = assert(t.compact_context({
                compaction = { request_id = "compact-1" },
                context = {
                    messages = {{ role = "user", content = "full" }},
                    tail_messages = {{ role = "user", content = "tail" }},
                    compaction = { checkpoint = {
                        provider = "another-provider",
                        format = "private.v1",
                        artifact = { items = {} },
                    }},
                },
            }))
            return plan.restore ~= nil, plan.messages[1].content
            "#,
        )
        .eval()
        .expect("fallback compaction plan");
    assert!(!restored);
    assert_eq!(content, "full");
}
