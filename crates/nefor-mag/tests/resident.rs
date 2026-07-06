//! Resident-evaluator API: env retention + cache identity by hash, rule-fn
//! evaluation (happy path + JSON roundtrip through a real eval), and clean
//! budget exhaustion on a non-terminating rule fn.

use std::path::{Path, PathBuf};

/// A loadable program: two `def`s in the retained top scope (a rule fn that
/// builds a modification embedding its argument, and a non-terminating fn) plus
/// a minimal valid graph as the program's value.
const PROGRAM: &str = r#"
(type mag.Task)
(type mag.Done)

(def make-mod
  (fn [output]
    {:actors [{:id "spawned" :factory "worker" :params {:seed output} :routes {}}]
     :messages [{:to "spawned" :content {:kind "go"}}]
     :kills []
     :rules []}))

;; Omega combinator: unary in x, but its body diverges via self-application
;; through parameter binding (closures snapshot at def, so this is how MAG
;; expresses unbounded recursion at all).
(def spin
  (fn [x] ((fn [f] (f f)) (fn [f] (f f)))))

(let [entry (node "adapter" {} : mag.Task -> mag.Done)
      out   (node "sink" {} : mag.Done -> mag.Done)]
  (graph
    entry -> out
    :terminal out))
"#;

fn write_program(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mag-resident-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("prog.mag"), PROGRAM).expect("write program");
    dir
}

fn load(dir: &Path) -> nefor_mag::LoadedProgram {
    nefor_mag::load(dir, "prog.mag").expect("program should load")
}

#[test]
fn load_retains_env_and_produces_initial_modification() {
    let dir = write_program("retain");
    let program = load(&dir);
    // Initial modification: entry is a source, so it gets an activation; the
    // terminal lowers to the `sink` actor.
    assert!(program.modification.actors.iter().any(|a| a.id == "sink"));
    assert!(!program.modification.messages.is_empty());
    assert!(program.hash.starts_with("sha256:"));
    // Env is retained: a resident rule fn is resolvable and applies.
    let out = nefor_mag::eval_fn(&program, "make-mod", serde_json::json!({}))
        .expect("resident env retained");
    assert_eq!(out.actors[0].id, "spawned");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn same_snapshot_loads_are_identical_by_hash() {
    let dir = write_program("identity");
    let a = load(&dir);
    let b = load(&dir);
    // Purity makes the env — and therefore the modification — an exact function
    // of the source snapshot.
    assert_eq!(a.hash, b.hash);
    assert_eq!(a.modification.hash, b.modification.hash);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn eval_fn_applies_rule_and_roundtrips_input_json() {
    let dir = write_program("eval");
    let program = load(&dir);
    let input = serde_json::json!({ "summary": "hi", "n": 3, "tags": ["a", "b"] });
    let modification =
        nefor_mag::eval_fn(&program, "make-mod", input.clone()).expect("rule fn evaluates");

    // Rule produced a well-formed modification…
    assert_eq!(modification.actors.len(), 1);
    assert_eq!(modification.actors[0].id, "spawned");
    assert_eq!(modification.actors[0].factory, "worker");
    assert_eq!(modification.messages[0].to, "spawned");
    // …and the JSON input round-tripped intact through Value and back
    // (json_to_value on the way in, value_to_json on the way out).
    assert_eq!(modification.actors[0].params["seed"], input);
    // A fire-time modification gets its canonical hash stamped.
    assert!(modification.hash.starts_with("sha256:"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn non_terminating_rule_fn_hits_budget_cleanly() {
    let dir = write_program("budget");
    let program = load(&dir);
    let err = nefor_mag::eval_fn(&program, "spin", serde_json::json!(1))
        .expect_err("divergent fn must error, not hang or crash");
    assert!(
        matches!(err, nefor_mag::error::MagError::Budget(_)),
        "expected a clean budget error, got: {err}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unknown_rule_fn_is_an_error() {
    let dir = write_program("unknown");
    let program = load(&dir);
    let err = nefor_mag::eval_fn(&program, "does-not-exist", serde_json::json!({}))
        .expect_err("unknown fn name must error");
    assert!(err.to_string().contains("does-not-exist"), "got: {err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A rule fn whose body reads a workspace template at fire time. The template
/// exists only under the program's `source_dir`, never in the process cwd — so
/// a successful read proves the fire-time env resolves `read` against the
/// loaded program's `source_dir`, not the process cwd.
const READ_PROGRAM: &str = r#"
(type mag.Task)
(type mag.Done)

(def make-mod
  (fn [output]
    {:actors [{:id "spawned" :factory "worker"
               :params {:tmpl (read "templates/greet.md")}
               :routes {}}]
     :messages [{:to "spawned" :content {:kind "go"}}]
     :kills []
     :rules []}))

(let [entry (node "adapter" {} : mag.Task -> mag.Done)
      out   (node "sink" {} : mag.Done -> mag.Done)]
  (graph
    entry -> out
    :terminal out))
"#;

#[test]
fn eval_fn_read_resolves_against_source_dir() {
    let dir = std::env::temp_dir().join(format!("mag-resident-read-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("templates")).expect("mkdir");
    std::fs::write(dir.join("prog.mag"), READ_PROGRAM).expect("write program");
    std::fs::write(dir.join("templates/greet.md"), "hello from workspace").expect("write template");

    let program = nefor_mag::load(&dir, "prog.mag").expect("program should load");
    let modification = nefor_mag::eval_fn(&program, "make-mod", serde_json::json!({}))
        .expect("rule fn must read the workspace template at fire time");
    assert_eq!(
        modification.actors[0].params["tmpl"], "hello from workspace",
        "read inside a fire-time rule fn must resolve against the program source_dir"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn load_rejects_entry_escaping_the_workspace() {
    let dir = write_program("entry-escape");
    let abs = nefor_mag::load(&dir, "/etc/passwd")
        .err()
        .expect("an absolute entry must be rejected");
    assert!(
        abs.to_string().contains("absolute"),
        "expected absolute-path rejection, got: {abs}"
    );
    let up = nefor_mag::load(&dir, "../prog.mag")
        .err()
        .expect("a `..`-escaping entry must be rejected");
    assert!(
        up.to_string().contains("path traversal"),
        "expected traversal rejection, got: {up}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
