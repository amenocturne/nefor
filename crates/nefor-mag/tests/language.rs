use nefor_mag::{compile, load_with_inputs, load_with_inputs_and_module_roots};
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
fn explicit_record_refinement_rejects_extra_fields() {
    let root = workspace("exact-refinement");
    let error = compile(
        "(type User {:name String})\n(artifact \"test.exact/v1\" (as User {:name \"Ada\" :admin true}))",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("does not conform"), "{error}");
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
    assert_eq!(artifact.data, json!("main.Payload"));

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
