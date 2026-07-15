use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mlua::{Lua, Table};

#[derive(Default)]
struct Deliveries {
    singles: Mutex<Vec<String>>,
    batches: Mutex<Vec<Vec<String>>>,
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn setup() -> (Lua, Arc<Deliveries>) {
    let lua = Lua::new();
    let deliveries = Arc::new(Deliveries::default());
    let nefor = lua.create_table().unwrap();
    let engine = lua.create_table().unwrap();

    let single_deliveries = Arc::clone(&deliveries);
    engine
        .set(
            "deliver",
            lua.create_function(move |_, (_peer, payload): (String, String)| {
                single_deliveries.singles.lock().unwrap().push(payload);
                Ok(())
            })
            .unwrap(),
        )
        .unwrap();

    let batch_deliveries = Arc::clone(&deliveries);
    engine
        .set(
            "deliver_batch",
            lua.create_function(move |_, (_peer, payloads): (String, Table)| {
                let payloads = payloads
                    .sequence_values::<String>()
                    .collect::<mlua::Result<Vec<_>>>()?;
                batch_deliveries.batches.lock().unwrap().push(payloads);
                Ok(())
            })
            .unwrap(),
        )
        .unwrap();

    engine
        .set(
            "send",
            lua.create_function(|_, _: mlua::Variadic<mlua::Value>| Ok(()))
                .unwrap(),
        )
        .unwrap();
    engine
        .set(
            "now",
            lua.create_function(|_, _: ()| Ok("2026-07-15T00:00:00.000Z"))
                .unwrap(),
        )
        .unwrap();
    nefor.set("engine", engine).unwrap();
    nefor::lua::bindings::install_json(&lua, &nefor).unwrap();
    lua.globals().set("nefor", nefor).unwrap();

    let lua_root = repo_root().join("lua");
    lua.load(format!(
        r#"package.path = "{}/?.lua;{}/?/init.lua;" .. package.path"#,
        lua_root.display(),
        lua_root.display()
    ))
    .exec()
    .unwrap();
    (lua, deliveries)
}

#[test]
fn replay_bearing_tui_batch_protects_complete_frame() {
    let (lua, deliveries) = setup();
    lua.load(
        r#"
        local spec = require("libs.compositors.chat_bridge").spawn_spec({ "nefor-tui" })
        local envs = {
          { type = "event", from = "sessions", body = { kind = "sessions.session_start" }, replay = false },
          { type = "event", from = "sessions", body = { kind = "sessions.replay.start" }, replay = true },
        }
        for i = 1, 1300 do
          envs[#envs + 1] = {
            type = "event",
            from = "agentic-loop",
            body = { kind = "chat.message.append", index = i },
            replay = true,
          }
        end
        envs[#envs + 1] = { type = "event", from = "sessions", body = { kind = "sessions.replay.end" }, replay = false }
        envs[#envs + 1] = { type = "event", from = "sessions", body = { kind = "sessions.resume_done" }, replay = false }
        spec.to_plugin(envs)
        "#,
    )
    .exec()
    .unwrap();

    assert!(deliveries.singles.lock().unwrap().is_empty());
    let batches = deliveries.batches.lock().unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].len(), 1_304);
    let first: serde_json::Value = serde_json::from_str(&batches[0][0]).unwrap();
    let middle: serde_json::Value = serde_json::from_str(&batches[0][652]).unwrap();
    let last: serde_json::Value = serde_json::from_str(&batches[0][1_303]).unwrap();
    assert_eq!(first["body"]["kind"], "sessions.session_start");
    assert_eq!(middle["body"]["kind"], "chat.message.append");
    assert_eq!(last["body"]["kind"], "sessions.resume_done");
}

#[test]
fn live_only_tui_batch_retains_bounded_single_delivery_path() {
    let (lua, deliveries) = setup();
    lua.load(
        r#"
        local spec = require("libs.compositors.chat_bridge").spawn_spec({ "nefor-tui" })
        spec.to_plugin({
          { type = "event", from = "agentic-loop", body = { kind = "chat.stream.delta", index = 1 }, replay = false },
          { type = "event", from = "agentic-loop", body = { kind = "chat.stream.delta", index = 2 }, replay = false },
        })
        "#,
    )
    .exec()
    .unwrap();

    assert_eq!(deliveries.singles.lock().unwrap().len(), 2);
    assert!(deliveries.batches.lock().unwrap().is_empty());
}
