//! Composition-template lowering: `gate` from `starter/mag/lib/templates.mag`
//! is a MAG library function (built on the `subgraph` primitive), not a
//! compiler builtin. Each test compiles a small program that requires the
//! stdlib and instantiates the template, then asserts the whole program
//! validates and lowers.

use std::path::PathBuf;

/// The deployed stdlib lib lives under `starter/mag`; `require "lib/templates"`
/// resolves against this as the source dir.
fn stdlib_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../starter/mag")
}

fn actor_ids(ir: &nefor_mag::ir::ModificationIr) -> Vec<&str> {
    ir.actors.iter().map(|a| a.id.as_str()).collect()
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
fn gate_lowers_with_human_approval_exit() {
    let source = r#"
(let [tpl (require "lib/templates")
      g ((get tpl "gate") {:id "review"
                           :model "opus"
                           :provider "chatgpt-provider"})
      entry (node "adapter" {:seed "provider-in"}
              : mag.Task -> generic-provider.ProviderOut)
      out   (node "sink" {}
              : human.Approved -> human.Approved)]
  (graph
    entry -> g
    g -> out
    :terminal out))
"#;
    let ir = nefor_mag::compile(source, &stdlib_dir()).expect("gate program compiles");

    let ids = actor_ids(&ir);
    for want in ["review.produce", "review.approve", "review.revise"] {
        assert!(ids.contains(&want), "missing {want}; got {ids:?}");
    }

    // Rejection folds through revise, then back to produce — an unbounded
    // revision cycle whose terminator is the human's approval.
    assert_eq!(
        route_dests(&ir, "review.approve", "human.Rejected"),
        vec!["review.revise"]
    );
    assert_eq!(
        route_dests(&ir, "review.revise", "generic-provider.ProviderOut"),
        vec!["review.produce"]
    );

    // Approval leaves via the single boundary port to the sink.
    assert_eq!(
        route_dests(&ir, "review.approve", "human.Approved"),
        vec!["sink"]
    );
}

#[test]
fn colliding_template_ids_are_rejected() {
    // Two gate instances sharing an :id collide on every namespaced id.
    let source = r#"
(let [tpl (require "lib/templates")
      a ((get tpl "gate") {:id "dup" :model "opus" :provider "p" :system "one"})
      b ((get tpl "gate") {:id "dup" :model "opus" :provider "p" :system "two"})
      lift (node "adapter" {:seed "provider-in"}
            : human.Approved -> generic-provider.ProviderOut)
      out (node "sink" {}
            : human.Approved -> human.Approved)]
  (graph
    a -> lift
    lift -> b
    b -> out
    :terminal out))
"#;
    let err = nefor_mag::compile(source, &stdlib_dir()).unwrap_err();
    assert!(
        err.to_string().contains("duplicate actor id") && err.to_string().contains("dup"),
        "expected a named collision error, got: {err}"
    );
}
