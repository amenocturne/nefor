use nefor_mag::{
    compile, eval_fn, load_with_inputs, load_with_inputs_and_module_roots, validate_rule_fn,
    validate_rule_fn_input,
};
use serde_json::json;
use std::fs;

fn workspace(name: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("nefor-mag-language-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(path.join("core")).unwrap();
    path
}

#[test]
fn artifact_is_the_only_top_level_output() {
    let root = workspace("artifact");
    let artifact = compile(r#"(artifact "nefor.graph/v1" {:answer 42})"#, &root).unwrap();
    assert_eq!(artifact.format, "nefor.graph/v1");
    assert_eq!(artifact.data, json!({"answer":42}));
    assert!(compile("42", &root)
        .unwrap_err()
        .to_string()
        .contains("must return Artifact"));
}

#[test]
fn typed_library_functions_return_artifacts() {
    let root = workspace("typed-artifact");
    let source = r#"
      (def emit (fn [T] [[value T]] -> Artifact
        (artifact "test.typed-artifact/v1" value)))
      (emit {:answer 42})
    "#;
    let artifact = compile(source, &root).unwrap();
    assert_eq!(artifact.format, "test.typed-artifact/v1");
    assert_eq!(artifact.data, json!({"answer":42}));
}

#[test]
fn rule_functions_are_named_unary_artifact_functions() {
    let root = workspace("rule-functions");
    fs::write(
        root.join("main.mag"),
        r#"
          (def expand (fn [[value String]] -> Artifact
            (artifact "nefor.graph-delta/v1" {:value value})))
          (def binary (fn [[left String] [right String]] -> Artifact
            (artifact "nefor.graph-delta/v1" {:left left :right right})))
          (def wrong-output (fn [[value String]] -> String value))
          (type Task {:task String :description String})
          (def expand-tasks (fn [[tasks (List Task)]] -> Artifact
            (artifact "nefor.graph-delta/v1"
              {:names (map (fn [[task Task]] -> String (get task "task")) tasks)})))
          (artifact "test.rules/v1" {})
        "#,
    )
    .unwrap();
    let loaded = load_with_inputs(&root, "main.mag", json!({})).unwrap();
    validate_rule_fn(&loaded, "expand").unwrap();
    let string = serde_json::json!({"kind":"primitive","name":"String"});
    let int = serde_json::json!({"kind":"primitive","name":"Int"});
    validate_rule_fn_input(&loaded, "expand", &string).unwrap();
    assert!(validate_rule_fn_input(&loaded, "expand", &int)
        .unwrap_err()
        .to_string()
        .contains("does not match its structural source type"));
    assert!(validate_rule_fn(&loaded, "binary")
        .unwrap_err()
        .to_string()
        .contains("must be unary"));
    assert!(validate_rule_fn(&loaded, "wrong-output")
        .unwrap_err()
        .to_string()
        .contains("must return Artifact"));
    assert!(validate_rule_fn(&loaded, "missing")
        .unwrap_err()
        .to_string()
        .contains("unresolved symbol"));
    let artifact = eval_fn(
        &loaded,
        "expand-tasks",
        json!([{"task":"one", "description":"first"}]),
    )
    .unwrap();
    assert_eq!(artifact.data["names"], json!(["one"]));
    let mismatch = eval_fn(
        &loaded,
        "expand-tasks",
        json!([{"task":"one", "description":7}]),
    )
    .unwrap_err()
    .to_string();
    assert!(mismatch.contains("$[0].description"), "{mismatch}");
}

#[test]
fn namespaced_transitive_and_diamond_imports_are_stable() {
    let root = workspace("modules");
    fs::create_dir_all(root.join("app")).unwrap();
    fs::write(
        root.join("core/types.mag"),
        "(type Validated [E T] (| E T))\n(def marker 7)",
    )
    .unwrap();
    fs::write(
        root.join("app/left.mag"),
        "(require \"core.types\")\n(def left core.types.marker)",
    )
    .unwrap();
    fs::write(
        root.join("app/right.mag"),
        "(require \"core.types\")\n(def right core.types.marker)",
    )
    .unwrap();
    fs::write(root.join("main.mag"),"(require \"app.left\")\n(require \"app.right\")\n(artifact \"test.modules/v1\" {:left app.left.left :right app.right.right :type (str core.types.Validated)})").unwrap();
    let loaded = load_with_inputs(&root, "main.mag", json!({})).unwrap();
    assert_eq!(loaded.artifact.data["left"], 7);
    assert_eq!(loaded.artifact.data["right"], 7);
    assert_eq!(loaded.artifact.data["type"], "core.types.Validated");
}

#[test]
fn qualified_nominal_constructors_do_not_duck_type() {
    let root = workspace("qualified-nominals");
    fs::create_dir_all(root.join("left")).unwrap();
    fs::create_dir_all(root.join("right")).unwrap();
    fs::write(root.join("left/types.mag"), "(type Payload {:value Int})").unwrap();
    fs::write(root.join("right/types.mag"), "(type Payload {:value Int})").unwrap();
    fs::write(
        root.join("main.mag"),
        r#"
          (require "left.types")
          (require "right.types")
          (def accept-left
            (fn [[value left.types.Payload]] -> left.types.Payload value))
          (artifact "test.nominal-identity/v1"
            (accept-left (as right.types.Payload {:value 1})))
        "#,
    )
    .unwrap();

    let error = load_with_inputs(&root, "main.mag", json!({}))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("left.types.Payload") && error.contains("right.types.Payload"),
        "same-shaped constructors from different modules must remain distinct: {error}"
    );
}

#[test]
fn product_type_evidence_preserves_order_and_grouping() {
    let root = workspace("product-grouping");
    let artifact = compile(
        r#"
          (artifact "test.product-grouping/v1"
            {:left (type-evidence (type-tag (+ (+ Int String) Bool)))
             :right (type-evidence (type-tag (+ Int (+ String Bool))))
             :flat (type-evidence (type-tag (+ Int String Bool)))})
        "#,
        &root,
    )
    .unwrap();

    let left = json!({"kind":"product","items":[
        {"kind":"product","items":[
            {"kind":"primitive","name":"Int"},
            {"kind":"primitive","name":"String"}
        ]},
        {"kind":"primitive","name":"Bool"}
    ]});
    let right = json!({"kind":"product","items":[
        {"kind":"primitive","name":"Int"},
        {"kind":"product","items":[
            {"kind":"primitive","name":"String"},
            {"kind":"primitive","name":"Bool"}
        ]}
    ]});
    let flat = json!({"kind":"product","items":[
        {"kind":"primitive","name":"Int"},
        {"kind":"primitive","name":"String"},
        {"kind":"primitive","name":"Bool"}
    ]});
    assert_eq!(artifact.data["left"], left);
    assert_eq!(artifact.data["right"], right);
    assert_eq!(artifact.data["flat"], flat);
    assert_ne!(artifact.data["left"], artifact.data["right"]);
    assert_ne!(artifact.data["left"], artifact.data["flat"]);
    assert_ne!(artifact.data["right"], artifact.data["flat"]);
}

#[test]
fn nominal_types_and_generic_foreigns_are_precisely_declared() {
    let root = workspace("types");
    let source = r#"
      (type Payload {:text String})
      (type Outcome [T] (| T Unit))
      (foreign nefor.factory.worker [T]
        {:params (Map String String) :input T :output (Outcome T)})
      (artifact "test.types/v1" {:payload (str Payload) :foreign (str nefor.factory.worker)})
    "#;
    let artifact = compile(source, &root).unwrap();
    assert_eq!(artifact.data["payload"], "main.Payload");
    assert_eq!(artifact.data["foreign"], "nefor.factory.worker");
}

#[test]
fn immutable_host_inputs_expose_only_typed_projections() {
    let root = workspace("inputs");
    fs::write(
        root.join("core/input.mag"),
        "(def contracts (foreign-contracts))",
    )
    .unwrap();
    fs::write(
        root.join("main.mag"),
        "(require \"core.input\")\n(artifact \"test.inputs/v1\" core.input.contracts)",
    )
    .unwrap();
    let loaded = load_with_inputs(
        &root,
        "main.mag",
        json!({"foreign_contracts":[{
            "identity":"x",
            "implementation":"private",
            "params":{"heterogeneous": true},
            "type_scheme":{
                "variables":[],
                "inputs":{"wire":{"kind":"private"}},
                "input_tags":["in"],
                "outputs":["out"]
            }
        }]}),
    )
    .unwrap();
    assert_eq!(
        loaded.artifact.data,
        json!([{
            "identity":"x",
            "type_scheme":{"input_tags":["in"],"outputs":["out"]}
        }])
    );
}

#[test]
fn file_reads_are_immutable_for_the_loaded_program() {
    let root = workspace("file-read-snapshot");
    fs::write(root.join("value.txt"), "first").unwrap();
    fs::write(
        root.join("main.mag"),
        r#"
          (def initial (read "value.txt"))
          (def reread (fn [[ignored Unit]] -> Artifact
            (artifact "test.file-read/v1" (read "value.txt"))))
          (artifact "test.file-read/v1" initial)
        "#,
    )
    .unwrap();

    let loaded = load_with_inputs(&root, "main.mag", json!({})).unwrap();
    assert_eq!(loaded.artifact.data, json!("first"));

    fs::write(root.join("value.txt"), "second").unwrap();
    let reread = eval_fn(&loaded, "reread", serde_json::Value::Null).unwrap();
    assert_eq!(reread.data, json!("first"));

    let reloaded = load_with_inputs(&root, "main.mag", json!({})).unwrap();
    assert_eq!(reloaded.artifact.data, json!("second"));
}

#[test]
fn fail_preserves_library_diagnostics() {
    let root = workspace("failure");
    let error = compile("(fail {:kind \"Invalid\" :errors [\"bad route\"]})", &root)
        .unwrap_err()
        .to_string();
    assert!(error.contains("Invalid"), "{error}");
    assert!(error.contains("bad route"), "{error}");
}

#[test]
fn typed_generic_functions_construct_nominal_records() {
    let root = workspace("typed-functions");
    let source = r#"
      (type Box [T] {:value T})
      (def box (fn [T] [[value T]] -> (Box T) (as (Box T) {:value value})))
      (artifact "test.typed/v1" (box 42))
    "#;
    let artifact = compile(source, &root).unwrap();
    assert_eq!(artifact.data, json!({"value":42}));
}

#[test]
fn checker_rejects_bad_returns_and_calls() {
    let root = workspace("type-errors");
    let bad_return = compile(
        "(def wrong (fn [[value Int]] -> String value))\n(artifact \"test.bad/v1\" {})",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        bad_return.contains("returns Int, declared String"),
        "{bad_return}"
    );

    let bad_call = compile("(def only-int (fn [[value Int]] -> Int value))\n(artifact \"test.bad/v1\" (only-int \"no\"))", &root)
        .unwrap_err().to_string();
    assert!(bad_call.contains("expected Int, got String"), "{bad_call}");
}

#[test]
fn host_inputs_are_opaque_outside_typed_projections() {
    let root = workspace("opaque-inputs");
    fs::write(
        root.join("main.mag"),
        "(artifact \"test.inputs/v1\" (get inputs :contracts))",
    )
    .unwrap();
    let error = load_with_inputs(
        &root,
        "main.mag",
        json!({"contracts":[{"identity":"worker"}]}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("get expects a map"), "{error}");
}

#[test]
fn foreign_capabilities_are_first_class_and_specialized() {
    let root = workspace("foreign-capability");
    let source = r#"
      (type Params {:seed String})
      (type Input {:prompt String})
      (type Output {:answer String})
      (foreign runtime.worker [P I O] {:params P :input I :output O})
      (def capability-id
        (fn [P I O] [[capability (Foreign P I O)] [params P]] -> String
          (foreign-id capability)))
      (artifact "test.foreign/v1"
        {:identity (capability-id (specialize runtime.worker [Params Input Output]) (as Params {:seed "x"}))})
    "#;
    let artifact = compile(source, &root).unwrap();
    assert_eq!(artifact.data["identity"], "runtime.worker");

    let bad = source.replace("{:seed \"x\"}", "{:wrong \"x\"}");
    assert!(compile(&bad, &root).is_err());
}

#[test]
fn specialized_foreign_evidence_round_trips_exactly() {
    let root = workspace("foreign-evidence-roundtrip");
    let artifact = compile(
        r#"
          (type Params {:seed String})
          (type Input {:prompt String})
          (type Error {:message String})
          (type Output {:answer String})
          (foreign runtime.worker [P I O] {:params P :input I :output O})
          (artifact "test.foreign-evidence/v1"
            (foreign-evidence
              (specialize runtime.worker
                [Params (List Input) (| Output Error)])))
        "#,
        &root,
    )
    .unwrap();

    let primitive = |name: &str| json!({"kind":"primitive","name":name});
    let named = |name: &str, field: &str, ty: serde_json::Value| {
        json!({
            "kind":"named",
            "name":name,
            "arguments":[],
            "body":{"kind":"record","fields":[{"name":field,"type":ty}]}
        })
    };
    let params = named("main.Params", "seed", primitive("String"));
    let input = named("main.Input", "prompt", primitive("String"));
    let error = named("main.Error", "message", primitive("String"));
    let output = named("main.Output", "answer", primitive("String"));
    assert_eq!(
        artifact.data,
        json!({
            "version": 2,
            "identity": "runtime.worker",
            "arguments": [
                params,
                {"kind":"list","item":input},
                {"kind":"union","items":[error, output]}
            ],
            "input": {"kind":"list","item":input},
            "output": {"kind":"union","items":[error, output]}
        })
    );
}

#[test]
fn empty_lists_retain_expected_element_types_at_runtime() {
    let root = workspace("empty-list");
    let source = r#"
      (def accept-strings (fn [[items (List String)]] -> Int (count items)))
      (artifact "test.empty-list/v1" {:count (accept-strings (as (List String) []))})
    "#;
    let artifact = compile(source, &root).unwrap();
    assert_eq!(artifact.data["count"], 0);
}

#[test]
fn record_literals_satisfy_precise_homogeneous_string_maps() {
    let root = workspace("record-map");
    let source = r#"
      (def accept-string-map (fn [[value (Map String String)]] -> Int (count value)))
      (artifact "test.record-map/v1"
        {:count (accept-string-map
          (as (Map String String) {:kind "task" :prompt "Audit"}))})
    "#;
    let artifact = compile(source, &root).unwrap();
    assert_eq!(artifact.data["count"], 2);
}

#[test]
fn circular_modules_report_the_cycle() {
    let root = workspace("cycle");
    fs::write(root.join("core/a.mag"), "(require \"core.b\")").unwrap();
    fs::write(root.join("core/b.mag"), "(require \"core.a\")").unwrap();
    fs::write(
        root.join("main.mag"),
        "(require \"core.a\")\n(artifact \"test.cycle/v1\" {})",
    )
    .unwrap();
    let error = load_with_inputs(&root, "main.mag", json!({}))
        .unwrap_err()
        .to_string();
    assert!(error.contains("core.a -> core.b -> core.a"), "{error}");
}

#[test]
fn entry_and_module_search_roots_are_independent() {
    let root = workspace("module-roots");
    let entry_root = root.join("entry");
    let library_root = root.join("libraries");
    fs::create_dir_all(&entry_root).unwrap();
    fs::create_dir_all(library_root.join("core")).unwrap();
    fs::write(library_root.join("core/types.mag"), "(def marker 42)").unwrap();
    fs::write(
        entry_root.join("main.mag"),
        "(require \"core.types\")\n(artifact \"test.module-roots/v1\" {:marker core.types.marker})",
    )
    .unwrap();
    let loaded =
        load_with_inputs_and_module_roots(&entry_root, "main.mag", json!({}), &[library_root])
            .unwrap();
    assert_eq!(loaded.artifact.data["marker"], 42);
}

#[test]
fn duplicate_canonical_modules_across_roots_are_rejected() {
    let root = workspace("duplicate-modules");
    let entry = root.join("entry");
    let left = root.join("left");
    let right = root.join("right");
    fs::create_dir_all(&entry).unwrap();
    fs::create_dir_all(left.join("core")).unwrap();
    fs::create_dir_all(right.join("core")).unwrap();
    fs::write(left.join("core/types.mag"), "(def side \"left\")").unwrap();
    fs::write(right.join("core/types.mag"), "(def side \"right\")").unwrap();
    fs::write(
        entry.join("main.mag"),
        "(require \"core.types\")\n(artifact \"test.duplicate/v1\" {})",
    )
    .unwrap();
    let error = load_with_inputs_and_module_roots(&entry, "main.mag", json!({}), &[left, right])
        .unwrap_err()
        .to_string();
    assert!(error.contains("ambiguous across search roots"), "{error}");
}

#[test]
fn nominal_values_require_explicit_refinement() {
    let root = workspace("nominal-opacity");
    let implicit = compile(
        "(type User {:name String})\n(def user (fn [[name String]] -> User {:name name}))\n(artifact \"test.nominal/v1\" {})",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        implicit.contains("use as for explicit refinement"),
        "{implicit}"
    );

    let artifact = compile(
        "(type User {:name String})\n(def user (fn [[name String]] -> User (as User {:name name})))\n(artifact \"test.nominal/v1\" (user \"Ada\"))",
        &root,
    )
    .unwrap();
    assert_eq!(artifact.data, json!({"name":"Ada"}));
}

#[test]
fn sum_refinement_preserves_explicit_leaf_constructor_evidence() {
    let root = workspace("sum-constructor-evidence");
    let artifact = compile(
        r#"
          (type X {:value Int})
          (type Y {:value Int})
          (type XY (| X Y))
          (type Nested (| XY X))
          (def generic (fn [T] [[value T]] -> T value))
          (def x (as X {:value 1}))
          (def selected (as Nested (generic (as XY x))))
          (artifact "test.sum-evidence/v1" (as X selected))
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(artifact.data, json!({"value": 1}));
}

#[test]
fn sum_refinement_rejects_lookalikes_without_matching_constructor_evidence() {
    let root = workspace("sum-constructor-rejection");
    let untagged = compile(
        r#"
          (type X {:value Int})
          (type Y {:value Int})
          (artifact "test.sum-evidence/v1" (as (| X Y) {:value 1}))
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        untagged.contains("no explicit constructor evidence"),
        "{untagged}"
    );

    let lookalike = compile(
        r#"
          (type X {:value Int})
          (type Y {:value Int})
          (def selected (as (| X Y) (as X {:value 1})))
          (artifact "test.sum-evidence/v1" (as Y selected))
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        lookalike.contains("cannot replace constructor evidence"),
        "{lookalike}"
    );

    let invalid = compile(
        r#"
          (type X {:value Int})
          (type Y {:value Int})
          (type Z {:value Int})
          (artifact "test.sum-evidence/v1" (as (| X Y) (as Z {:value 1})))
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(invalid.contains("not accepted"), "{invalid}");
}

#[test]
fn product_values_are_exact_ordered_tuples_with_authored_grouping() {
    let root = workspace("product-values");
    let artifact = compile(
        r#"
          (artifact "test.product-values/v1"
            {:flat (as (+ Int String Int) [1 "middle" 2])
             :left (as (+ (+ Int String) Bool)
                       [(as (+ Int String) [3 "left"]) true])
             :right (as (+ Int (+ String Bool))
                        [4 (as (+ String Bool) ["right" false])])})
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(
        artifact.data,
        json!({
            "flat": [1, "middle", 2],
            "left": [[3, "left"], true],
            "right": [4, ["right", false]],
        })
    );

    for (name, source) in [
        (
            "short",
            r#"(artifact "test.product/v1" (as (+ Int String) [1]))"#,
        ),
        (
            "long",
            r#"(artifact "test.product/v1" (as (+ Int String) [1 "x" 2]))"#,
        ),
        (
            "positional",
            r#"(artifact "test.product/v1" (as (+ Int String) ["x" 1]))"#,
        ),
        (
            "old-intersection",
            r#"(artifact "test.product/v1" (as (+ Int Int) 1))"#,
        ),
    ] {
        let error = compile(source, &workspace(name)).unwrap_err().to_string();
        assert!(error.contains("does not conform"), "{name}: {error}");
    }
}

#[test]
fn product_positions_preserve_selected_constructor_evidence() {
    let root = workspace("product-constructor-evidence");
    let artifact = compile(
        r#"
          (type X {:value Int})
          (type Y {:value Int})
          (def tuple
            (as (+ (| X Y) String)
                [(as (| X Y) (as X {:value 1})) "kept"]))
          (def accept (fn [[value (+ (| X Y) String)]] -> (+ (| X Y) String) value))
          (artifact "test.product-evidence/v1" (accept tuple))
        "#,
        &root,
    )
    .unwrap();
    let envelope = artifact.data[0].as_object().unwrap();
    assert_eq!(envelope.len(), 2);
    assert!(envelope["type"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(envelope["value"], json!({"value": 1}));
    assert_eq!(artifact.data[1], "kept");
}

#[test]
fn explicit_record_refinement_reports_missing_and_unexpected_fields() {
    let root = workspace("exact-refinement");
    let error = compile(
        "(type BashOptions {:timeout_ms Int})\n(artifact \"test.exact/v1\" (as BashOptions {:timeout-ms 30000}))",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("does not conform"), "{error}");
    assert!(error.contains("missing fields: timeout_ms"), "{error}");
    assert!(error.contains("unexpected fields: timeout-ms"), "{error}");
}

#[test]
fn explicit_record_refinement_reports_field_diffs_for_generic_nominals() {
    let root = workspace("generic-exact-refinement");
    let error = compile(
        "(type Pair [T] {:first T :second T})\n(artifact \"test.exact/v1\" (as (Pair Int) {:first 1 :other 2}))",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("missing fields: second"), "{error}");
    assert!(error.contains("unexpected fields: other"), "{error}");
}

#[test]
fn recursive_evaluation_is_bounded() {
    let root = workspace("fuel");
    let error = compile(
        "(def loop (fn [[n Int]] -> Int (loop n)))\n(artifact \"test.fuel/v1\" (loop 0))",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("evaluation budget exceeded"), "{error}");
}

#[test]
fn repeated_pure_calls_fit_the_budget_by_reusing_results() {
    let root = workspace("memoized-budget");
    let calls = std::iter::repeat_n("(identity value)", 28_000)
        .collect::<Vec<_>>()
        .join(" ");
    let source = format!(
        "(def value 7)\n(def identity (fn [[item Int]] -> Int item))\n(artifact \"test.memoized-budget/v1\" [{calls}])"
    );

    let artifact = compile(&source, &root).unwrap();
    assert_eq!(artifact.data.as_array().unwrap().len(), 28_000);
}

#[test]
fn named_functions_keep_their_lexical_recursive_binding() {
    let root = workspace("lexical-recursion");
    let artifact = compile(
        r#"
          (def walk (fn [[items (List Int)]] -> Int
            (if (= (count items) 0)
              0
              (walk (remove-at items 0)))))
          (def saved walk)
          (def before (saved [1 2 3]))
          (def walk (fn [[items (List Int)]] -> Int 99))
          (artifact "test.lexical-recursion/v1"
            {:before before :after (saved [1 2 3])})
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(artifact.data, json!({"before": 0, "after": 0}));
}

#[test]
fn deeply_nested_non_function_expressions_are_bounded() {
    let root = workspace("expression-depth");
    let mut expression = String::from("\"value\"");
    for _ in 0..180 {
        expression = format!("(str {expression})");
    }
    let source = format!("(artifact \"test.depth/v1\" {expression})");
    let error = compile(&source, &root).unwrap_err().to_string();
    assert!(
        error.contains("expression nesting limit reached"),
        "{error}"
    );
}

#[test]
fn builtin_type_rules_are_total_and_assoc_checks_values() {
    let root = workspace("builtin-rules");
    let arity = compile("(artifact \"test.builtin/v1\" (map))", &root)
        .unwrap_err()
        .to_string();
    assert!(arity.contains("expected 2, got 0"), "{arity}");

    let mismatch = compile(
        "(def update (fn [[value {:count Int}]] -> {:count Int} (assoc value :count \"many\")))\n(artifact \"test.builtin/v1\" {})",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(mismatch.contains("expected Int, got String"), "{mismatch}");
}

#[test]
fn canonical_and_sort_by_are_typed_deterministic_builtins() {
    let root = workspace("canonical-sort-by");
    let artifact = compile(
        r#"
          (type Item {:id String :rank Int})
          (def items (as (List Item)
            [(as Item {:id "third" :rank 30})
             (as Item {:id "first" :rank 10})
             (as Item {:id "second" :rank 20})]))
          (def ordered
            (sort-by
              (fn [[item Item]] -> String (get item "id"))
              items))
          (artifact "test.canonical/v1"
            {:canonical (canonical {:nodes ["a" "b"]
                                    :meta {:z 2 :a 1}
                                    :kind "edge"})
             :removed (remove-at ["a" "b" "c"] 1)
             :string-ok (conforms? "value" (type-evidence (type-tag String)))
             :string-bad (conforms? 42 (type-evidence (type-tag String)))
             :item-ok (conforms? {:id "item" :rank 1}
                        (type-evidence (type-tag Item)))
             :ids (map (fn [[item Item]] -> String (get item "id")) ordered)})
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(
        artifact.data,
        json!({
            "canonical": "{\"kind\":\"edge\",\"meta\":{\"a\":1,\"z\":2},\"nodes\":[\"a\",\"b\"]}",
            "removed": ["a", "c"],
            "string-ok": true,
            "string-bad": false,
            "item-ok": true,
            "ids": ["first", "second", "third"]
        })
    );

    let bad_key = compile(
        "(artifact \"test.sort/v1\" (sort-by (fn [[value Int]] -> Int value) [2 1]))",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        bad_key.contains("sort-by callback must return String"),
        "{bad_key}"
    );

    let bad_arity = compile("(artifact \"test.canonical/v1\" (canonical 1 2))", &root)
        .unwrap_err()
        .to_string();
    assert!(bad_arity.contains("expected 1, got 2"), "{bad_arity}");
}

#[test]
fn graph_product_input_accepts_repeated_typed_fan_in() {
    let root = workspace("product-fan-in");
    let mag_lib =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../starter/mag/lib");
    fs::write(
        root.join("main.mag"),
        r#"
          (require "nefor.graph")
          (type CoveredLeft {:value Int})
          (type CoveredRight {:value String})
          (type CoveredChoice (| CoveredLeft CoveredRight))
          (foreign test.product [T]
            {:params Unit :input (+ T T) :output T})
          (let [left (nefor.graph.source
                       "left" (type-tag nefor.contracts.Text)
                       (as nefor.contracts.Text {:content "left"}))
                right (nefor.graph.source
                        "right" (type-tag nefor.contracts.Text)
                        (as nefor.contracts.Text {:content "right"}))
                join-input (nefor.graph.port
                             "join"
                             (type-tag (+ nefor.contracts.Text nefor.contracts.Text))
                             "test.Value")
                join-output (nefor.graph.port
                              "join" (type-tag nefor.contracts.Text) "test.Value")
                join-actor (nefor.graph.actor
                             "join" (specialize test.product [nefor.contracts.Text]) nil
                             (nefor.graph.store-port join-input)
                             [(nefor.graph.store-port join-output)])
                join (nefor.graph.node
                       "join" "ordinary" [join-actor]
                       (as (List nefor.graph.StoredRoute) [])
                       (as (List nefor.graph.Message) [])
                       join-input join-output)
                result (nefor.graph.output
                         "result" (type-tag nefor.contracts.Text))
                topology (nefor.graph.graph
                           [(nefor.graph.edge left join)
                            (nefor.graph.edge right join)
                            (nefor.graph.edge join result)])
                product-result (nefor.graph.output
                                 "product-result"
                                 (type-tag
                                   (+ nefor.contracts.Text
                                      nefor.contracts.Text)))
                output-topology (nefor.graph.graph
                                  [(nefor.graph.edge left product-result)
                                   (nefor.graph.edge right product-result)])
                reversed-output-topology (nefor.graph.graph
                                           [(nefor.graph.edge right product-result)
                                            (nefor.graph.edge left product-result)])
                choice-start
                  (nefor.graph.source
                    "choice-start"
                    (type-tag CoveredChoice)
                    (as CoveredChoice (as CoveredLeft {:value 1})))
                choice-result
                  (nefor.graph.output "choice-result" (type-tag CoveredLeft))
                choice-topology
                  (nefor.graph.graph
                    [(nefor.graph.edge choice-start choice-result)])
                choice-rule
                  (nefor.graph.rule
                    "observe-choice" (get choice-start "output") "observe-choice")
                contracts (map nefor.graph.foreign-contract
                                (foreign-contracts))
                checked (nefor.graph.validate topology contracts)
                output-checked
                  (nefor.graph.validate output-topology contracts)
                choice-without-rule
                  (nefor.graph.validate choice-topology contracts)
                choice-with-rule
                  (nefor.graph.validate-with-rules
                    choice-topology [choice-rule] contracts)
                lowered (nefor.graph.lower topology)
                lowered-left
                  (first (filter
                    (fn [[candidate nefor.graph.LowerActor]] -> Bool
                      (= (get candidate "id") "left"))
                    (get lowered "actors")))
                lowered-right
                  (first (filter
                    (fn [[candidate nefor.graph.LowerActor]] -> Bool
                      (= (get candidate "id") "right"))
                    (get lowered "actors")))
                destinations
                  (concat
                    (get (get lowered-left "routes") "nefor.graph.Value")
                    (get (get lowered-right "routes") "nefor.graph.Value"))]
            (artifact "test.product-fan-in/v1"
              {:tag (get checked "tag")
               :output-tag (get output-checked "tag")
               :choice-without-rule (get choice-without-rule "tag")
               :choice-with-rule (get choice-with-rule "tag")
               :permutation-stable
                 (= (canonical (nefor.graph.lower output-topology))
                    (canonical (nefor.graph.lower reversed-output-topology)))
               :positions
                 (map (fn [[destination nefor.graph.LowerDestination]] -> Int
                        (get destination "product_position"))
                      destinations)
               :edge-ids
                 (map (fn [[destination nefor.graph.LowerDestination]] -> String
                        (get destination "edge_id"))
                      destinations)}))
        "#,
    )
    .unwrap();
    let inputs = json!({
        "foreign_contracts": [
            {
                "identity": "nefor.factory.source",
                "type_scheme": {
                    "input_tags": ["mag.Unit"],
                    "outputs": ["nefor.graph.Value"]
                }
            },
            {
                "identity": "nefor.factory.output",
                "type_scheme": {
                    "input_tags": ["nefor.graph.Value"],
                    "outputs": ["nefor.graph.Value"]
                }
            },
            {
                "identity": "test.product",
                "type_scheme": {
                    "input_tags": ["test.Value"],
                    "outputs": ["test.Value"]
                }
            }
        ]
    });
    let artifact =
        load_with_inputs_and_module_roots(&root, "main.mag", inputs, &[root.clone(), mag_lib])
            .unwrap()
            .artifact;
    assert_eq!(
        artifact.data["tag"], "core.validated.Valid",
        "{:?}",
        artifact.data
    );
    assert_eq!(artifact.data["output-tag"], "core.validated.Valid");
    assert_eq!(
        artifact.data["choice-without-rule"],
        "core.validated.Invalid"
    );
    assert_eq!(artifact.data["choice-with-rule"], "core.validated.Valid");
    assert_eq!(artifact.data["permutation-stable"], true);
    assert_eq!(artifact.data["positions"], json!([0, 1]));
    let edge_ids = artifact.data["edge-ids"].as_array().unwrap();
    assert_eq!(edge_ids.len(), 2);
    assert_ne!(edge_ids[0], edge_ids[1]);
}

#[test]
fn graph_descriptor_operations_are_compiler_owned() {
    let root = workspace("graph-descriptors");
    let artifact = compile(
        r#"
          (type Left {:value Int})
          (type Right {:value String})
          (type Choice (| Left Right))
          (artifact "test.graph-descriptors/v1"
            {:arm (descriptor-accepts?
                    (type-evidence (type-tag Choice))
                    (type-evidence (type-tag Left)))
             :wrong-arm (descriptor-accepts?
                          (type-evidence (type-tag Left))
                          (type-evidence (type-tag Right)))
             :product (descriptor-input-covered-by?
                        (type-evidence (type-tag (+ Left Left)))
                        [(type-evidence (type-tag Left))
                         (type-evidence (type-tag Left))])
             :assignments (descriptor-input-assignments
                            (type-evidence (type-tag (+ Left Left)))
                            [(type-evidence (type-tag Left))
                             (type-evidence (type-tag (+ Left Left)))
                             (type-evidence (type-tag Left))])
             :covered-output (descriptor-output-covered-by?
                               (type-evidence (type-tag Choice))
                               [(type-evidence (type-tag Left))
                                (type-evidence (type-tag Right))])
             :uncovered-output (descriptor-output-covered-by?
                                 (type-evidence (type-tag Choice))
                                 [(type-evidence (type-tag Left))])})
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(
        artifact.data,
        json!({
            "arm": true,
            "wrong-arm": false,
            "product": true,
            "assignments": [0, -1, 1],
            "covered-output": true,
            "uncovered-output": false
        })
    );

    let forged = compile(
        r#"(artifact "test.graph-descriptors/v1"
             (as TypeDescriptor {:kind "primitive" :name "String"}))"#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(forged.contains("does not conform"), "{forged}");
}

#[test]
fn data_is_not_a_source_type_or_cast_target() {
    let root = workspace("removed-data");
    let declaration = compile(
        "(type Payload {:value Data})\n(artifact \"test.removed/v1\" {})",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        declaration.contains("unresolved symbol: Data"),
        "{declaration}"
    );

    let cast = compile("(artifact \"test.removed/v1\" (as Data {:value 1}))", &root)
        .unwrap_err()
        .to_string();
    assert!(cast.contains("unresolved symbol: Data"), "{cast}");
}

#[test]
fn union_unification_commits_substitutions_and_rejects_ambiguity() {
    let root = workspace("union-substitution");
    let artifact = compile(
        "(def select (fn [T] [[x (| T String)]] -> T (as T x)))\n(artifact \"test.union/v1\" (select 42))",
        &root,
    )
    .unwrap();
    assert_eq!(artifact.data, json!(42));

    let ambiguous = compile(
        "(def select (fn [T U] [[x (| T U)]] -> T (as T x)))\n(artifact \"test.union/v1\" (select 42))",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(ambiguous.contains("ambiguous union match"), "{ambiguous}");
}

#[test]
fn type_tags_are_typed_canonical_witnesses() {
    let root = workspace("type-tags");
    let artifact = compile(
        "(type Payload {:value Int})\n(def tag-of (fn [T] [[value T]] -> (TypeTag T) (type-tag T)))\n(artifact \"test.tag/v1\" (tag-of (as Payload {:value 1})))",
        &root,
    )
    .unwrap();
    assert_eq!(
        artifact.data,
        json!({
            "kind":"named",
            "name":"main.Payload",
            "arguments":[],
            "body":{
                "kind":"record",
                "fields":[{
                    "name":"value",
                    "type":{"kind":"primitive","name":"Int"}
                }]
            }
        })
    );

    let unknown = compile("(artifact \"test.tag/v1\" (type-tag Missing))", &root)
        .unwrap_err()
        .to_string();
    assert!(unknown.contains("Missing"), "{unknown}");
}

#[test]
fn duplicate_foreign_identities_are_rejected() {
    let root = workspace("duplicate-foreign");
    let error = compile(
        "(foreign runtime.worker {:params Unit :input Unit :output Unit})\n(foreign runtime.worker {:params Unit :input Unit :output Unit})\n(artifact \"test.foreign/v1\" {})",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("duplicate foreign declaration"), "{error}");
}

#[test]
fn nested_callbacks_capture_enclosing_generic_binders() {
    let root = workspace("nested-generic-binders");
    let artifact = compile(
        "(def contains? (fn [T] [[values (List T)] [value T]] -> Bool (= (count (filter (fn [[candidate T]] -> Bool (= candidate value)) values)) 1)))\n(artifact \"test.generics/v1\" (contains? [1 2] 1))",
        &root,
    )
    .unwrap();
    assert_eq!(artifact.data, json!(true));
}

#[test]
fn generic_calls_unify_arguments_inside_the_same_nominal_type() {
    let root = workspace("nominal-generic-unification");
    let artifact = compile(
        "(type Port [T] {:actor String})\n(def identity-port (fn [O] [[port (Port O)]] -> (Port O) port))\n(def forward-port (fn [T] [[port (Port T)]] -> (Port T) (identity-port port)))\n(artifact \"test.generics/v1\" (forward-port (as (Port Int) {:actor \"worker\"})))",
        &root,
    )
    .unwrap();
    assert_eq!(artifact.data, json!({"actor":"worker"}));
}

#[test]
fn type_schema_preserves_qualified_nominals_and_substitutes_generics() {
    let root = workspace("type-schema");
    fs::write(
        root.join("core/types.mag"),
        "(type Box [T] {:value T})\n(def schema (type-schema (type-tag (Box (List String)))))",
    )
    .unwrap();
    fs::write(
        root.join("main.mag"),
        "(require \"core.types\")\n(artifact \"test.schema/v1\" core.types.schema)",
    )
    .unwrap();
    let artifact = load_with_inputs(&root, "main.mag", json!({}))
        .unwrap()
        .artifact;
    assert_eq!(artifact.data["version"], 1);
    assert_eq!(artifact.data["root"]["kind"], "named");
    assert_eq!(artifact.data["root"]["name"], "core.types.Box");
    assert_eq!(
        artifact.data["root"]["body"]["fields"][0]["schema"]["kind"],
        "list"
    );
    assert_eq!(
        artifact.data["root"]["body"]["fields"][0]["schema"]["item"]["kind"],
        "string"
    );
}

#[test]
fn tagged_sum_json_round_trips_aliases_and_rejects_forged_envelopes() {
    let root = workspace("tagged-sum-json");
    fs::write(
        root.join("main.mag"),
        r#"
          (type X {:value Int})
          (type Y {:value Int})
          (type XY (| X Y))
          (type Alias XY)
          (type Nested {:choice Alias :pair (+ String Alias)})
          (def accept (fn [[value Nested]] -> Artifact
            (artifact "test.tagged-sum/v1" value)))
          (artifact "test.tagged-sum-schema/v1" (type-schema (type-tag Alias)))
        "#,
    )
    .unwrap();
    let loaded = load_with_inputs(&root, "main.mag", json!({})).unwrap();
    let variants = loaded.artifact.data["root"]["variants"].as_array().unwrap();
    assert_eq!(variants.len(), 2);
    let first = variants[0]["tag"].as_str().unwrap();
    let second = variants[1]["tag"].as_str().unwrap();
    assert_ne!(first, second);
    assert!(first.starts_with("sha256:"));
    assert!(second.starts_with("sha256:"));

    let input = json!({
        "choice": {"type": first, "value": {"value": 1}},
        "pair": ["kept", {"type": second, "value": {"value": 2}}]
    });
    let artifact = eval_fn(&loaded, "accept", input.clone()).unwrap();
    assert_eq!(artifact.data, input);

    for malformed in [
        json!({"choice": {"value": {"value": 1}}, "pair": ["kept", {"type": second, "value": {"value": 2}}]}),
        json!({"choice": {"type": first}, "pair": ["kept", {"type": second, "value": {"value": 2}}]}),
        json!({"choice": {"type": first, "value": {"value": 1}, "extra": true}, "pair": ["kept", {"type": second, "value": {"value": 2}}]}),
        json!({"choice": {"type": "sha256:forged", "value": {"value": 1}}, "pair": ["kept", {"type": second, "value": {"value": 2}}]}),
        json!({"choice": {"type": first, "value": {"value": "wrong"}}, "pair": ["kept", {"type": second, "value": {"value": 2}}]}),
        json!({"choice": {"value": 1}, "pair": ["kept", {"type": second, "value": {"value": 2}}]}),
    ] {
        assert!(eval_fn(&loaded, "accept", malformed).is_err());
    }
}

#[test]
fn type_schema_rejects_non_data_and_non_string_map_keys() {
    let root = workspace("type-schema-errors");
    for (ty, expected) in [
        (
            "(Fn String String)",
            "Fn cannot enter a concrete semantic descriptor",
        ),
        (
            "Artifact",
            "Artifact cannot enter a concrete semantic descriptor",
        ),
        (
            "(TypeTag String)",
            "TypeTag cannot enter a concrete semantic descriptor",
        ),
        ("(Map Int String)", "JSON object keys must be String"),
    ] {
        let source = format!("(artifact \"test.schema/v1\" (type-schema (type-tag {ty})))");
        let error = compile(&source, &root).unwrap_err().to_string();
        assert!(error.contains(expected), "{ty}: {error}");
    }
}
