//! Unit tests for `starter/openai-provider/init.lua` driven from Rust.
//!
//! These tests exercise the `to_plugin` callback during a synthetic
//! session-replay window — the path that rebuilds the provider binary's
//! per-chat_id history table on cross-process /resume after a nefor
//! restart. The harness installs a stub `nefor.*` surface that records
//! every `nefor.engine.deliver(name, payload)` call so the test can
//! assert the binary saw the right re-fed `<prefix>.chat.create` /
//! `<prefix>.chat.append` envelopes.
//!
//! Post batch-protocol refactor `to_plugin` takes a LIST of envelopes
//! per invocation (`function to_plugin(envs) for _, env in ipairs(envs)
//! do ... end end`). The Lua-side helpers below feed the wrapper a
//! one-element list per "live" call to keep the per-envelope semantics
//! these tests pin, and per-envelope the test sets `env.replay` to
//! match what the framework would stamp inline as it walks
//! `sessions.replay.*` framing.
//!
//! Why not e2e: the cross-process flow (kill nefor, start fresh, /resume)
//! requires driving the engine binary across two process lifetimes plus
//! a real provider binary. Pinning the contract at the wrapper boundary
//! is sharper for regression: any change that breaks the rebuild path
//! shows up here even if the e2e harness can't easily express the
//! cross-process scenario.

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
        .and_then(|p| p.parent())
        .expect("repo root is two levels above crates/nefor")
        .to_path_buf()
}

/// Drive the wrapper's `to_plugin` callback for a sequence of replayed
/// envelopes inside a `sessions.replay.start` / `sessions.replay.end`
/// window, then assert every `nefor.engine.deliver(name, ...)` payload
/// the wrapper produced.
///
/// Scenario covered:
///   1. `<prefix>.chat.create` → delivered to binary verbatim.
///   2. `<prefix>.chat.append { user }` → delivered to binary verbatim.
///   3. `tool.result` carrying `result.text` + `result.next_state.chat_id`
///      that we own → synthesises an assistant `<prefix>.chat.append`.
///   4. `<prefix>.chat.append { tool }` → delivered to binary verbatim.
///
/// Net effect: the binary observes the full
/// system → user → assistant → tool history exactly as the live path
/// would have produced it, so the next live turn (a fresh
/// `<prefix>.chat.complete`) sees full prior context.
#[test]
fn replay_window_re_feeds_chat_history_into_provider_binary() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    // Spawn the wrapper. `nefor.plugins.spawn` is a no-op stub, so this
    // just returns the spec table; we drive `to_plugin` directly.
    lua.load(
        r#"
        local op = require("provider")
        local spec = op.spawn_spec("ollama", { "/bin/true" }, {})
        _to_plugin = spec.to_plugin
        _replay = require("core.history_replay")
        "#,
    )
    .exec()
    .expect("spawn wrapper");

    // Replay window open. Drive the recorded envelope sequence — these
    // are the shapes the bus log carries on disk after a normal turn.
    // Batched signature: every call hands the wrapper a one-element list
    // with `env.replay = true` (what the framework stamps when iterating
    // a tail framed by sessions.replay.start/end).
    lua.load(
        r#"
        _replay.set(true)
        local function deliver_env(from, body)
            _to_plugin({ { type = "event", from = from, body = body, replay = true } })
        end

        -- 1. Engine→provider chat.create (recorded with target=ollama).
        deliver_env("engine", { kind = "ollama.chat.create", chat_id = "chat-1", model = "qwen3" })
        -- 2. Engine→provider chat.append { system }.
        deliver_env("engine", {
            kind    = "ollama.chat.append",
            chat_id = "chat-1",
            message = { role = "system", content = "you are a helpful assistant" },
        })
        -- 3. Engine→provider chat.append { user }.
        deliver_env("engine", {
            kind    = "ollama.chat.append",
            chat_id = "chat-1",
            message = { role = "user", content = "hi" },
        })
        -- 4. Wrapper-emitted tool.result close. Carries the assistant
        --    text + next_state.chat_id; the live binary push_assistant
        --    happened inside the provider process, so we synthesize the
        --    chat.append here.
        deliver_env("provider-wrapper", {
            kind   = "tool.result",
            id     = "firing-1",
            result = {
                text          = "Hello! How can I help?",
                finish_reason = "stop",
                next_state    = { chat_id = "chat-1" },
            },
        })

        _replay.set(false)
        _delivered = _test.delivered()
        "#,
    )
    .exec()
    .expect("drive replay");

    let delivered = collect_delivered(&lua, "_delivered");

    // Assertion shape: every payload delivered to "ollama" carries a
    // `<prefix>.chat.{create,append}` body. The synthesized assistant
    // append is at index 4 (after create + 2 appends + the synthesis
    // input was a tool.result that we don't deliver verbatim).
    assert_eq!(
        delivered.len(),
        4,
        "expected 4 delivered envelopes (create + system + user + synthesized assistant), got {}: {:#?}",
        delivered.len(),
        delivered
    );

    let kinds: Vec<&str> = delivered.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            "ollama.chat.create",
            "ollama.chat.append",
            "ollama.chat.append",
            "ollama.chat.append",
        ],
        "kinds re-fed in order: create → system append → user append → synthesized assistant append; got {:?}",
        delivered
    );

    let roles: Vec<Option<&str>> = delivered
        .iter()
        .map(|e| e.role.as_deref())
        .collect();
    assert_eq!(
        roles,
        vec![None, Some("system"), Some("user"), Some("assistant")],
        "roles in order; got {:#?}",
        delivered
    );

    // The synthesized assistant message must carry the assistant text.
    let assistant = &delivered[3];
    assert_eq!(
        assistant.content.as_deref(),
        Some("Hello! How can I help?"),
        "assistant chat.append must carry the model's text from result.text"
    );

    // Ownership: the chat_id reaches every delivered envelope.
    for entry in &delivered {
        assert_eq!(
            entry.chat_id.as_deref(),
            Some("chat-1"),
            "every re-fed envelope carries the original chat_id; got {:#?}",
            entry
        );
    }
}

/// Cross-wrapper isolation: when two providers (mock-plugin + ollama)
/// coexist, only the wrapper that originally created a chat_id should
/// re-feed envelopes for it on replay. Without ownership filtering the
/// "wrong" wrapper would deliver chat.append envelopes to its binary
/// for chat_ids it never created — and the binary would emit a
/// chat.error because that chat_id doesn't exist in its table.
#[test]
fn cross_wrapper_isolation_unowned_chat_ids_drop() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    lua.load(
        r#"
        local op = require("provider")
        -- mock_provider chats and ollama chats use independent spawn_spec
        -- instances; the two actor instances don't share state.
        _mock_to_plugin   = op.spawn_spec("mock-plugin", { "/bin/true" }, {}).to_plugin
        _ollama_to_plugin = op.spawn_spec("ollama",      { "/bin/true" }, {}).to_plugin
        _replay = require("core.history_replay")
        "#,
    )
    .exec()
    .expect("spawn both provider instances");

    lua.load(
        r#"
        _replay.set(true)
        local function fire(body, from)
            local envs = { { type = "event", from = from, body = body, replay = true } }
            _mock_to_plugin(envs)
            _ollama_to_plugin(envs)
        end

        -- Mock chat is created via mock-plugin.chat.create.
        fire({ kind = "mock-plugin.chat.create", chat_id = "chat-mock-1" }, "engine")
        fire({
            kind    = "mock-plugin.chat.append",
            chat_id = "chat-mock-1",
            message = { role = "user", content = "hi mock" },
        }, "engine")
        -- Ollama chat is created via ollama.chat.create.
        fire({ kind = "ollama.chat.create", chat_id = "chat-ollama-1" }, "engine")
        fire({
            kind    = "ollama.chat.append",
            chat_id = "chat-ollama-1",
            message = { role = "user", content = "hi ollama" },
        }, "engine")
        -- tool.result for the ollama chat. Only the ollama wrapper owns
        -- it; mock-plugin must drop.
        fire({
            kind = "tool.result", id = "f1",
            result = { text = "ollama reply", next_state = { chat_id = "chat-ollama-1" } },
        }, "provider-wrapper")

        _replay.set(false)
        _delivered = _test.delivered()
        "#,
    )
    .exec()
    .expect("drive replay");

    let delivered = collect_delivered(&lua, "_delivered");

    // Filter per binary.
    let mock_payloads: Vec<&DeliveredEntry> = delivered
        .iter()
        .filter(|e| e.peer == "mock-plugin")
        .collect();
    let ollama_payloads: Vec<&DeliveredEntry> = delivered
        .iter()
        .filter(|e| e.peer == "ollama")
        .collect();

    // mock-plugin: create + user append only. No ollama-prefixed
    // envelopes leaked through, no synthesized assistant for the
    // ollama chat.
    assert_eq!(
        mock_payloads.len(),
        2,
        "mock-plugin must see only its own chat.create + chat.append; got {:#?}",
        mock_payloads
    );
    assert!(
        mock_payloads
            .iter()
            .all(|e| e.kind.starts_with("mock-plugin.")),
        "mock-plugin must not see ollama-prefixed envelopes during replay; got {:#?}",
        mock_payloads
    );
    assert!(
        mock_payloads
            .iter()
            .all(|e| e.chat_id.as_deref() == Some("chat-mock-1")),
        "mock-plugin must only re-feed its owned chat_id; got {:#?}",
        mock_payloads
    );

    // ollama: create + user append + synthesized assistant. No mock-prefixed
    // envelopes leaked through.
    assert_eq!(
        ollama_payloads.len(),
        3,
        "ollama must see its own create + user + synthesized assistant; got {:#?}",
        ollama_payloads
    );
    let ollama_kinds: Vec<&str> = ollama_payloads.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        ollama_kinds,
        vec!["ollama.chat.create", "ollama.chat.append", "ollama.chat.append"],
        "ollama re-feed shape; got {:#?}",
        ollama_payloads
    );
    let ollama_roles: Vec<Option<&str>> =
        ollama_payloads.iter().map(|e| e.role.as_deref()).collect();
    assert_eq!(
        ollama_roles,
        vec![None, Some("user"), Some("assistant")],
        "ollama roles in order; got {:#?}",
        ollama_payloads
    );
}

/// In-process /resume of a chat already created in the same process must
/// not re-deliver `<prefix>.chat.create` to the binary — the binary's
/// `chats.create` errors on duplicate ids. The wrapper tracks
/// `owned_chat_ids` on the live path too, so the replay handler can
/// recognise an in-process duplicate and skip re-delivery.
///
/// Cross-process resume (the bug we fixed) is the OPPOSITE of this:
/// fresh process, owned set is empty, first-seen chat.create gets
/// through.
#[test]
fn in_process_resume_skips_duplicate_chat_create() {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    lua.load(
        r#"
        local op = require("provider")
        _to_plugin = op.spawn_spec("ollama", { "/bin/true" }, {}).to_plugin
        _replay = require("core.history_replay")
        "#,
    )
    .exec()
    .expect("spawn wrapper");

    // Live turn — wrapper sees chat.create through the live path.
    // Batched signature: hand the wrapper a one-element envs list with
    // `replay = false` (the framework's per-envelope stamp under live
    // dispatch).
    lua.load(
        r#"
        _replay.set(false)
        _to_plugin({ {
            type = "event", from = "engine", replay = false,
            body = { kind = "ollama.chat.create", chat_id = "chat-1" },
        } })
        _live_delivered_count = #_test.delivered()
        "#,
    )
    .exec()
    .expect("drive live");

    let live_count: i64 = lua
        .load(r#"return _live_delivered_count"#)
        .eval()
        .expect("live count");
    assert_eq!(live_count, 1, "live path delivers chat.create to binary");

    // /resume of the same chat_id while still in-process — replay path
    // sees chat.create again. Must skip (otherwise binary errors).
    lua.load(
        r#"
        _replay.set(true)
        _to_plugin({ {
            type = "event", from = "engine", replay = true,
            body = { kind = "ollama.chat.create", chat_id = "chat-1" },
        } })
        _replay.set(false)
        _replay_delivered = _test.delivered()
        "#,
    )
    .exec()
    .expect("drive replay");

    let after = collect_delivered(&lua, "_replay_delivered");
    assert_eq!(
        after.len(),
        0,
        "in-process /resume must NOT re-deliver an already-created chat.create; \
         live path already delivered it once and `_test.delivered()` drains the \
         log between snapshots, so the replay-window snapshot must be empty. \
         Got {:#?}",
        after
    );
}

// --------------------------------------------------------------------
// harness
// --------------------------------------------------------------------

#[derive(Debug)]
struct DeliveredEntry {
    peer: String,
    kind: String,
    chat_id: Option<String>,
    role: Option<String>,
    content: Option<String>,
}

fn collect_delivered(lua: &Lua, global: &str) -> Vec<DeliveredEntry> {
    let tbl: Table = lua.globals().get(global).expect("delivered table");
    let len = tbl.len().expect("len") as usize;
    let mut out = Vec::with_capacity(len);
    for i in 1..=len {
        let row: Table = tbl.get(i).expect("row");
        let peer: String = row.get("peer").expect("peer");
        let kind: String = row.get("kind").expect("kind");
        let chat_id: Option<String> = match row.get::<Value>("chat_id").ok() {
            Some(Value::String(s)) => s.to_str().ok().map(|bs| bs.to_string()),
            _ => None,
        };
        let role: Option<String> = match row.get::<Value>("role").ok() {
            Some(Value::String(s)) => s.to_str().ok().map(|bs| bs.to_string()),
            _ => None,
        };
        let content: Option<String> = match row.get::<Value>("content").ok() {
            Some(Value::String(s)) => s.to_str().ok().map(|bs| bs.to_string()),
            _ => None,
        };
        out.push(DeliveredEntry {
            peer,
            kind,
            chat_id,
            role,
            content,
        });
    }
    out
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
    // history_replay module no longer self-subscribes at require-time
    // (now wired explicitly by starter/init.lua via
    // history_replay.install()); these tests drive the replay-window
    // flag via the module's public set() helper instead.
    let bus_tbl = lua.create_table()?;
    let on_event = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
    bus_tbl.set("on_event", on_event)?;
    nefor.set("bus", bus_tbl)?;

    // engine — record `deliver(peer, payload)` calls into a global
    // `_delivered_log` table that tests inspect via `_test.delivered()`.
    // `send` is a no-op (the wrapper doesn't `send` from to_plugin in
    // the replay path; only `deliver`).
    let engine_tbl = lua.create_table()?;
    let send_fn = lua.create_function(|_, _: mlua::Variadic<Value>| Ok(()))?;
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
        row.set("role", message_role)?;
        row.set("content", message_content)?;
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
    let rg_plugin_lua = repo_root().join("plugins").join("reasoner-graph").join("lua");
    let rg_plugin_lua_str = rg_plugin_lua.display().to_string();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{starter}/?.lua",
          "{starter}/?/init.lua",
          "{plugin_lua}/?.lua",
          "{plugin_lua}/?/init.lua",
          "{rg_plugin_lua}/?.lua",
          "{rg_plugin_lua}/?/init.lua",
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
        rg_plugin_lua = rg_plugin_lua_str,
    );
    lua.load(&script).exec()
}
