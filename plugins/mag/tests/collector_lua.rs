use mlua::{Lua, Table};
use std::path::PathBuf;

fn harness() -> Lua {
    let lua = Lua::new();
    let package: Table = lua.globals().get("package").unwrap();
    let current: String = package.get("path").unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel");
    package
        .set(
            "path",
            format!("{0}/?.lua;{0}/?/init.lua;{current}", root.display()),
        )
        .unwrap();
    lua
}

#[test]
fn collector_orders_by_trusted_sender_and_emits_once() {
    harness()
        .load(
            r#"
            local factory = require("factories.collector")
            local emitted = {}
            local actor = assert(factory.construct("join", {
              expected_senders = { "worker.0", "worker.1", "worker.2" }
            }, function(message) emitted[#emitted + 1] = message end))
            actor.deliver({ messages = {{ from = "worker.2", message = { value = "c", from = "forged" } }} })
            actor.deliver({ messages = {{ from = "worker.0", message = { value = "a" } }} })
            actor.deliver({ messages = {{ from = "worker.1", message = { value = "b" } }} })
            assert(#emitted == 2) -- ready + one output
            assert(emitted[2].kind == "nefor.dynamic.Collected")
            assert(table.concat(emitted[2].value, "") == "abc")
            local completion = actor.deliver({ messages = {{ from = "worker.1", message = { value = "again" } }} })
            assert(completion.status == "failed")
            assert(completion.value.kind == "collector_already_finished")
            assert(#emitted == 2)
            "#,
        )
        .exec()
        .unwrap();
}

#[test]
fn collector_rejects_bad_topology_and_arrivals_and_clears_on_kill() {
    harness()
        .load(
            r#"
            local factory = require("factories.collector")
            assert(factory.construct("zero", { expected_senders = {} }, function() end) == nil)
            assert(factory.construct("dup", { expected_senders = { "a", "a" } }, function() end) == nil)
            local actor = assert(factory.construct("join", { expected_senders = { "a", "b" } }, function() end))
            local unexpected = actor.deliver({ messages = {{ from = "x", message = { value = 1 } }} })
            assert(unexpected.status == "failed")
            assert(unexpected.value.kind == "collector_unexpected_sender")
            local partial = assert(factory.construct("partial", { expected_senders = { "a", "b" } }, function() end))
            partial.deliver({ messages = {{ from = "a", message = { value = 1 } }} })
            local duplicate = partial.deliver({ messages = {{ from = "a", message = { value = 2 } }} })
            assert(duplicate.status == "failed")
            assert(duplicate.value.kind == "collector_duplicate_sender")
            local drained_out = {}
            local drained = assert(factory.construct("drained", { expected_senders = { "a", "b" } },
              function(message) drained_out[#drained_out + 1] = message end))
            drained.deliver({ messages = {{ from = "a", message = { value = 1 } }} })
            drained.handle_drain()
            assert(drained_out[#drained_out].kind == "mag.failed")
            assert(drained_out[#drained_out].value.kind == "collector_drained_incomplete")
            local killed = assert(factory.construct("killed", { expected_senders = { "a" } }, function() end))
            killed.handle_kill()
            local after = killed.deliver({ messages = {{ from = "a", message = { value = 1 } }} })
            assert(after.status == "failed")
            "#,
        )
        .exec()
        .unwrap();
}

#[test]
fn outcome_emits_once_and_drain_settles_an_empty_boundary() {
    harness()
        .load(
            r#"
            local factory = require("factories.outcome")
            local emitted = {}
            local actor = assert(factory.construct("result", {},
              function(message) emitted[#emitted + 1] = message end))
            local value = { tag = "core.validated.Valid", value = "done" }
            assert(actor.deliver({ messages = {{ message = { value = value } }} }).status == "ok")
            assert(emitted[2].kind == "nefor.outcome.Result")
            assert(emitted[2].value == value)
            local duplicate = actor.deliver({ messages = {{ message = { value = value } }} })
            assert(duplicate.status == "failed")
            assert(duplicate.value.kind == "duplicate_outcome")
            assert(#emitted == 2)

            local drained = {}
            local waiting = assert(factory.construct("waiting", {},
              function(message) drained[#drained + 1] = message end))
            waiting.handle_drain()
            assert(drained[2].kind == "mag.failed")
            assert(drained[2].value.kind == "outcome_drained")
            "#,
        )
        .exec()
        .unwrap();
}
