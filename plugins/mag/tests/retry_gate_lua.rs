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
fn retry_gate_obeys_budgets_and_preserves_payload_identity() {
    harness()
        .load(
            r#"
            local factory = require("factories.retry-gate")
            local function exercise(maximum, count)
              local emitted = {}
              local actor = assert(factory.construct("gate", { max_retries = maximum },
                function(message) emitted[#emitted + 1] = message end))
              local payloads = {}
              for index = 1, count do
                payloads[index] = { index = index }
                actor.deliver({ messages = {{ message = { value = payloads[index] } }} })
              end
              return emitted, payloads
            end

            local zero, zero_payloads = exercise(0, 1)
            assert(#zero == 2 and zero[2].kind == "nefor.retry.Exhausted")
            assert(rawequal(zero[2].value, zero_payloads[1]))

            local one, one_payloads = exercise(1, 2)
            assert(one[2].kind == "nefor.retry.Continue" and rawequal(one[2].value, one_payloads[1]))
            assert(one[3].kind == "nefor.retry.Exhausted" and rawequal(one[3].value, one_payloads[2]))

            local three, three_payloads = exercise(3, 4)
            for index = 1, 3 do
              assert(three[index + 1].kind == "nefor.retry.Continue")
              assert(rawequal(three[index + 1].value, three_payloads[index]))
            end
            assert(three[5].kind == "nefor.retry.Exhausted")
            assert(rawequal(three[5].value, three_payloads[4]))
            "#,
        )
        .exec()
        .unwrap();
}

#[test]
fn retry_gate_latches_and_diagnoses_late_input_without_branch_output() {
    harness()
        .load(
            r#"
            local factory = require("factories.retry-gate")
            local emitted, diagnostics = {}, {}
            local actor = assert(factory.construct("gate", { max_retries = 0 },
              function(message) emitted[#emitted + 1] = message end,
              { diagnostic = function(value) diagnostics[#diagnostics + 1] = value end }))
            actor.deliver({ messages = {{ message = { value = { first = true } } }} })
            actor.deliver({ messages = {{ message = { value = { late = true } } }} })
            actor.deliver({ messages = {{ message = { value = { later = true } } }} })
            assert(#emitted == 2) -- ready + exactly one exhausted branch
            assert(emitted[2].kind == "nefor.retry.Exhausted")
            assert(#diagnostics == 2)
            assert(diagnostics[1].kind == "late_input_after_exhaustion")
            assert(diagnostics[2].kind == "late_input_after_exhaustion")
            assert(factory.construct("bad", { max_retries = -1 }, function() end) == nil)
            assert(factory.construct("bad", { max_retries = 1.5 }, function() end) == nil)
            "#,
        )
        .exec()
        .unwrap();
}
