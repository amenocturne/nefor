//! End-to-end lowering acceptance test: the two-agents design fixture compiles
//! from its `.mag` source to its hand-lowered modification, byte-for-byte modulo
//! the out-of-band `hash` field and source formatting.

use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/mag/tests/fixtures")
}

#[test]
fn two_agents_fixture_compiles_modulo_hash() {
    let dir = fixtures_dir();
    let source =
        std::fs::read_to_string(dir.join("two-agents.mag")).expect("fixture source should exist");
    let ir = nefor_mag::compile(&source, &dir).expect("fixture should compile");

    // Structural + ordering comparison: re-serialize both through serde so
    // formatting is normalized; drop the hash field the compiler adds.
    let mut produced = serde_json::to_value(&ir).expect("modification serializes");
    produced
        .as_object_mut()
        .expect("modification is an object")
        .remove("hash");

    let expected_str = std::fs::read_to_string(dir.join("two-agents.modification.json"))
        .expect("fixture json should exist");
    let expected: serde_json::Value =
        serde_json::from_str(&expected_str).expect("fixture json parses");

    let produced_pretty = serde_json::to_string_pretty(&produced).unwrap();
    let expected_pretty = serde_json::to_string_pretty(&expected).unwrap();

    assert_eq!(
        produced_pretty, expected_pretty,
        "compiled modification diverges from the fixture"
    );
}

#[test]
fn two_agents_fixture_has_deterministic_hash() {
    let dir = fixtures_dir();
    let source = std::fs::read_to_string(dir.join("two-agents.mag")).unwrap();
    let a = nefor_mag::compile(&source, &dir).unwrap();
    let b = nefor_mag::compile(&source, &dir).unwrap();
    assert_eq!(a.hash, b.hash);
    assert!(a.hash.starts_with("sha256:"));
}

#[test]
fn colliding_agent_ids_are_rejected() {
    // Two agent instantiations sharing an :id collide on every namespaced
    // internal actor id.
    let source = r#"
(type mag.Task)
(type generic-provider.FinalAnswer)

(let [a (agent {:id "dup" :model "opus" :system "one" :max-steps 10}
          : mag.Task -> generic-provider.FinalAnswer)
      b (agent {:id "dup" :model "opus" :system "two" :max-steps 20}
          : generic-provider.FinalAnswer -> generic-provider.FinalAnswer)
      out (node "sink" {}
          : generic-provider.FinalAnswer -> generic-provider.FinalAnswer)]
  (graph
    a -> b
    b -> out
    :terminal out))
"#;
    let err = nefor_mag::compile(source, &fixtures_dir()).unwrap_err();
    assert!(
        err.to_string().contains("duplicate actor id") && err.to_string().contains("dup"),
        "expected a named collision error, got: {err}"
    );
}

#[test]
fn agent_profile_lowers_onto_its_llm_actors() {
    // The agent's :profile is the control plane's hook: the lead resolves it
    // and overlays provider/model/reasoning_effort keyed by the namespaced
    // llm actor ids, so :profile must land on exactly those actors' params.
    let source = r#"
(type mag.Task)
(type generic-provider.FinalAnswer)

(let [worker (agent {:id "worker" :system "Answer." :provider "chatgpt"
                     :profile "standard" :tools ["fs/read"] :max-steps 50}
               : mag.Task -> generic-provider.FinalAnswer)
      out (node "sink" {}
          : generic-provider.FinalAnswer -> generic-provider.FinalAnswer)]
  (graph worker -> out :terminal out))
"#;
    let ir = nefor_mag::compile(source, &fixtures_dir()).expect("agent program compiles");
    for id in ["worker.llm", "worker.exhaust"] {
        let actor = ir
            .actors
            .iter()
            .find(|a| a.id == id)
            .unwrap_or_else(|| panic!("no actor {id}"));
        assert_eq!(
            actor.params.get("profile").and_then(|v| v.as_str()),
            Some("standard"),
            "{id} carries the agent's :profile"
        );
    }
    // Non-llm internals stay unprofiled.
    let entry = ir.actors.iter().find(|a| a.id == "worker.entry").unwrap();
    assert!(entry.params.get("profile").is_none());
}

fn route_dests<'a>(ir: &'a nefor_mag::ir::ModificationIr, id: &str, key: &str) -> Vec<&'a str> {
    let actor = ir
        .actors
        .iter()
        .find(|a| a.id == id)
        .unwrap_or_else(|| panic!("no actor {id}"));
    actor
        .routes
        .get(key)
        .unwrap_or_else(|| panic!("actor {id} has no route {key}; routes: {:?}", actor.routes))
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect()
}

#[test]
fn agent_without_max_steps_expands_to_bare_loop() {
    // No :max-steps → no injected bound: the expansion is the bare cycle
    // (entry, llm, run-tool, tool-result) with a single output port on llm.
    // The typed union output on llm is the loop's terminator.
    let source = r#"
(type mag.Task)
(type generic-provider.FinalAnswer)

(let [worker (agent {:id "worker" :model "opus" :system "Answer." :provider "chatgpt"}
               : mag.Task -> generic-provider.FinalAnswer)
      out (node "sink" {}
          : generic-provider.FinalAnswer -> generic-provider.FinalAnswer)]
  (graph worker -> out :terminal out))
"#;
    let ir = nefor_mag::compile(source, &fixtures_dir()).expect("unbounded agent compiles");

    let mut ids: Vec<&str> = ir.actors.iter().map(|a| a.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![
            "sink",
            "worker.entry",
            "worker.llm",
            "worker.run-tool",
            "worker.tool-result",
        ],
        "no loop-counter, no exhaust"
    );

    // The bare cycle: llm -> run-tool -> tool-result -> llm.
    assert_eq!(
        route_dests(&ir, "worker.llm", "generic-tool.ToolCalls"),
        vec!["worker.run-tool"]
    );
    assert_eq!(
        route_dests(&ir, "worker.run-tool", "generic-tool.ToolHandle"),
        vec!["worker.tool-result"]
    );
    assert_eq!(
        route_dests(&ir, "worker.tool-result", "generic-provider.ProviderOut"),
        vec!["worker.llm"]
    );

    // Single output port: only llm routes the boundary type to the sink.
    assert_eq!(
        route_dests(&ir, "worker.llm", "generic-provider.FinalAnswer"),
        vec!["sink"]
    );
}

#[test]
fn agent_with_max_steps_expands_bounded() {
    // Authored :max-steps → the bounded expansion: loop-counter on the
    // back-edge with the authored max (no default), exhaust summarizer as a
    // second output port.
    let source = r#"
(type mag.Task)
(type generic-provider.FinalAnswer)

(let [worker (agent {:id "worker" :model "opus" :system "Answer." :provider "chatgpt"
                     :max-steps 7}
               : mag.Task -> generic-provider.FinalAnswer)
      out (node "sink" {}
          : generic-provider.FinalAnswer -> generic-provider.FinalAnswer)]
  (graph worker -> out :terminal out))
"#;
    let ir = nefor_mag::compile(source, &fixtures_dir()).expect("bounded agent compiles");

    let counter = ir
        .actors
        .iter()
        .find(|a| a.id == "worker.loop-counter")
        .expect("authored :max-steps lowers a loop-counter");
    assert_eq!(
        counter.params.get("max").and_then(|v| v.as_i64()),
        Some(7),
        "the counter carries the authored bound, not a default"
    );

    // The back-edge threads the counter; exhaustion exits to the summarizer.
    assert_eq!(
        route_dests(&ir, "worker.tool-result", "generic-provider.ProviderOut"),
        vec!["worker.loop-counter"]
    );
    assert_eq!(
        route_dests(&ir, "worker.loop-counter", "generic-provider.ProviderOut"),
        vec!["worker.llm"]
    );
    assert_eq!(
        route_dests(&ir, "worker.loop-counter", "mag.LoopExhausted"),
        vec!["worker.exhaust"]
    );

    // Both boundary output ports route to the sink.
    assert_eq!(
        route_dests(&ir, "worker.llm", "generic-provider.FinalAnswer"),
        vec!["sink"]
    );
    assert_eq!(
        route_dests(&ir, "worker.exhaust", "generic-provider.FinalAnswer"),
        vec!["sink"]
    );
}
