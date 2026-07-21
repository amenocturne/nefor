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
      (def emit (fn [[data Data]] -> Artifact (artifact "test.typed-artifact/v1" data)))
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
fn nominal_types_and_generic_foreigns_are_declared_data() {
    let root = workspace("types");
    let source = r#"
      (type Payload {:text String})
      (type Outcome [T] (| T Unit))
      (foreign nefor.factory.worker [T]
        {:params (Map String Data) :input T :output (Outcome T)})
      (artifact "test.types/v1" {:payload (str Payload) :foreign (str nefor.factory.worker)})
    "#;
    let artifact = compile(source, &root).unwrap();
    assert_eq!(artifact.data["payload"], "main.Payload");
    assert_eq!(artifact.data["foreign"], "nefor.factory.worker");
}

#[test]
fn immutable_inputs_are_visible_to_programs() {
    let root = workspace("inputs");
    fs::write(
        root.join("core/input.mag"),
        "(def contracts (get inputs :foreign_contracts))",
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
        json!({"foreign_contracts":[{"name":"x"}]}),
    )
    .unwrap();
    assert_eq!(loaded.artifact.data, json!([{"name":"x"}]));
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
fn as_is_an_explicit_checked_external_data_boundary() {
    let root = workspace("as-boundary");
    fs::write(root.join("main.mag"), "(type Contract {:identity String})\n(artifact \"test.as/v1\" (as (List Contract) (get inputs :contracts)))").unwrap();
    let good = load_with_inputs(
        &root,
        "main.mag",
        json!({"contracts":[{"identity":"worker"}]}),
    )
    .unwrap();
    assert_eq!(good.artifact.data, json!([{"identity":"worker"}]));
    let bad = load_with_inputs(&root, "main.mag", json!({"contracts":[{"missing":true}]}))
        .unwrap_err()
        .to_string();
    assert!(bad.contains("does not conform"), "{bad}");
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
fn record_literals_satisfy_homogeneous_string_maps() {
    let root = workspace("record-map");
    let source = r#"
      (def accept-data-map (fn [[value (Map String Data)]] -> Int (count value)))
      (artifact "test.record-map/v1"
        {:count (accept-data-map (as (Map String Data) {:kind "task" :prompt "Audit"}))})
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
fn explicit_record_refinement_reports_missing_and_unexpected_fields() {
    let root = workspace("exact-refinement");
    let error = compile(
        "(type BashOptions {:timeout_ms Data})\n(artifact \"test.exact/v1\" (as BashOptions {:timeout-ms 30000}))",
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
                contracts (map nefor.graph.foreign-contract
                                (as (List Data)
                                  (get (as (Map String Data) inputs)
                                       "foreign_contracts")))
                checked (nefor.graph.validate topology contracts)]
            (artifact "test.product-fan-in/v1" {:tag (get checked "tag")}))
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
    assert_eq!(artifact.data["tag"], "core.validated.Valid");
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
        json!({"kind":"named","name":"main.Payload","arguments":[]})
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
fn type_schema_rejects_non_data_and_non_string_map_keys() {
    let root = workspace("type-schema-errors");
    for (ty, expected) in [
        ("(Fn String String)", "Fn is not representable"),
        ("Artifact", "Artifact is not representable"),
        ("(TypeTag String)", "TypeTag is not representable"),
        ("(Map Int String)", "JSON object keys must be String"),
    ] {
        let source = format!("(artifact \"test.schema/v1\" (type-schema (type-tag {ty})))");
        let error = compile(&source, &root).unwrap_err().to_string();
        assert!(error.contains(expected), "{ty}: {error}");
    }
}
