//! Composition-template lowering: `gate` and `retry-bounded` from
//! `starter/mag/lib/templates.mag` are MAG library functions (built on the
//! `subgraph` primitive), not compiler builtins. Each test compiles a small
//! program that requires the stdlib and instantiates a template, then asserts
//! the whole program validates and lowers — including the bounded-loop check
//! passing through the template's internal loop-counter.

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
fn retry_bounded_lowers_and_bounds_its_loop() {
    let source = r#"
(let [tpl (require "lib/templates")
      retry ((get tpl "retry-bounded") {:id "fixer"
                                        :model "opus"
                                        :provider "chatgpt-provider"
                                        :max-steps 10})
      entry (node "adapter" {:seed "provider-in"}
              : mag.Task -> generic-provider.ProviderOut)
      out   (node "sink" {}
              : generic-provider.FinalAnswer -> generic-provider.FinalAnswer)]
  (graph
    entry -> retry
    retry -> out
    :terminal out))
"#;
    let ir = nefor_mag::compile(source, &stdlib_dir()).expect("retry-bounded program compiles");

    // The template namespaced every internal id under :id.
    let ids = actor_ids(&ir);
    for want in [
        "fixer.produce",
        "fixer.bound",
        "fixer.repair",
        "fixer.exhaust",
    ] {
        assert!(ids.contains(&want), "missing {want}; got {ids:?}");
    }

    // Bounded cycle: the loop-counter routes the failure back to repair and the
    // exhausted variant out. (That this compiled at all means validate_bounded_loops
    // accepted the cycle — it passes through fixer.bound.)
    assert_eq!(
        route_dests(&ir, "fixer.bound", "generic-control.Fail"),
        vec!["fixer.repair"]
    );
    assert_eq!(
        route_dests(&ir, "fixer.bound", "mag.LoopExhausted"),
        vec!["fixer.exhaust"]
    );
    assert_eq!(
        route_dests(&ir, "fixer.repair", "generic-provider.ProviderOut"),
        vec!["fixer.produce"]
    );

    // Both boundary output ports (produce's happy path, exhaust) route to sink.
    assert_eq!(
        route_dests(&ir, "fixer.produce", "generic-provider.FinalAnswer"),
        vec!["sink"]
    );
    assert_eq!(
        route_dests(&ir, "fixer.exhaust", "generic-provider.FinalAnswer"),
        vec!["sink"]
    );
}

#[test]
fn gate_lowers_with_human_approval_exit() {
    let source = r#"
(let [tpl (require "lib/templates")
      g ((get tpl "gate") {:id "review"
                           :model "opus"
                           :provider "chatgpt-provider"
                           :max-steps 5})
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
    for want in [
        "review.produce",
        "review.approve",
        "review.bound",
        "review.revise",
        "review.exhaust",
    ] {
        assert!(ids.contains(&want), "missing {want}; got {ids:?}");
    }

    // Rejection folds through the loop-counter into revise, then back to produce.
    assert_eq!(
        route_dests(&ir, "review.approve", "human.Rejected"),
        vec!["review.bound"]
    );
    assert_eq!(
        route_dests(&ir, "review.bound", "human.Rejected"),
        vec!["review.revise"]
    );
    assert_eq!(
        route_dests(&ir, "review.revise", "generic-provider.ProviderOut"),
        vec!["review.produce"]
    );

    // Approval leaves via the boundary port to the sink.
    assert_eq!(
        route_dests(&ir, "review.approve", "human.Approved"),
        vec!["sink"]
    );
    assert_eq!(
        route_dests(&ir, "review.exhaust", "human.Approved"),
        vec!["sink"]
    );
}

#[test]
fn colliding_template_ids_are_rejected() {
    // Two retry-bounded instances sharing an :id collide on every namespaced id.
    let source = r#"
(let [tpl (require "lib/templates")
      a ((get tpl "retry-bounded") {:id "dup" :model "opus" :provider "p" :max-steps 3})
      b ((get tpl "retry-bounded") {:id "dup" :model "opus" :provider "p" :max-steps 4})
      out (node "sink" {}
            : generic-provider.FinalAnswer -> generic-provider.FinalAnswer)]
  (graph
    a -> b
    b -> out
    :terminal out))
"#;
    let err = nefor_mag::compile(source, &stdlib_dir()).unwrap_err();
    assert!(
        err.to_string().contains("duplicate actor id") && err.to_string().contains("dup"),
        "expected a named collision error, got: {err}"
    );
}
