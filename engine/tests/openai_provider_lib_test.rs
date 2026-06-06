//! Unit tests for the plugin lib at
//! `plugins/openai-provider/lua/openai-provider/init.lua`.
//!
//! The lib exposes PURE primitives — no orchestrator coupling, no
//! callbacks. These tests drive `translator(name)` and
//! `replay_rebuild(env, name)` directly and assert on:
//!
//! Coverage:
//!
//! - translator.outbound — every kind rename direction; nil drops for
//!   ready/goodbye/hello-without-model; provider tagging on auth.status,
//!   models.listed, model.set_ack; turn.error → chat.message.append
//!   (interrupted vs error); chat.complete.result + chat.error
//!   pass-through (lib leaves the prefixed kind alone so consumers can
//!   do the agentic-loop coupling themselves).
//! - translator.inbound — chat.input.submit / chat.interrupt_all drop;
//!   canonical → prefixed renames; provider-target filter on
//!   chat.auth.set, chat.login_requested, chat.logout_requested,
//!   chat.model.list_requested, chat.model.set; env.from == name
//!   self-echo drop; live-path chat.create owned-id tracking.
//! - translator.maybe_inject_static_token — fires once on the first
//!   ready when opts.static_token is set; idempotent thereafter; no-op
//!   when token absent or kind isn't ready.
//! - replay_rebuild — chat.create re-feed (+ duplicate skip and
//!   in-process history re-feed skip); chat.append re-feed gated on
//!   ownership; tool.result → synthesized
//!   assistant chat.append (+ drops for non-owned, error-shaped,
//!   empty-content cases).

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
    let core = lua_dir();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{plugin}/?.lua",
          "{plugin}/?/init.lua",
          "{core}/?.lua",
          "{core}/?/init.lua",
          package.path,
        }}, ";")
        "#,
        plugin = plugin.display(),
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

#[test]
fn inbound_live_chat_create_tracks_ownership() {
    // After inbound() observes a live <prefix>.chat.create, the
    // subsequent replay_rebuild for the same chat_id must skip the
    // duplicate and every history re-feed for that chat_id (otherwise
    // replay mutates a live binary chat that already has the history).
    let lua = lua_with_lib();
    let dup_skipped: bool = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            -- Live path observes the create.
            local b = t.inbound({
                type = "event", from = "engine",
                body = { kind = "ollama.chat.create", chat_id = "c1" },
            })
            assert(b ~= nil)
            assert(b.kind == "ollama.chat.create")
            -- Reset the deliver log so the replay path's drop is observable.
            _delivered_log = {}
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = { kind = "ollama.chat.create", chat_id = "c1" },
            }, "ollama")
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = {
                    kind    = "ollama.chat.append",
                    chat_id = "c1",
                    message = { role = "user", content = "hi again" },
                },
            }, "ollama")
            provider.replay_rebuild({
                type = "event", from = "provider-wrapper", replay = true,
                body = {
                    kind   = "tool.result",
                    id     = "f1",
                    result = {
                        text       = "assistant text",
                        next_state = { chat_id = "c1" },
                    },
                },
            }, "ollama")
            provider.replay_rebuild({
                type = "event", from = "ollama", replay = true,
                body = {
                    kind    = "ollama.chat.complete.result",
                    chat_id = "c1",
                    output  = { text = "final text", finish_reason = "stop" },
                },
            }, "ollama")
            return #_delivered_log == 0
            "#,
        )
        .eval()
        .expect("eval");
    assert!(
        dup_skipped,
        "owned chat replay must not re-deliver create, appends, or synthesized assistant turns"
    );
}

#[test]
fn inbound_history_facts_drop_before_binary_delivery() {
    let lua = lua_with_lib();
    let (create_v, message_v): (Value, Value) = lua
        .load(
            r#"
            local t = provider.translator("ollama")
            local create = t.inbound({
                type = "event", from = "engine",
                body = { kind = "chat.history.create", provider = "ollama", chat_id = "c1" },
            })
            local message = t.inbound({
                type = "event", from = "engine",
                body = {
                    kind = "chat.history.message",
                    provider = "ollama",
                    chat_id = "c1",
                    message = { role = "user", content = "hi" },
                },
            })
            return create, message
            "#,
        )
        .eval()
        .expect("eval");
    assert!(matches!(create_v, Value::Nil));
    assert!(matches!(message_v, Value::Nil));
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

// ---------------------------------------------------------------------
// replay_rebuild
// ---------------------------------------------------------------------

#[test]
fn replay_rebuild_folds_history_facts_into_single_restore() {
    let lua = lua_with_lib();
    let (n, kind, chat_id, model, history_len, first_role, second_role): (
        i64,
        String,
        String,
        String,
        i64,
        String,
        String,
    ) = lua
        .load(
            r#"
            provider.replay_rebuild({
                type = "event", from = "sessions", replay = true,
                body = { kind = "sessions.replay.start" },
            }, "ollama")
            provider.replay_rebuild({
                type = "event", from = "ollama", replay = true,
                body = {
                    kind = "chat.history.create",
                    provider = "ollama",
                    chat_id = "c1",
                    model = "qwen3",
                },
            }, "ollama")
            provider.replay_rebuild({
                type = "event", from = "ollama", replay = true,
                body = {
                    kind = "chat.history.message",
                    provider = "ollama",
                    chat_id = "c1",
                    message = { role = "user", content = "hi" },
                },
            }, "ollama")
            provider.replay_rebuild({
                type = "event", from = "ollama", replay = true,
                body = {
                    kind = "chat.history.message",
                    provider = "ollama",
                    chat_id = "c1",
                    message = { role = "assistant", content = "hello" },
                },
            }, "ollama")
            provider.replay_rebuild({
                type = "event", from = "sessions", replay = false,
                body = { kind = "sessions.replay.end" },
            }, "ollama")
            local row = _delivered_log[1]
            return #_delivered_log,
                row.body.kind,
                row.body.chat_id,
                row.body.model,
                #row.body.history,
                row.body.history[1].role,
                row.body.history[2].role
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(n, 1);
    assert_eq!(kind, "ollama.chat.restore");
    assert_eq!(chat_id, "c1");
    assert_eq!(model, "qwen3");
    assert_eq!(history_len, 2);
    assert_eq!(first_role, "user");
    assert_eq!(second_role, "assistant");
}

#[test]
fn replay_rebuild_chat_create_first_seen_delivers() {
    let lua = lua_with_lib();
    let (n, kind, chat_id, peer): (i64, String, String, String) = lua
        .load(
            r#"
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = { kind = "ollama.chat.create", chat_id = "c1", model = "qwen3" },
            }, "ollama")
            local row = _delivered_log[1]
            return #_delivered_log, row.body.kind, row.body.chat_id, row.peer
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(n, 1);
    assert_eq!(kind, "ollama.chat.create");
    assert_eq!(chat_id, "c1");
    assert_eq!(peer, "ollama");
}

#[test]
fn replay_rebuild_chat_append_dropped_for_unowned() {
    let lua = lua_with_lib();
    let n: i64 = lua
        .load(
            r#"
            -- No prior chat.create for c-other → unowned → drop.
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = {
                    kind    = "ollama.chat.append",
                    chat_id = "c-other",
                    message = { role = "user", content = "hi" },
                },
            }, "ollama")
            return #_delivered_log
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(n, 0, "unowned chat.append must drop");
}

#[test]
fn replay_rebuild_chat_append_delivers_for_owned() {
    let lua = lua_with_lib();
    let (n, role): (i64, String) = lua
        .load(
            r#"
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = { kind = "ollama.chat.create", chat_id = "c1" },
            }, "ollama")
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = {
                    kind    = "ollama.chat.append",
                    chat_id = "c1",
                    message = { role = "user", content = "hi" },
                },
            }, "ollama")
            return #_delivered_log, _delivered_log[2].body.message.role
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(n, 2, "create + append both delivered");
    assert_eq!(role, "user");
}

#[test]
fn replay_rebuild_tool_result_synthesizes_assistant_append() {
    let lua = lua_with_lib();
    let (n, kind, role, content): (i64, String, String, String) = lua
        .load(
            r#"
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = { kind = "ollama.chat.create", chat_id = "c1" },
            }, "ollama")
            provider.replay_rebuild({
                type = "event", from = "provider-wrapper", replay = true,
                body = {
                    kind   = "tool.result",
                    id     = "firing-1",
                    result = {
                        text          = "Hello!",
                        finish_reason = "stop",
                        next_state    = { chat_id = "c1" },
                    },
                },
            }, "ollama")
            local row = _delivered_log[2]
            return #_delivered_log, row.body.kind, row.body.message.role, row.body.message.content
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(n, 2, "create + synthesized assistant chat.append");
    assert_eq!(kind, "ollama.chat.append");
    assert_eq!(role, "assistant");
    assert_eq!(content, "Hello!");
}

#[test]
fn replay_rebuild_tool_result_drops_error_shaped() {
    let lua = lua_with_lib();
    let n: i64 = lua
        .load(
            r#"
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = { kind = "ollama.chat.create", chat_id = "c1" },
            }, "ollama")
            provider.replay_rebuild({
                type = "event", from = "provider-wrapper", replay = true,
                body = { kind = "tool.result", id = "f1", error = "boom" },
            }, "ollama")
            return #_delivered_log
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(
        n, 1,
        "error-shaped tool.result must not synthesize an append"
    );
}

#[test]
fn replay_rebuild_tool_result_drops_for_unowned_chat() {
    let lua = lua_with_lib();
    let n: i64 = lua
        .load(
            r#"
            -- chat-other belongs to a different provider; this name doesn't
            -- own it, so the tool.result must not synthesize an append.
            provider.replay_rebuild({
                type = "event", from = "provider-wrapper", replay = true,
                body = {
                    kind   = "tool.result",
                    id     = "f1",
                    result = { text = "x", next_state = { chat_id = "c-other" } },
                },
            }, "ollama")
            return #_delivered_log
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(n, 0);
}

#[test]
fn replay_rebuild_tool_result_drops_when_empty_content() {
    let lua = lua_with_lib();
    let n: i64 = lua
        .load(
            r#"
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = { kind = "ollama.chat.create", chat_id = "c1" },
            }, "ollama")
            -- Empty text + no tool_calls → no assistant turn to record.
            provider.replay_rebuild({
                type = "event", from = "provider-wrapper", replay = true,
                body = {
                    kind   = "tool.result",
                    id     = "f1",
                    result = { text = "", next_state = { chat_id = "c1" } },
                },
            }, "ollama")
            return #_delivered_log
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(n, 1, "create delivered; empty-content tool.result skipped");
}

#[test]
fn replay_rebuild_tool_result_includes_tool_calls_in_synthesis() {
    let lua = lua_with_lib();
    let (n, has_tool_calls): (i64, bool) = lua
        .load(
            r#"
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = { kind = "ollama.chat.create", chat_id = "c1" },
            }, "ollama")
            provider.replay_rebuild({
                type = "event", from = "provider-wrapper", replay = true,
                body = {
                    kind   = "tool.result",
                    id     = "f1",
                    result = {
                        text       = "",
                        tool_calls = { { id = "t1", name = "read_file", arguments = "{}" } },
                        next_state = { chat_id = "c1" },
                    },
                },
            }, "ollama")
            local row = _delivered_log[2]
            local tcs = row.body.message.tool_calls
            return #_delivered_log, type(tcs) == "table" and #tcs == 1
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(n, 2);
    assert!(
        has_tool_calls,
        "synthesized assistant message must carry tool_calls"
    );
}

#[test]
fn replay_rebuild_cross_name_ownership_is_isolated() {
    // Two translators (mock-plugin + ollama) on the same Lua state.
    // Ownership must be per-name; replay for ollama's chat must not
    // leak into mock-plugin's owned set and vice versa.
    let lua = lua_with_lib();
    let (mock_count, ollama_count): (i64, i64) = lua
        .load(
            r#"
            -- Create one chat per provider.
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = { kind = "mock-plugin.chat.create", chat_id = "c-mock" },
            }, "mock-plugin")
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = { kind = "ollama.chat.create", chat_id = "c-ollama" },
            }, "ollama")
            -- tool.result for the ollama chat. Mock-plugin must NOT
            -- synthesize an append (it doesn't own c-ollama); ollama MUST.
            provider.replay_rebuild({
                type = "event", from = "provider-wrapper", replay = true,
                body = {
                    kind = "tool.result", id = "f1",
                    result = { text = "ollama reply", next_state = { chat_id = "c-ollama" } },
                },
            }, "mock-plugin")
            provider.replay_rebuild({
                type = "event", from = "provider-wrapper", replay = true,
                body = {
                    kind = "tool.result", id = "f1",
                    result = { text = "ollama reply", next_state = { chat_id = "c-ollama" } },
                },
            }, "ollama")
            local mock_n, ollama_n = 0, 0
            for i = 1, #_delivered_log do
                local row = _delivered_log[i]
                if row.peer == "mock-plugin" then mock_n = mock_n + 1
                elseif row.peer == "ollama"   then ollama_n = ollama_n + 1 end
            end
            return mock_n, ollama_n
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(mock_count, 1, "mock-plugin: chat.create only");
    assert_eq!(ollama_count, 2, "ollama: chat.create + synthesized append");
}

// ---------------------------------------------------------------------
// replay_rebuild — chat.complete.result (orchestrator path)
// ---------------------------------------------------------------------
//
// Regression: chatgpt + openai-provider hold model-emitted assistant
// turns (`push_assistant_tool_calls`) only in process memory. On
// /resume the bus log re-fires chat.create + chat.append for user/tool
// messages, but the assistant entries were never on the bus and don't
// rebuild — leaving orphaned `function_call_output` items in the
// rebuilt history and a 400 ("No tool call found for function call
// output") on the next /responses POST. The sub-agent path was covered
// by the `tool.result` synthesis arm; this is the orchestrator
// equivalent (chat-1 driven directly via `<prefix>.chat.complete`).

#[test]
fn replay_rebuild_chat_complete_result_synthesizes_assistant_append() {
    let lua = lua_with_lib();
    let (n, kind, role, content, n_tcs): (i64, String, String, String, i64) = lua
        .load(
            r#"
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = { kind = "ollama.chat.create", chat_id = "c1" },
            }, "ollama")
            provider.replay_rebuild({
                type = "event", from = "ollama", replay = true,
                body = {
                    kind    = "ollama.chat.complete.result",
                    chat_id = "c1",
                    output  = {
                        text          = "thinking...",
                        finish_reason = "tool_calls",
                        tool_calls    = {
                            {
                                id       = "call_abc",
                                name     = "dispatch-graph",
                                arguments = { nodes = {} },
                            },
                        },
                    },
                },
            }, "ollama")
            local row = _delivered_log[2]
            return #_delivered_log, row.body.kind, row.body.message.role,
                   row.body.message.content, #row.body.message.tool_calls
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(n, 2, "chat.create + synthesized assistant chat.append");
    assert_eq!(kind, "ollama.chat.append");
    assert_eq!(role, "assistant");
    assert_eq!(content, "thinking...");
    assert_eq!(
        n_tcs, 1,
        "tool_calls from chat.complete.result.output ride into the synthesized message"
    );
}

#[test]
fn replay_rebuild_chat_complete_result_drops_for_unowned_chat() {
    let lua = lua_with_lib();
    let n: i64 = lua
        .load(
            r#"
            provider.replay_rebuild({
                type = "event", from = "ollama", replay = true,
                body = {
                    kind    = "ollama.chat.complete.result",
                    chat_id = "c-other",
                    output  = { text = "x" },
                },
            }, "ollama")
            return #_delivered_log
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(n, 0, "unowned chat must not produce a synthesized append");
}

#[test]
fn replay_rebuild_chat_complete_result_drops_empty_assistant_turn() {
    let lua = lua_with_lib();
    let n: i64 = lua
        .load(
            r#"
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = { kind = "ollama.chat.create", chat_id = "c1" },
            }, "ollama")
            -- finish_reason=error with no text and no tool_calls — the
            -- chat.complete failed (e.g. HTTP 503). No assistant turn
            -- to record; must not synthesize a junk message.
            provider.replay_rebuild({
                type = "event", from = "ollama", replay = true,
                body = {
                    kind    = "ollama.chat.complete.result",
                    chat_id = "c1",
                    output  = { text = "", finish_reason = "error" },
                },
            }, "ollama")
            return #_delivered_log
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(
        n, 1,
        "failed chat.complete.result must not produce an empty assistant append"
    );
}

#[test]
fn replay_rebuild_chat_complete_result_text_only_synthesizes() {
    // Final assistant turn after a tool-call cycle: text + no tool_calls.
    // Must still synthesize the assistant chat.append so the orchestrator
    // chat sees its own terminal message on resume.
    let lua = lua_with_lib();
    let (n, content, role): (i64, String, String) = lua
        .load(
            r#"
            provider.replay_rebuild({
                type = "event", from = "engine", replay = true,
                body = { kind = "ollama.chat.create", chat_id = "c1" },
            }, "ollama")
            provider.replay_rebuild({
                type = "event", from = "ollama", replay = true,
                body = {
                    kind    = "ollama.chat.complete.result",
                    chat_id = "c1",
                    output  = { text = "Done.", finish_reason = "stop" },
                },
            }, "ollama")
            local row = _delivered_log[2]
            return #_delivered_log, row.body.message.content, row.body.message.role
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(n, 2);
    assert_eq!(content, "Done.");
    assert_eq!(role, "assistant");
}
