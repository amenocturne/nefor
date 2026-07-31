use mlua::{Lua, Table};
use std::path::PathBuf;

fn run(script: &str) {
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
    lua.load(
        r#"
        events = {}
        nefor = {
          log = function() end,
          now_ms = function() return 0 end,
          emit = function(event) events[#events + 1] = event end,
        }
        "#,
    )
    .exec()
    .unwrap();
    let kernel_path = root.join("init.lua");
    let kernel_source = std::fs::read_to_string(kernel_path).unwrap();
    let kernel: Table = lua.load(&kernel_source).eval().unwrap();
    lua.globals().set("kernel", kernel).unwrap();
    lua.load(script).exec().unwrap();
}

#[test]
fn terminal_settlement_is_first_write_wins_even_after_host_take() {
    run(r#"
        assert(kernel.begin_run({run_id="race", run_name="race", session_id="s"}).ok)
        assert(kernel.start("race", {
          actors={{id="result", factory="nefor.factory.stub", params={greeting="first"}, routes={}}},
          messages={{to="result", content={kind="stub.In"}}}, kills={}, rules={},
          result={from={actor="result", wire="stub.Out"}}
        }).ok)
        local emit = kernel.context("race").router:emitter("result")
        emit({kind="stub.Out", greeting="second"})
        local first = assert(kernel.take_run_complete("race"))
        assert(first.result.greeting == "first")
        emit({kind="stub.Out", greeting="third"})
        assert(kernel.take_run_complete("race") == nil)
        local ignored, completed = 0, 0
        for _, event in ipairs(events) do
          if event.kind == "mag.terminal_settlement_ignored" then ignored = ignored + 1 end
          if event.kind == "mag.run_complete" then completed = completed + 1 end
        end
        assert(ignored == 2)
        assert(completed == 1)
        assert(kernel.context("race").terminal_settlement.completion.result.greeting == "first")
        "#);
}

#[test]
fn killed_generation_cannot_route_settle_or_trigger_rules() {
    run(r#"
        assert(kernel.begin_run({run_id="stale", run_name="stale", session_id="s"}).ok)
        assert(kernel.start("stale", {
          actors={
            {id="source", factory="nefor.factory.stub", params={value="first"},
             routes={["stub.Out"]={{actor="downstream", wire="stub.In"}}}},
            {id="downstream", factory="nefor.factory.stub", params={}, routes={}},
            {id="result", factory="nefor.factory.stub", params={}, routes={}}
          },
          messages={{to="source", content={kind="stub.In"}}}, kills={},
          rules={{id="watch", on={actor="source", wire="stub.Out"}, fn="noop"}},
          result={from={actor="result", wire="stub.Out"}}
        }).ok)
        assert(kernel.take_rule_trigger("stale"))
        local ctx = kernel.context("stale")
        local stale_emit = ctx.router:emitter("source")
        assert(kernel.apply("stale", {actors={}, messages={}, kills={"source"}, rules={}}).ok)
        stale_emit({kind="stub.Out", value="late"})
        assert(kernel.take_rule_trigger("stale") == nil)
        assert(kernel.take_run_complete("stale") == nil)
        local ignored = 0
        for _, event in ipairs(events) do
          if event.kind == "mag.emission_ignored" and event.from == "source" then
            ignored = ignored + 1
            assert(event.reason == "actor_dead")
          end
        end
        assert(ignored == 1)
        "#);
}
