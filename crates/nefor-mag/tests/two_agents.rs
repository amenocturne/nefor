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
