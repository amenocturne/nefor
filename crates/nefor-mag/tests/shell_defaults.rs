//! Mag-as-shell compiler defaults: the `bash` capability node, inline node
//! expressions with auto-generated ids, infix pipe chains, the implicit
//! terminal, and bare-expression programs. Explicit authoring forms keep
//! working unchanged; the new defaults only fill in what wasn't written.

use std::path::PathBuf;

use nefor_mag::ir::ModificationIr;

fn compile(source: &str) -> Result<ModificationIr, nefor_mag::error::MagError> {
    nefor_mag::compile(source, &PathBuf::from("."))
}

fn actor<'a>(ir: &'a ModificationIr, id: &str) -> &'a nefor_mag::ir::ActorIr {
    ir.actors
        .iter()
        .find(|a| a.id == id)
        .unwrap_or_else(|| panic!("no actor '{id}'; got {:?}", actor_ids(ir)))
}

fn actor_ids(ir: &ModificationIr) -> Vec<&str> {
    ir.actors.iter().map(|a| a.id.as_str()).collect()
}

fn dests<'a>(ir: &'a ModificationIr, id: &str, key: &str) -> Vec<&'a str> {
    actor(ir, id)
        .routes
        .get(key)
        .unwrap_or_else(|| {
            panic!(
                "actor '{id}' has no route '{key}'; routes: {:?}",
                actor(ir, id).routes
            )
        })
        .as_array()
        .expect("route dests are an array")
        .iter()
        .map(|v| v.as_str().expect("dest is a string"))
        .collect()
}

// ---- bare expression = one-node program -------------------------------------

#[test]
fn bare_bash_expression_compiles_to_one_node_program() {
    let ir = compile(r#"(bash "ls")"#).expect("bare bash compiles");

    // One bash node plus the implicit canonical sink.
    assert_eq!(actor_ids(&ir), vec!["bash-1", "sink"]);

    let bash = actor(&ir, "bash-1");
    assert_eq!(bash.factory, "bash");
    assert_eq!(bash.params["command"].as_str(), Some("ls"));
    // Its stdout pipes into the implicit terminal.
    assert_eq!(dests(&ir, "bash-1", "mag.Text"), vec!["sink"]);
    // The sink is terminal: no routes downstream.
    assert!(actor(&ir, "sink").routes.is_empty());
    assert_eq!(actor(&ir, "sink").factory, "sink");
}

#[test]
fn bash_source_node_is_seeded_with_a_unit_activation() {
    let ir = compile(r#"(bash "ls")"#).expect("compiles");
    assert_eq!(ir.messages.len(), 1, "one initial activation");
    assert_eq!(ir.messages[0].to, "bash-1");
    assert_eq!(
        ir.messages[0].content["kind"].as_str(),
        Some("mag.Unit"),
        "a Unit-fired bash runs the command with no stdin"
    );
}

// ---- pipes compose -----------------------------------------------------------

#[test]
fn pipe_chain_gets_implicit_ids_and_terminal() {
    let ir = compile(r#"((bash "rg foo") -> (bash "sort"))"#).expect("chain compiles");

    assert_eq!(actor_ids(&ir), vec!["bash-1", "bash-2", "sink"]);
    // Appearance-order ids carry the pipe: bash-1's stdout is bash-2's stdin.
    assert_eq!(dests(&ir, "bash-1", "mag.Text"), vec!["bash-2"]);
    assert_eq!(dests(&ir, "bash-2", "mag.Text"), vec!["sink"]);
    // Only the chain head fires from the initial activation.
    assert_eq!(ir.messages.len(), 1);
    assert_eq!(ir.messages[0].to, "bash-1");
    assert_eq!(ir.messages[0].content["kind"].as_str(), Some("mag.Unit"));
}

#[test]
fn chain_composes_inside_a_graph_form() {
    let ir = compile(
        r#"
(let [out (node "sink" {} : mag.Text -> mag.Text)]
  (graph ((bash "rg foo") -> (bash "sort")) -> out :terminal out))
"#,
    )
    .expect("chain inside graph compiles");
    assert_eq!(actor_ids(&ir), vec!["bash-1", "bash-2", "sink"]);
    assert_eq!(dests(&ir, "bash-2", "mag.Text"), vec!["sink"]);
}

// ---- id determinism ----------------------------------------------------------

#[test]
fn inline_ids_are_deterministic_across_compiles() {
    let source = r#"((bash "a") -> (bash "b") -> (bash "c"))"#;
    let first = compile(source).expect("first compile");
    let second = compile(source).expect("second compile");
    assert_eq!(actor_ids(&first), actor_ids(&second));
    assert_eq!(
        first.hash, second.hash,
        "identical source yields an identical modification hash"
    );
    assert_eq!(
        actor_ids(&first),
        vec!["bash-1", "bash-2", "bash-3", "sink"]
    );
}

#[test]
fn binding_a_bash_node_renames_it_like_any_node() {
    let ir = compile(
        r#"
(let [list (bash "ls")]
  (list -> (bash "sort")))
"#,
    )
    .expect("bound bash compiles");
    // The let binding wins over the auto id; the unbound node keeps its
    // appearance-order id.
    assert_eq!(actor_ids(&ir), vec!["list", "bash-2", "sink"]);
    assert_eq!(dests(&ir, "list", "mag.Text"), vec!["bash-2"]);
}

// ---- implicit terminal on graph forms -----------------------------------------

#[test]
fn graph_without_terminal_or_sink_defaults_to_the_last_node() {
    let ir = compile(
        r#"
(let [a (bash "rg foo")
      b (bash "sort")]
  (graph a -> b))
"#,
    )
    .expect("sink-less graph compiles via the implicit terminal");
    assert_eq!(actor_ids(&ir), vec!["a", "b", "sink"]);
    assert_eq!(dests(&ir, "b", "mag.Text"), vec!["sink"]);
    assert!(actor(&ir, "sink").routes.is_empty());
}

#[test]
fn explicit_terminal_and_sink_autodetect_stay_unchanged() {
    // Explicit :terminal.
    let explicit = compile(
        r#"
(let [a (bash "ls")
      out (node "sink" {} : mag.Text -> mag.Text)]
  (graph a -> out :terminal out))
"#,
    )
    .expect("explicit terminal compiles");
    assert_eq!(actor_ids(&explicit), vec!["a", "sink"]);

    // Sink auto-detect without :terminal.
    let detected = compile(
        r#"
(let [a (bash "ls")
      out (node "sink" {} : mag.Text -> mag.Text)]
  (graph a -> out))
"#,
    )
    .expect("sink auto-detect compiles");
    assert_eq!(actor_ids(&detected), vec!["a", "sink"]);
    assert_eq!(explicit.hash, detected.hash, "same program either way");
}

// ---- type checking across `->` -------------------------------------------------

#[test]
fn type_mismatch_across_pipe_still_rejects() {
    // FooData is neither mag.Unit nor mag.Text, so it cannot pipe into bash.
    let err = compile(
        r#"
(let [x (node "stub" {} : mag.Task -> mag.FooData)]
  (x -> (bash "sort")))
"#,
    )
    .expect_err("incompatible pipe must reject");
    let msg = err.to_string();
    assert!(
        msg.contains("type") || msg.contains("mismatch") || msg.contains("compatible"),
        "the rejection names the type problem; got: {msg}"
    );
}

// ---- bash argument validation ---------------------------------------------------

#[test]
fn bash_requires_one_nonempty_string_command() {
    assert!(compile("(bash)").is_err(), "no command rejects");
    assert!(compile(r#"(bash "")"#).is_err(), "empty command rejects");
    assert!(compile("(bash 42)").is_err(), "non-string command rejects");
    assert!(
        compile(r#"(bash "a" "b")"#).is_err(),
        "two arguments reject"
    );
}

#[test]
fn bash_command_is_an_evaluated_expression() {
    let ir = compile(r#"(bash (str "rg " "foo"))"#).expect("computed command compiles");
    assert_eq!(
        actor(&ir, "bash-1").params["command"].as_str(),
        Some("rg foo")
    );
}
