use nefor_mag::{
    compile as compile_artifact, compile_file_with_inputs,
    compile_file_with_inputs_and_module_roots,
    compile_file_with_inputs_and_module_roots_and_options_and_syntax, compile_with_options,
    CompilerLimits, CompilerOptions, SyntaxMode,
};
use serde_json::json;
use std::fs;

fn compile(
    source: &str,
    source_dir: &std::path::Path,
) -> Result<serde_json::Value, nefor_mag::error::MagError> {
    compile_artifact(source, source_dir)
}

fn compile_lisp(
    source: &str,
    source_dir: &std::path::Path,
) -> Result<serde_json::Value, nefor_mag::error::MagError> {
    nefor_mag::compile_with_syntax(source, source_dir, SyntaxMode::Lisp)
}

fn workspace(name: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("nefor-mag-language-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(path.join("core")).unwrap();
    path
}

#[test]
fn nominal_adts_construct_match_and_serialize_by_constructor() {
    let root = workspace("nominal-adts");
    let artifact = compile(
        r#"
        type Result<E, A> = Error(E) | Ok(A)
let result = Result<String, Int>.Ok(42)
artifact {value: result, equal: (=)(result, Result<String, Int>.Ok(42)), different: (=)(result, Result<String, Int>.Error("42")), rendered: match result { case Error(error) => error, case Ok(answer) => str(answer) }}
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(
        artifact,
        json!({
            "value": {"constructor": "Ok", "value": 42},
            "equal": true,
            "different": false,
            "rendered": "42",
        })
    );
}

#[test]
fn nominal_adt_construction_checks_the_selected_payload_type() {
    let root = workspace("nominal-adt-payload");
    let error = compile(
        r#"
        type Choice = Number(Int)
artifact(Choice.Number("not an integer"))
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("expected Int, got String"), "{error}");
}

#[test]
fn type_declarations_reject_duplicate_generic_parameters() {
    let root = workspace("duplicate-type-generics");
    let error = compile("type Bad<T, T> = Wrap(T)\nartifact {}", &root)
        .unwrap_err()
        .to_string();
    assert!(error.contains("duplicate generic parameter T"), "{error}");

    let error = compile(
        "let identity<T, T>: fn(T) -> T = |value| => value\nartifact(identity(1))",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("duplicate generic parameter T"), "{error}");
}

#[test]
fn nominal_adt_descriptors_schemas_and_ids_include_owner_arguments() {
    let root = workspace("nominal-adt-evidence");
    let artifact = compile(
        r#"
        type Result<E, A> = Ok(A) | Error(E)
artifact {text: type_evidence(type_tag<Result<String, Int>>()), bool: type_evidence(type_tag<Result<Bool, Int>>()), text_id: type_id(type_evidence(type_tag<Result<String, Int>>())), bool_id: type_id(type_evidence(type_tag<Result<Bool, Int>>())), schema: type_schema(type_tag<Result<String, Int>>())}
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(artifact["text"]["kind"], "adt");
    assert_eq!(artifact["text"]["name"], "main.Result");
    assert_eq!(artifact["text"]["constructors"][0]["name"], "Error");
    assert_eq!(artifact["text"]["constructors"][1]["name"], "Ok");
    assert_ne!(artifact["text_id"], artifact["bool_id"]);
    assert_eq!(artifact["schema"]["version"], 2);
    assert_eq!(artifact["schema"]["root"]["kind"], "adt");
    assert_eq!(artifact["schema"]["root"]["name"], "main.Result");
    assert_eq!(artifact["schema"]["root"]["owner_id"], artifact["text_id"]);
}

#[test]
fn type_descriptor_operations_preserve_nested_arguments_aliases_and_schemas() {
    let root = workspace("descriptor-operations");
    let artifact = compile(
        r#"
        type Box<T> {value: T}
type Pair<Left, Right> {left: Left, right: Right}
type Choice = Text(String) | Count(Int)
type Alias<T> = Box<T>
let pair = type_evidence(type_tag<Pair<String, Box<Int>>>() )
let alias = type_evidence(type_tag<Alias<Bool>>())
let listed = list_type(alias)
artifact {
  constructor: type_constructor(pair),
  arguments: type_arguments(pair),
  alias_constructor: type_constructor(alias),
  alias_arguments: type_arguments(alias),
  primitive_constructor: type_constructor(type_evidence(type_tag<String>())),
  constructor_payload: adt_constructor_payload(type_evidence(type_tag<Choice>()), "Count"),
  primitive_arguments: type_arguments(type_evidence(type_tag<String>())),
  components: type_components(pair),
  list_components: type_components(listed),
  primitive_components: type_components(type_evidence(type_tag<String>())),
  listed: listed,
  descriptor_schema: descriptor_schema(listed),
  tag_schema: type_schema(type_tag<List<Box<Bool>>>()),
}
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(artifact["constructor"], "main.Pair");
    assert_eq!(artifact["arguments"][0]["name"], "String");
    assert_eq!(artifact["arguments"][1]["name"], "main.Box");
    assert_eq!(artifact["arguments"][1]["arguments"][0]["name"], "Int");
    assert_eq!(artifact["alias_constructor"], "main.Box");
    assert_eq!(artifact["constructor_payload"]["name"], "Int");
    assert_eq!(artifact["alias_arguments"][0]["name"], "Bool");
    assert_eq!(artifact["primitive_constructor"], "");
    assert_eq!(artifact["primitive_arguments"], json!([]));
    assert_eq!(artifact["primitive_components"], json!([]));
    assert_eq!(artifact["components"].as_array().unwrap().len(), 4);
    assert_eq!(artifact["components"][0], artifact["arguments"][0]);
    assert_eq!(artifact["components"][2], artifact["arguments"][0]);
    assert_eq!(artifact["components"][3], artifact["arguments"][1]);
    assert_eq!(
        artifact["list_components"],
        json!([artifact["listed"]["item"]])
    );
    assert_eq!(artifact["listed"]["kind"], "list");
    assert_eq!(artifact["listed"]["item"]["name"], "main.Box");
    assert_eq!(artifact["descriptor_schema"], artifact["tag_schema"]);

    let error = compile(
        "artifact(type_schema(type_evidence(type_tag<String>())))",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("type_schema expects TypeTag"), "{error}");
}

#[test]
fn nominal_adt_checker_enforces_ownership_exhaustiveness_and_branch_uniformity() {
    let root = workspace("nominal-adt-errors");
    let declarations = r#"
      type First = Same(Int) | Other(String)
type Second = Same(Int) | Other(String)
let value = First.Same(1)
    "#;
    for (expression, expected) in [
        (
            "match value { case Same(x) => x }",
            "non-exhaustive match; missing Other",
        ),
        (
            "match value { case Same(x) => x, case Same(y) => y, case Other(z) => 0 }",
            "duplicate match arm for Same",
        ),
        (
            "match value { case Same(x) => x, case Foreign(z) => 0 }",
            "constructor Foreign is not a member of main.First",
        ),
        (
            "if true then 1 else \"no\"",
            "if branches must return one compatible type",
        ),
        ("(1: First)", "value does not conform to main.First"),
    ] {
        let source = format!("{declarations}\nartifact({expression})");
        let error = compile(&source, &root).unwrap_err().to_string();
        assert!(error.contains(expected), "{expression}: {error}");
    }
}

#[test]
fn integer_builtins_are_registered_typed_and_evaluated() {
    let root = workspace("integer-builtins");
    let artifact = compile(
        r#"
let product: Int = int_mul(6, 7)
let positive_gt: Bool = int_gt(8, 3)
let equal_gt: Bool = int_gt(3, 3)
let negative_gt: Bool = int_gt(2, 5)
artifact {
  product: product,
  positive_gt: positive_gt,
  equal_gt: equal_gt,
  negative_gt: negative_gt,
}
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(
        artifact,
        json!({
            "product": 42,
            "positive_gt": true,
            "equal_gt": false,
            "negative_gt": false,
        })
    );
}

#[test]
fn integer_builtins_reject_non_integer_arguments() {
    let root = workspace("integer-builtin-types");
    for source in [
        r#"artifact(int_mul(2, "3"))"#,
        r#"artifact(int_gt("2", 3))"#,
    ] {
        let error = compile(source, &root).unwrap_err().to_string();
        assert!(error.contains("expected Int, got String"), "{error}");
    }
}

#[test]
fn int_mul_rejects_i64_overflow() {
    let root = workspace("int-mul-overflow");
    let error = compile("artifact(int_mul(9223372036854775807, 2))", &root)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("int_mul overflow: 9223372036854775807 * 2"),
        "{error}"
    );
}

#[test]
fn artifact_is_the_only_top_level_output() {
    let root = workspace("artifact");
    let artifact = compile(r#"artifact {answer: 42}"#, &root).unwrap();
    assert_eq!(artifact, json!({"answer":42}));
    assert!(compile("42", &root)
        .unwrap_err()
        .to_string()
        .contains("must return Artifact"));
}

#[test]
fn lisp_form_diagnostics_survive_authored_lowering() {
    let root = workspace("lisp-form-diagnostics");

    assert!(matches!(
        compile_lisp("(require)", &root),
        Err(nefor_mag::error::MagError::Arity {
            expected: 1,
            got: 0
        })
    ));
    assert_eq!(
        compile_lisp("(artifact (let x 1))", &root)
            .unwrap_err()
            .to_string(),
        "type error: let is only valid directly in a source or function block"
    );
    assert_eq!(
        compile_lisp("(artifact (fn [] Int 1))", &root)
            .unwrap_err()
            .to_string(),
        "type error: typed fn signature required"
    );
    assert_eq!(
        compile_lisp("(artifact (match nil [Int value]))", &root)
            .unwrap_err()
            .to_string(),
        "type error: match arm must be [Constructor binding expression]"
    );
    assert_eq!(
        compile_lisp("(artifact (type-tag []))", &root)
            .unwrap_err()
            .to_string(),
        "type error: invalid type expression"
    );
}

#[test]
fn packed_values_have_an_explicit_compiler_owned_envelope() {
    let root = workspace("packed-value-envelope");
    let artifact = compile(
        r#"type Nested {nested: Bool}
type PackedInput {type: String, value: Nested}
artifact(pack(PackedInput {type: "sha256:user-authored", value: Nested {nested: true}}))"#,
        &root,
    )
    .unwrap();

    assert_eq!(
        artifact,
        json!({
            "$mag": "packed-value",
            "value": {
                "type": "sha256:user-authored",
                "value": {"nested": true}
            }
        })
    );
}

#[test]
fn packed_path_strings_traverses_nominal_records_and_checks_the_selected_shape() {
    let root = workspace("packed-path-strings");
    let artifact = compile(
        r#"type References {one: String, many: List<String>}
type Params {references: References}
let params = pack(Params {references: References {one: "actor.one", many: ["actor.two", "actor.three"]}})
artifact {
  one: packed_path_strings(params, ["references", "one"], false),
  many: packed_path_strings(params, ["references", "many"], true),
}"#,
        &root,
    )
    .unwrap();

    assert_eq!(
        artifact,
        json!({
            "one": ["actor.one"],
            "many": ["actor.two", "actor.three"],
        })
    );

    let missing = compile(
        r#"type Params {reference: String}
let params = pack(Params {reference: "actor.one"})
artifact(packed_path_strings(params, ["missing"], false))"#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        missing.contains("packed_path_strings path <root> has no field \"missing\""),
        "{missing}"
    );

    let wrong_shape = compile(
        r#"type Params {reference: String}
let params = pack(Params {reference: "actor.one"})
artifact(packed_path_strings(params, ["reference"], true))"#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        wrong_shape.contains("packed_path_strings expected List<String> at reference, got string"),
        "{wrong_shape}"
    );
}

#[test]
fn raw_multiline_strings_support_scala_style_margins() {
    let root = workspace("raw-multiline-strings");
    let artifact = compile(
        r#"
          let script = strip_margin("""|set -e
                              |echo 'export PATH="$HOME/.local/bin:$PATH"'
                              |find . \( -name '*.mag' -o -name '*.md' \)""")
artifact {script: script, single_line: replace(script, "\n", " ")}
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(
        artifact,
        json!({
            "script": "set -e\necho 'export PATH=\"$HOME/.local/bin:$PATH\"'\nfind . \\( -name '*.mag' -o -name '*.md' \\)",
            "single_line": "set -e echo 'export PATH=\"$HOME/.local/bin:$PATH\"' find . \\( -name '*.mag' -o -name '*.md' \\)"
        })
    );
}

#[test]
fn rust_compilation_returns_the_artifact_directly() {
    let root = workspace("compilation-artifact");
    assert_eq!(
        CompilerLimits::default(),
        CompilerLimits {
            evaluation_steps: 1_000_000,
            call_depth: 64,
            expression_depth: 128,
            memoized_calls: 16_384,
        }
    );
    let artifact = compile_artifact("artifact {answer: 42}", &root).unwrap();
    assert_eq!(artifact, json!({"answer": 42}));

    let constrained = compile_with_options(
        "artifact {answer: 42}",
        &root,
        CompilerOptions {
            limits: CompilerLimits {
                evaluation_steps: 0,
                ..CompilerLimits::default()
            },
        },
    );
    assert!(constrained.is_err());
}

#[test]
fn direct_let_bindings_and_mutual_recursion_share_a_lexical_scope() {
    let root = workspace("direct-let-mutual");
    let artifact = compile(
        r#"
          let finished = "done"
let first: fn(List<Int>) -> String = |items| => if (=)(count(items), 0) then finished else second(remove_at(items, 0))
let second: fn(List<Int>) -> String = |items| => if (=)(count(items), 0) then finished else first(remove_at(items, 0))
artifact(first([1, 2, 3]))
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(artifact, json!("done"));
}

#[test]
fn direct_let_schedules_forward_values_and_rejects_strict_cycles() {
    let root = workspace("direct-let-forward");
    let artifact = compile(
        r#"
          let message = str(prefix, " world")
let prefix = "hello"
artifact(message)
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(artifact, json!("hello world"));

    let error = compile(
        r#"
          let x = str(y, "!")
let y = str(x, "?")
artifact(x)
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("x -> y -> x"), "{error}");

    let call_through = compile(
        r#"
          let read_value: fn() -> String = | | => value
let value = read_value()
artifact(value)
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        call_through.contains("value -> read_value -> value"),
        "{call_through}"
    );
}

#[test]
fn strict_binding_reports_unknown_symbol_instead_of_recursive_peers() {
    let root = workspace("direct-let-unknown");
    let error = compile(
        r#"
          let answer = missing
let rendered = str("answer: ", answer)
artifact(rendered)
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("unresolved symbol: missing"), "{error}");
    assert!(!error.contains("recursive strict bindings"), "{error}");
}

#[test]
fn recursive_activations_have_distinct_local_binding_slots() {
    let root = workspace("recursive-local-slots");
    let artifact = compile(
        r#"
          let countdown: fn(Bool) -> Int = |again| => {
let result = if again then countdown(false) else 0
result
}
artifact(countdown(true))
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(artifact, json!(0));
}

#[test]
fn direct_let_supports_typed_value_and_function_overloads() {
    let root = workspace("direct-let-overloads");
    let artifact = compile(
        r#"
          let render = "plain"
let render: fn(Int) -> String = |value| => str(value)
let render: fn(Bool) -> String = |value| => str(value)
artifact([(render: String), render(7), render(true)])
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(artifact, json!(["plain", "7", "true"]));

    let duplicate = compile(
        r#"
          let render<T>: fn(T) -> T = |value| => value
let render<U>: fn(U) -> U = |value| => value
artifact {}
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        duplicate.contains("duplicate visible overload"),
        "{duplicate}"
    );
}

#[test]
fn builtin_signatures_participate_in_typed_overload_sets() {
    let root = workspace("builtin-overload-collision");
    let error = compile(
        r#"
          let count<T>: fn(List<T>) -> Int = |items| => 99
artifact(count([1, 2, 3]))
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("duplicate visible overload count"),
        "{error}"
    );

    {
        let (name, declaration) = ("str", r#"let str: fn(Int) -> String = |value| => "custom""#);
        let source = format!("{declaration}\n(artifact {{}})");
        let error = match compile(&source, &root) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("{name} collision unexpectedly compiled"),
        };
        assert!(
            error.contains(&format!("duplicate visible overload {name}")),
            "{name}: {error}"
        );
    }

    let artifact = compile(
        r#"
          let count: fn(Int) -> Int = |value| => 99
artifact {custom: count(1), builtin: count([1, 2, 3])}
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(artifact, json!({"custom":99,"builtin":3}));
}

#[test]
fn generic_binders_do_not_leak_from_peer_signatures() {
    let root = workspace("generic-binder-scope");
    let error = compile(
        r#"
          let identity<T>: fn(T) -> T = |value| => value
let leaked: fn(T) -> T = |value| => value
artifact {}
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("unresolved symbol: T"), "{error}");
}

#[test]
fn nested_same_name_generic_binders_are_fresh_per_candidate() {
    let root = workspace("fresh-generic-binders");
    let artifact = compile(
        r#"
          type Port<T> {value: T}
type Continue<T> {value: T}
let store_port<T>: fn(Port<T>) -> String = |port| => "stored"
let retry_gate<T>: fn(Port<Continue<T>>) -> String = |continued| => store_port(continued)
artifact(retry_gate(Port<Continue<Int>> {value: Continue<Int> {value: 1}}))
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(artifact, json!("stored"));
}

#[test]
fn ambient_generic_variables_remain_rigid_during_candidate_instantiation() {
    let root = workspace("rigid-ambient-generics");
    let error = compile(
        r#"
          let takes_string: fn(fn(String) -> String) -> String = |candidate| => candidate("value")
let outer<T>: fn(T) -> String = |x| => {
let local<U>: fn(U) -> T = |ignored| => x
takes_string(local)
}
artifact {}
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("no overload local matches (Fn String String)"),
        "{error}"
    );
}

#[test]
fn expected_results_resolve_return_only_overloads_and_generics() {
    let root = workspace("expected-result-overloads");
    let artifact = compile(
        r#"
          let choose: fn() -> Int = | | => 7
let choose: fn() -> String = | | => "selected"
let produce<T>: fn() -> T = | | => ("generic": T)
let produce_string: fn() -> String = | | => produce()
let invoke_string: fn(fn(Unit) -> String) -> String = |producer| => producer(nil)
let unit_producer<T>: fn(Unit) -> T = |ignored| => ("higher_order": T)
artifact {overload: (choose(): String), declared: produce_string(), higher_order: invoke_string(unit_producer)}
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(
        artifact,
        json!({
            "overload": "selected",
            "declared": "generic",
            "higher_order": "higher_order",
        })
    );
}

#[test]
fn output_only_generic_memoization_is_specialization_aware() {
    let root = workspace("output-generic-memoization");
    let artifact = compile(
        r#"
          let identify<T>: fn() -> TypeTag<T> = | | => type_tag<T>()
artifact {string: (identify(): TypeTag<String>), integer: (identify(): TypeTag<Int>)}
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(artifact["string"]["name"], json!("String"));
    assert_eq!(artifact["integer"]["name"], json!("Int"));
}

#[test]
fn explicit_phantom_specializations_reach_evaluation_and_memoization() {
    let root = workspace("explicit-phantom-specialization");
    let artifact = compile(
        r#"
          let reveal<T>: fn() -> TypeDescriptor = | | => type_evidence(type_tag<T>())
artifact {string: reveal<String>(), integer: reveal<Int>(), string_again: reveal<String>()}
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(artifact["string"]["name"], json!("String"));
    assert_eq!(artifact["integer"]["name"], json!("Int"));
    assert_eq!(artifact["string_again"], artifact["string"]);
}

#[test]
fn explicit_generic_calls_support_exact_arguments_holes_imports_aliases_and_overloads() {
    let root = workspace("explicit-generic-calls");
    fs::create_dir_all(root.join("helpers")).unwrap();
    fs::write(
        root.join("helpers/generic.mag"),
        r#"
        type Entry<Key, Value> {key: Key, value: Value}
let entry<Key, Value>: fn(Key, Value) -> Entry<Key, Value> = |key, value| => Entry<Key, Value> {key: key, value: value}
        "#,
    )
    .unwrap();
    fs::write(
        root.join("helpers/direct.mag"),
        "let identity<T>: fn(T) -> T = |value| => value\nlet `identity.with.dots`<T>: fn(T) -> T = |value| => value",
    )
    .unwrap();
    fs::write(
        root.join("main.mag"),
        r#"
        import helpers.generic as generic
import helpers.direct.{}
import helpers.generic.{entry as make}
let select<T>: fn(T) -> T = |value| => value
let select<Left, Right>: fn(Left, Right) -> Left = |left, right| => left
let ignore<T>: fn(Int) -> Int = |value| => value
let exact = generic.entry<String, Int>("exact", 1)
let hole = make<String, _>("hole", 2)
let inferred = make("inferred", 3)
artifact {exact: exact, field: exact.value, hole: hole, inferred: inferred, qualified: helpers.direct.identity<String>("qualified"), quoted: helpers.direct.`identity.with.dots`<String>("quoted"), one: select<Int>(4), two: select<Int, String>(5, "ignored"), unused: ignore<String>(6)}
        "#,
    )
    .unwrap();

    let artifact = compile_file_with_inputs_and_module_roots(
        &root,
        "main.mag",
        json!({}),
        std::slice::from_ref(&root),
    )
    .unwrap();
    assert_eq!(
        artifact,
        json!({
            "exact": {"key": "exact", "value": 1},
            "field": 1,
            "hole": {"key": "hole", "value": 2},
            "inferred": {"key": "inferred", "value": 3},
            "qualified": "qualified",
            "quoted": "quoted",
            "one": 4,
            "two": 5,
            "unused": 6,
        })
    );
}

#[test]
fn qualified_match_patterns_resolve_and_validate_the_scrutinee_owner() {
    let root = workspace("qualified-match-patterns");
    fs::create_dir_all(root.join("helpers")).unwrap();
    fs::write(
        root.join("helpers/outcome.mag"),
        "type Outcome<T> = Ok(T) | Error(String)",
    )
    .unwrap();

    fs::write(
        root.join("main.mag"),
        r#"
import helpers.outcome as outcome
let value: outcome.Outcome<Int> = outcome.Outcome<Int>.Ok(42)
artifact(match value { case outcome.Outcome.Ok(answer) => answer, case outcome.Outcome.Error(message) => 0 })
"#,
    )
    .unwrap();
    let artifact = compile_file_with_inputs_and_module_roots(
        &root,
        "main.mag",
        json!({}),
        std::slice::from_ref(&root),
    )
    .unwrap();
    assert_eq!(artifact, json!(42));

    fs::write(
        root.join("main.mag"),
        r#"
import helpers.outcome.{Outcome as Choice}
let value: Choice<Int> = Choice<Int>.Error("no")
artifact(match value { case Choice.Ok(answer) => answer, case Error(message) => 0 })
"#,
    )
    .unwrap();
    let artifact = compile_file_with_inputs_and_module_roots(
        &root,
        "main.mag",
        json!({}),
        std::slice::from_ref(&root),
    )
    .unwrap();
    assert_eq!(artifact, json!(0));

    let error = compile(
        r#"
type Outcome = Ok(Int) | Error(String)
type Other = Ok(Int) | Error(String)
let value: Outcome = Outcome.Ok(1)
artifact(match value { case Other.Ok(answer) => answer, case Outcome.Error(message) => 0 })
"#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("pattern owner main.Other does not match scrutinee owner main.Outcome"),
        "{error}"
    );

    for (arms, expected) in [
        (
            "case Outcome.Ok(answer) => answer, case Ok(other) => other, case Outcome.Error(message) => 0",
            "duplicate match arm for Ok",
        ),
        (
            "case Outcome.Ok(answer) => answer",
            "non-exhaustive match; missing Error",
        ),
    ] {
        let source = format!(
            "type Outcome = Ok(Int) | Error(String)\nlet value: Outcome = Outcome.Ok(1)\nartifact(match value {{ {arms} }})"
        );
        let error = compile(&source, &root).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn call_rejections_report_original_argument_positions_and_candidate_structure() {
    let root = workspace("call-rejection-structure");
    let declarations = r#"
let choose: fn(Int, String, Bool) -> Int = |first, middle, last| => first
let choose: fn(String, Int, Bool) -> String = |first, middle, last| => first
"#;
    for (arguments, position, parameter) in [
        ("\"wrong\", \"ok\", true", "argument 1", "'first'"),
        ("1, 2, true", "argument 2", "'middle'"),
        ("1, \"ok\", 3", "argument 3", "'last'"),
    ] {
        let source = format!("{declarations}\nartifact(choose({arguments}))");
        let error = compile(&source, &root).unwrap_err().to_string();
        assert!(error.contains("closest candidates"), "{error}");
        assert!(error.contains(position), "{error}");
        assert!(error.contains(parameter), "{error}");
        assert!(error.contains("candidate #"), "{error}");
        assert!(error.contains("3 value args"), "{error}");
    }

    let generic = compile(
        r#"
let same<T>: fn(T, T, T) -> T = |first, middle, last| => first
artifact(same<_>(1, 2, "wrong"))
"#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(generic.contains("1 explicit type args"), "{generic}");
    assert!(generic.contains("argument 3 'last'"), "{generic}");
    assert!(generic.contains("conflicting inference"), "{generic}");

    let result = compile(
        r#"
let produce<T>: fn() -> T = | | => ("value": T)
artifact((produce<String>(): Int))
"#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        result.contains("result expected Int, got String"),
        "{result}"
    );

    let higher_order = compile(
        r#"
let apply: fn(fn(Int) -> String, Int, Bool) -> String = |callback, value, enabled| => callback(value)
artifact(apply(((|value| => value): fn(Int) -> Int), 1, true))
"#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        higher_order.contains("argument 1 'callback'") && higher_order.contains("(Fn Int String)"),
        "{higher_order}"
    );

    let fixed_after_hole = compile(
        r#"
let accept<T, U>: fn(T, U, Bool) -> T = |first, second, enabled| => first
artifact(accept<_, String>(1, "ok", 3))
"#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        fixed_after_hole.contains("argument 3 'enabled' expected Bool, got Int"),
        "{fixed_after_hole}"
    );
    assert!(
        !fixed_after_hole.contains("conflicting inference for explicit type argument hole"),
        "{fixed_after_hole}"
    );

    let reordered = compile(
        r#"
let rank<T>: fn(T, T, Int) -> T = |first, second, last| => first
let rank<U>: fn(U, U, Bool) -> U = |first, second, last| => first
artifact(rank("value", true, 1))
"#,
        &root,
    )
    .unwrap_err()
    .to_string();
    let first_candidate = reordered.lines().nth(1).unwrap_or_default();
    assert!(
        first_candidate.contains("last: Int") && first_candidate.contains("argument 2 'second'"),
        "{reordered}"
    );
}

#[test]
fn nominal_ascription_accepts_compatible_function_call_results() {
    let root = workspace("nominal-ascription-call");
    let artifact = compile(
        r#"
newtype UserId = String
let raw: fn() -> String = | | => "u-1"
let already_wrapped: fn() -> UserId = | | => ("u-2": UserId)
artifact {introduced: (raw(): UserId), retained: (already_wrapped(): UserId)}
"#,
        &root,
    )
    .unwrap();
    assert_eq!(artifact, json!({"introduced": "u-1", "retained": "u-2"}));
}

#[test]
fn explicit_generic_calls_diagnose_arity_conflicts_and_unresolved_holes() {
    let root = workspace("explicit-generic-call-errors");
    let cases = [
        (
            "let entry<Key, Value>: fn(Key, Value) -> Value = |key, value| => value\nartifact(entry<String>(\"key\", 1))",
            "generic call entry has no overload with exactly 1 type arguments; declared arities: 2",
        ),
        (
            "let entry<Key, Value>: fn(Key, Value) -> Value = |key, value| => value\nartifact(entry<String, Int>(\"key\", \"wrong\"))",
            "expected Int, got String",
        ),
        (
            "let produce<T>: fn() -> T = | | => (\"value\": T)\nartifact(produce<_>())",
            "cannot infer explicit type argument hole for produce at position 1 (T)",
        ),
        (
            "let same<T>: fn(T, T) -> T = |left, right| => left\nartifact(same<_>(1, \"wrong\"))",
            "conflicting inference for explicit type argument hole in same at position 1 (T)",
        ),
        (
            "let produce<T>: fn() -> T = | | => (\"value\": T)\nlet produce<T>: fn(Int) -> T = |value| => (\"value\": T)\nartifact(produce<_>())",
            "cannot infer explicit type argument hole for produce at position 1 (T)",
        ),
        (
            "let same<T>: fn(T, T) -> T = |left, right| => left\nlet same<T>: fn(T, T, Unit) -> T = |left, right, ignored| => left\nartifact(same<_>(1, \"wrong\"))",
            "conflicting inference for explicit type argument hole in same at position 1 (T)",
        ),
        (
            "let plain: fn(Int) -> Int = |value| => value\nartifact(plain<Int>(1))",
            "plain is not a generic function and does not accept explicit type arguments",
        ),
    ];
    for (source, expected) in cases {
        let error = compile(source, &root).unwrap_err().to_string();
        assert!(
            error.contains(expected),
            "expected {expected:?} in {error:?}"
        );
    }
}

#[test]
fn expected_function_types_resolve_value_function_name_overloads() {
    let root = workspace("higher-order-overload");
    let artifact = compile(
        r#"
          let transform = "plain"
let transform: fn(Int) -> String = |value| => str(value)
let apply_one: fn(fn(Int) -> String) -> String = |operation| => operation(7)
artifact([(transform: String), apply_one(transform)])
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(artifact, json!(["plain", "7"]));
}

#[test]
fn nested_scopes_preserve_all_differently_typed_overloads() {
    let root = workspace("nested-overloads");
    let artifact = compile(
        r#"
          let render: fn(Int) -> String = |value| => str(value)
let use_outer: fn(String) -> String = |render| => str((render: String), render(7))
let run: fn() -> List<String> = | | => {
let show: fn(Int) -> String = |value| => str(value)
let show: fn(Bool) -> String = |value| => str(value)
[show(1), show(true)]
}
artifact {outer: use_outer("value="), local: run()}
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(
        artifact,
        json!({"outer": "value=7", "local": ["1", "true"]})
    );
}

#[test]
fn closures_inside_strict_values_see_the_completed_peer_frame() {
    let root = workspace("closure-record-recursion");
    let artifact = compile(
        r#"
          type Handlers {even: fn(List<Int>) -> Bool, odd: fn(List<Int>) -> Bool}
let handlers = Handlers {even: ((|items| => if (=)(count(items), 0) then true else get(handlers, "odd")(remove_at(items, 0))): fn(List<Int>) -> Bool), odd: ((|items| => if (=)(count(items), 0) then false else get(handlers, "even")(remove_at(items, 0))): fn(List<Int>) -> Bool)}
artifact(get(handlers, "even")([1, 2]))
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(artifact, json!(true));
}

#[test]
fn direct_let_builds_nested_shared_scopes_and_checks_parameter_collisions() {
    let root = workspace("direct-let-nested");
    let artifact = compile(
        r#"
          let run: fn(List<Int>) -> String = |items| => {
let finished = "nested"
let first: fn(List<Int>) -> String = |remaining| => if (=)(count(remaining), 0) then finished else second(remove_at(remaining, 0))
let second: fn(List<Int>) -> String = |remaining| => if (=)(count(remaining), 0) then finished else first(remove_at(remaining, 0))
first(items)
}
artifact(run([1, 2]))
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(artifact, json!("nested"));

    let collision = compile(
        r#"
          let item = 1
let use_item: fn(Int) -> Int = |item| => item
artifact {}
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        collision.contains("duplicate visible overload item"),
        "{collision}"
    );
}

#[test]
fn typed_library_functions_return_artifacts() {
    let root = workspace("typed-artifact");
    let source = r#"
      type Answer {answer: Int}
let emit<T>: fn(T) -> Artifact = |value| => artifact(value)
emit(Answer {answer: 42})
    "#;
    let artifact = compile(source, &root).unwrap();
    assert_eq!(artifact, json!({"answer":42}));
}

#[test]
fn qualified_nominal_constructors_do_not_duck_type() {
    let root = workspace("qualified-nominals");
    fs::create_dir_all(root.join("left")).unwrap();
    fs::create_dir_all(root.join("right")).unwrap();
    fs::write(root.join("left/types.mag"), "type Payload {value: Int}").unwrap();
    fs::write(root.join("right/types.mag"), "type Payload {value: Int}").unwrap();
    fs::write(
        root.join("main.mag"),
        r#"
          import left.types.{}
import right.types.{}
let accept_left: fn(left.types.Payload) -> left.types.Payload = |value| => value
artifact(accept_left(right.types.Payload {value: 1}))
        "#,
    )
    .unwrap();

    let error = compile_file_with_inputs(&root, "main.mag", json!({}))
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
          artifact {left: type_evidence(type_tag<((Int, String), Bool)>()), right: type_evidence(type_tag<(Int, (String, Bool))>()), flat: type_evidence(type_tag<(Int, String, Bool)>())}
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
    assert_eq!(artifact["left"], left);
    assert_eq!(artifact["right"], right);
    assert_eq!(artifact["flat"], flat);
    assert_ne!(artifact["left"], artifact["right"]);
    assert_ne!(artifact["left"], artifact["flat"]);
    assert_ne!(artifact["right"], artifact["flat"]);
}

#[test]
fn immutable_host_inputs_expose_only_typed_projections() {
    let root = workspace("inputs");
    fs::write(
        root.join("core/input.mag"),
        "type TypeScheme {input_tags: List<String>, outputs: List<String>}\ntype Contract {identity: String, type_scheme: TypeScheme}\nlet contracts = host_input(\"factory_contracts\", type_tag<List<Contract>>())",
    )
    .unwrap();
    fs::write(
        root.join("main.mag"),
        "let inputs = 1\nimport core.input.{}\nartifact {local: (inputs: Int), contracts: core.input.contracts}",
    )
    .unwrap();
    let loaded = compile_file_with_inputs(
        &root,
        "main.mag",
        json!({"factory_contracts":[{
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
        loaded,
        json!({"local":1,"contracts":[{
            "identity":"x",
            "type_scheme":{"input_tags":["in"],"outputs":["out"]}
        }]})
    );
}

#[test]
fn host_input_projection_selects_the_hidden_capability_by_type() {
    let root = workspace("typed-input-capability");
    fs::write(
        root.join("main.mag"),
        r#"
          let inputs = 1
artifact {local: (inputs: Int), host: host_input("message", type_tag<String>())}
        "#,
    )
    .unwrap();

    let loaded = compile_file_with_inputs(&root, "main.mag", json!({"message":"hello"})).unwrap();
    assert_eq!(loaded, json!({"local":1,"host":"hello"}));
}

#[test]
fn host_input_projection_reports_missing_and_mistyped_values() {
    let root = workspace("input-errors");
    fs::write(
        root.join("main.mag"),
        "artifact(host_input(\"count\", type_tag<Int>()))",
    )
    .unwrap();

    let missing = compile_file_with_inputs(&root, "main.mag", json!({}))
        .unwrap_err()
        .to_string();
    assert!(
        missing.contains("host input \"count\" is not present"),
        "{missing}"
    );

    let mistyped = compile_file_with_inputs(&root, "main.mag", json!({"count":"many"}))
        .unwrap_err()
        .to_string();
    assert!(
        mistyped.contains("host input \"count\": type error: expected Int"),
        "{mistyped}"
    );

    for (ty, value) in [
        ("Bool", json!("true")),
        ("String", json!(true)),
        ("Unit", json!({})),
    ] {
        fs::write(
            root.join("main.mag"),
            format!("artifact(host_input(\"value\", type_tag<{ty}>()))"),
        )
        .unwrap();
        let error = compile_file_with_inputs(&root, "main.mag", json!({"value":value}))
            .unwrap_err()
            .to_string();
        assert!(error.contains(&format!("expected {ty}")), "{error}");
    }

    fs::write(
        root.join("main.mag"),
        "type Step {enabled: Bool, label: String}\ntype Config {steps: List<Step>}\nartifact(host_input(\"config\", type_tag<Config>()))",
    )
    .unwrap();
    let nested = compile_file_with_inputs(
        &root,
        "main.mag",
        json!({"config":{"steps":[{"enabled":"yes","label":"build"}]}}),
    )
    .unwrap_err()
    .to_string();
    assert!(nested.contains("expected Bool"), "{nested}");
}

#[test]
fn json_data_files_are_parsed_into_mag_values() {
    let root = workspace("file-read_json");
    let file = root.join("toolsets.json");
    fs::write(&file, r#"{"read_only":["read_file","read_image"]}"#).unwrap();
    fs::write(
        root.join("main.mag"),
        r#"
          let manifest = read_json("toolsets.json")
artifact(get(manifest, "read_only"))
        "#,
    )
    .unwrap();

    let loaded = compile_file_with_inputs(&root, "main.mag", json!({})).unwrap();
    assert_eq!(loaded, json!(["read_file", "read_image"]));

    fs::write(&file, "not json").unwrap();
    let error = compile_file_with_inputs(&root, "main.mag", json!({}))
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot parse JSON toolsets.json"), "{error}");
}

#[test]
fn fail_preserves_library_diagnostics() {
    let root = workspace("failure");
    let error = compile("type Failure {kind: String, errors: List<String>}\nfail(Failure {kind: \"Invalid\", errors: [\"bad route\"]})", &root)
        .unwrap_err()
        .to_string();
    assert!(error.contains("Invalid"), "{error}");
    assert!(error.contains("bad route"), "{error}");
}

#[test]
fn never_branch_adopts_the_returning_branch_type() {
    let root = workspace("never-branch");
    let artifact = compile(
        r#"
        type Choice = Stop(Unit) | Go(String)
type Unreachable {kind: String}
let choice = Choice.Go("matched")
artifact {conditional: if true then "ok" else fail(Unreachable {kind: "unreachable"}), matched: match choice { case Stop(value) => fail(Unreachable {kind: "unreachable"}), case Go(value) => value }}
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(artifact, json!({"conditional":"ok","matched":"matched"}));
}

#[test]
fn typed_generic_functions_construct_nominal_records() {
    let root = workspace("typed-functions");
    let source = r#"
      type Box<T> {value: T}
let box<T>: fn(T) -> Box<T> = |value| => Box<T> {value: value}
artifact(box(42))
    "#;
    let artifact = compile(source, &root).unwrap();
    assert_eq!(artifact, json!({"value":42}));
}

#[test]
fn checker_rejects_bad_returns_and_calls() {
    let root = workspace("type-errors");
    let bad_return = compile(
        "let wrong: fn(Int) -> String = |value| => value\nartifact {}",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        bad_return.contains("returns Int, declared String"),
        "{bad_return}"
    );

    let bad_call = compile(
        "let only_int: fn(Int) -> Int = |value| => value\nartifact(only_int(\"no\"))",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(bad_call.contains("expected Int, got String"), "{bad_call}");
}

#[test]
fn host_inputs_are_opaque_outside_typed_projections() {
    let root = workspace("opaque-inputs");
    fs::write(
        root.join("main.mag"),
        "artifact(get(inputs, \":contracts\"))",
    )
    .unwrap();
    let error = compile_file_with_inputs(
        &root,
        "main.mag",
        json!({"contracts":[{"identity":"worker"}]}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("get expects a record"), "{error}");
}

#[test]
fn factory_descriptions_use_ordinary_typed_functions() {
    let root = workspace("factory-data");
    let source = r#"
      type Params {seed: String}
type Input {prompt: String}
type Output {answer: String}
type FactoryDescription {factory: String, type_arguments: List<TypeDescriptor>, params: Params}
let worker: fn(Params, TypeTag<Input>, TypeTag<Output>) -> FactoryDescription = |params, input, output| => FactoryDescription {factory: "runtime.worker", type_arguments: [type_evidence(input), type_evidence(output)], params: params}
artifact(worker(Params {seed: "x"}, type_tag<Input>(), type_tag<Output>()))
    "#;
    let artifact = compile(source, &root).unwrap();
    assert_eq!(artifact["factory"], "runtime.worker");

    let bad = source.replace("{seed: \"x\"}", "{wrong: \"x\"}");
    assert!(compile(&bad, &root).is_err());
}

#[test]
fn empty_lists_retain_expected_element_types_at_runtime() {
    let root = workspace("empty-list");
    let source = r#"
      let accept_strings: fn(List<String>) -> Int = |items| => count(items)
artifact {count: accept_strings(([]: List<String>))}
    "#;
    let artifact = compile(source, &root).unwrap();
    assert_eq!(artifact["count"], 0);
}

#[test]
fn record_literals_do_not_construct_native_maps() {
    let root = workspace("record-map");
    let source = r#"
      let accept_string_map: fn(Map<String, String>) -> Int = |value| => count(value)
artifact {count: accept_string_map(Map<String, String> {kind: "task", prompt: "Audit"})}
    "#;
    assert!(compile(source, &root).is_err());
}

#[test]
fn standalone_record_values_and_types_are_rejected() {
    let root = workspace("anonymous-record-rejection");
    for source in [
        "let value = {x: 1}\nartifact(nil)",
        "let make: fn() -> Int = | | => {x: 1}\nartifact(nil)",
        "artifact([{x: 1}])",
    ] {
        let error = compile(source, &root).unwrap_err().to_string();
        assert!(
            error.contains("expected name")
                || error.contains("unresolved symbol")
                || error.contains("block result type requires an annotation"),
            "{source}: {error}"
        );
    }
}

#[test]
fn named_fields_remain_owned_by_nominals_and_adt_constructors() {
    let root = workspace("constructor-owned-fields");
    let artifact = compile(
        r#"
        type Payload {value: Int}
type Choice = Selected(Payload)
let payload = Payload {value: 7}
let selected = Choice.Selected(Payload {value: 9})
artifact {payload: payload, selected: match selected { case Selected(value) => get(value, "value") }}
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(artifact, json!({"payload":{"value":7},"selected":9}));

    for fields in ["wrong: 9", "value: 9, extra: 1"] {
        let source = format!(
            "type Payload {{value: Int}}\ntype Choice = Selected(Payload)\nartifact(Choice.Selected(Payload {{{fields}}}))"
        );
        let error = compile(&source, &root).unwrap_err().to_string();
        assert!(
            error.contains("does not conform to main.Payload"),
            "{error}"
        );
    }
}

#[test]
fn circular_modules_report_the_cycle() {
    let root = workspace("cycle");
    fs::write(root.join("core/a.mag"), "import core.b.{}").unwrap();
    fs::write(root.join("core/b.mag"), "import core.a.{}").unwrap();
    fs::write(root.join("main.mag"), "import core.a.{}\nartifact {}").unwrap();
    let error = compile_file_with_inputs(&root, "main.mag", json!({}))
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
    fs::write(library_root.join("core/types.mag"), "let marker = 42").unwrap();
    fs::write(
        entry_root.join("main.mag"),
        "import core.types.{}\nartifact {marker: core.types.marker}",
    )
    .unwrap();
    let loaded = compile_file_with_inputs_and_module_roots(
        &entry_root,
        "main.mag",
        json!({}),
        &[library_root],
    )
    .unwrap();
    assert_eq!(loaded["marker"], 42);
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
    fs::write(left.join("core/types.mag"), "let side = \"left\"").unwrap();
    fs::write(right.join("core/types.mag"), "let side = \"right\"").unwrap();
    fs::write(entry.join("main.mag"), "import core.types.{}\nartifact {}").unwrap();
    let error =
        compile_file_with_inputs_and_module_roots(&entry, "main.mag", json!({}), &[left, right])
            .unwrap_err()
            .to_string();
    assert!(error.contains("ambiguous across search roots"), "{error}");
}

#[test]
fn nominal_values_require_explicit_refinement() {
    let root = workspace("nominal-opacity");
    let implicit = compile(
        "type User {name: String}\nlet user: fn(String) -> User = |name| => {name: name}\nartifact {}",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        implicit.contains("unresolved symbol: name") || implicit.contains("expected name"),
        "{implicit}"
    );

    let artifact = compile(
        "type User {name: String}\nlet user: fn(String) -> User = |name| => User {name: name}\nartifact(user(\"Ada\"))",
        &root,
    )
    .unwrap();
    assert_eq!(artifact, json!({"name":"Ada"}));
}

#[test]
fn product_values_are_exact_ordered_tuples_with_authored_grouping() {
    let root = workspace("product-values");
    let artifact = compile(
        r#"
          artifact {flat: ([1, "middle", 2]: (Int, String, Int)), left: ([([3, "left"]: (Int, String)), true]: ((Int, String), Bool)), right: ([4, (["right", false]: (String, Bool))]: (Int, (String, Bool)))}
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(
        artifact,
        json!({
            "flat": [1, "middle", 2],
            "left": [[3, "left"], true],
            "right": [4, ["right", false]],
        })
    );

    for (name, source) in [
        ("short", r#"artifact(([1]: (Int, String)))"#),
        ("long", r#"artifact(([1, "x", 2]: (Int, String)))"#),
        ("positional", r#"artifact((["x", 1]: (Int, String)))"#),
        ("old-intersection", r#"artifact((1: (Int, Int)))"#),
    ] {
        let error = compile(source, &workspace(name)).unwrap_err().to_string();
        assert!(error.contains("does not conform"), "{name}: {error}");
    }
}

#[test]
fn explicit_record_refinement_reports_missing_and_unexpected_fields() {
    let root = workspace("exact-refinement");
    let error = compile(
        "type ProcessOptions {timeout_ms: Int}\nartifact(ProcessOptions {`timeout-ms`: 30000})",
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
        "type Pair<T> {first: T, second: T}\nartifact(Pair<Int> {first: 1, other: 2})",
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
        "let loop: fn(Int) -> Int = |n| => loop(n)\nartifact(loop(0))",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("evaluation budget exceeded"), "{error}");
}

#[test]
fn repeated_pure_calls_fit_the_budget_by_reusing_results() {
    let root = workspace("memoized-budget");
    let calls = std::iter::repeat_n("identity(value)", 28_000)
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!(
        "let value = 7\nlet identity: fn(Int) -> Int = |item| => item\nartifact([{calls}])"
    );

    let artifact = compile(&source, &root).unwrap();
    assert_eq!(artifact.as_array().unwrap().len(), 28_000);
}

#[test]
fn duplicate_visible_function_signatures_are_rejected() {
    let root = workspace("lexical-recursion");
    let error = compile(
        r#"
          let walk: fn(List<Int>) -> Int = |items| => if (=)(count(items), 0) then 0 else walk(remove_at(items, 0))
let saved = walk
let before = saved([1, 2, 3])
let walk: fn(List<Int>) -> Int = |items| => 99
artifact {before: before, after: saved([1, 2, 3])}
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("duplicate visible overload walk"), "{error}");
}

#[test]
fn deeply_nested_non_function_expressions_are_bounded() {
    let root = workspace("expression-depth");
    let mut expression = String::from("\"value\"");
    for _ in 0..130 {
        expression = format!("str({expression})");
    }
    let source = format!("artifact({expression})");
    let error = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(move || compile(&source, &root).unwrap_err().to_string())
        .unwrap()
        .join()
        .unwrap();
    assert!(
        error.contains("expression nesting limit reached"),
        "{error}"
    );
}

#[test]
fn builtin_type_rules_are_total_and_assoc_checks_values() {
    let root = workspace("builtin-rules");
    let arity = compile("artifact(map())", &root).unwrap_err().to_string();
    assert!(arity.contains("expected 2, got 0"), "{arity}");

    let mismatch = compile(
        "type Counter {count: Int}\nlet update: fn(Counter) -> Counter = |value| => assoc(value, \":count\", \"many\")\nartifact {}",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        mismatch.contains("cannot assoc") || mismatch.contains("expected Int, got String"),
        "{mismatch}"
    );

    for source in [
        "type Counter {count: Int}\nartifact(get(Counter {count: 1}, 0))",
        "type Counter {count: Int}\nartifact(assoc(Counter {count: 1}, 0, 2))",
    ] {
        let error = compile(source, &root).unwrap_err().to_string();
        assert!(error.contains("expected String, got Int"), "{error}");
    }

    let keys = compile(
        r#"
          type Counter {count: Int}
let original = Counter {count: 1}
artifact {string: get(original, "count"), keyword: get(original, "count"), assoc_string: get(assoc(original, "count", 2), "count"), assoc_keyword: get(assoc(original, "count", 3), "count")}
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(
        keys,
        json!({"string":1,"keyword":1,"assoc_string":2,"assoc_keyword":3})
    );
}

#[test]
fn group_by_produces_native_map_and_preserves_bucket_source_order() {
    let root = workspace("group_by-order");
    let artifact = compile(
        r#"
          let first_character: fn(String) -> String = |value| => if (=)(value, "a1") then "a" else "b"
let grouped = group_by(first_character, ["b1", "a1", "b2"])
let empty = group_by(((|value| => value): fn(String) -> String), ([]: List<String>))
let skewed = group_by(((|value| => "all"): fn(Int) -> String), [1, 2, 3, 4, 5, 6, 7, 8])
let interleaved = group_by(((|value| => if (=)(value, "a1") then "a" else if (=)(value, "a2") then "a" else "b"): fn(String) -> String), ["a1", "b1", "a2", "b2"])
artifact {grouped: grouped, a: __map_get(grouped, "a"), b: __map_get(grouped, "b"), b_concatenated: concat(__map_get(grouped, "b"), ["b3"]), b_equals_list: (=)(__map_get(grouped, "b"), ["b1", "b2"]), empty: empty, skewed: __map_get(skewed, "all"), interleaved_a: __map_get(interleaved, "a"), interleaved_b: __map_get(interleaved, "b")}
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(
        artifact,
        json!({
            "grouped": {"a": ["a1"], "b": ["b1", "b2"]},
            "a": ["a1"],
            "b": ["b1", "b2"],
            "b_concatenated": ["b1", "b2", "b3"],
            "b_equals_list": true,
            "empty": {},
            "skewed": [1, 2, 3, 4, 5, 6, 7, 8],
            "interleaved_a": ["a1", "a2"],
            "interleaved_b": ["b1", "b2"]
        })
    );
}

#[test]
fn group_by_visits_left_to_right_and_stops_at_the_first_callback_error() {
    let root = workspace("group_by-callback-error");
    let error = compile(
        r#"
          let key: fn(String) -> String = |value| => if (=)(value, "first") then fail(str("visited:", value)) else fail(str("visited:", value))
artifact(group_by(key, ["first", "second"]))
        "#,
        &root,
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("visited:first"), "{error}");
    assert!(!error.contains("visited:second"), "{error}");
}

#[test]
fn group_by_rejects_invalid_static_calls() {
    let root = workspace("group_by-invalid-calls");
    for (label, source, expected) in [
        (
            "callback result",
            "artifact(group_by(((|value| => value): fn(Int) -> Int), [1]))",
            "group_by callback must return String",
        ),
        (
            "collection",
            "artifact(group_by(((|value| => str(value)): fn(Int) -> String), 1))",
            "group_by expects List",
        ),
        (
            "arity",
            "artifact(group_by(((|value| => str(value)): fn(Int) -> String)))",
            "arity: expected 2, got 1",
        ),
    ] {
        let error = compile(source, &root).unwrap_err().to_string();
        assert!(error.contains(expected), "{label}: {error}");
    }
}

#[test]
fn group_by_builtin_and_non_colliding_user_overload_are_distinguishable() {
    let root = workspace("group_by-overload");
    let artifact = compile(
        r#"
          let group_by: fn(Int) -> String = |value| => "custom"
let grouped = group_by(((|value| => value): fn(String) -> String), ["builtin", "builtin"])
artifact {custom: group_by(1), builtin: __map_get(grouped, "builtin")}
        "#,
        &root,
    )
    .unwrap();

    assert_eq!(
        artifact,
        json!({"custom": "custom", "builtin": ["builtin", "builtin"]})
    );
}

#[test]
fn canonical_and_sort_by_are_typed_deterministic_builtins() {
    let root = workspace("canonical-sort_by");
    let artifact = compile(
        r#"
          type Item {id: String, rank: Int}
type CanonicalMeta {z: Int, a: Int}
type CanonicalInput {nodes: List<String>, meta: CanonicalMeta, kind: String}
let items = ([Item {id: "third", rank: 30}, Item {id: "first", rank: 10}, Item {id: "second", rank: 20}]: List<Item>)
let ordered = sort_by(((|item| => get(item, "id")): fn(Item) -> String), items)
artifact {canonical: canonical(CanonicalInput {nodes: ["a", "b"], meta: CanonicalMeta {z: 2, a: 1}, kind: "edge"}), removed: remove_at(["a", "b", "c"], 1), string_ok: conforms("value", type_evidence(type_tag<String>())), string_bad: conforms(42, type_evidence(type_tag<String>())), item_ok: conforms(Item {id: "item", rank: 1}, type_evidence(type_tag<Item>())), ids: map(((|item| => get(item, "id")): fn(Item) -> String), ordered)}
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(
        artifact,
        json!({
            "canonical": "{\"kind\":\"edge\",\"meta\":{\"a\":1,\"z\":2},\"nodes\":[\"a\",\"b\"]}",
            "removed": ["a", "c"],
            "string_ok": true,
            "string_bad": false,
            "item_ok": true,
            "ids": ["first", "second", "third"]
        })
    );

    let bad_key = compile(
        "artifact(sort_by(((|value| => value): fn(Int) -> Int), [2, 1]))",
        &root,
    )
    .unwrap_err()
    .to_string();
    assert!(
        bad_key.contains("sort_by callback must return String"),
        "{bad_key}"
    );

    let bad_arity = compile("artifact(canonical(1, 2))", &root)
        .unwrap_err()
        .to_string();
    assert!(bad_arity.contains("expected 1, got 2"), "{bad_arity}");
}

#[test]
fn fallible_nodes_compose_with_kleisli_semantics() {
    let root = workspace("node-kleisli-composition");
    let mag_lib = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mag/lib");
    fs::write(
        root.join("main.mag"),
        r#"
          import core.types.{}
import nefor.graph.{}
import nefor.node.{}
import nefor.result.{}
type Success {value: Int}
type Failure {message: String}
let fallible = nefor.graph.identity<core.types.Result<Failure, Success>>("fallible")
let continuation = nefor.result.lift<Success, Failure, Success>("continuation-result", nefor.graph.identity<Success>("continuation"))
let composed = nefor.result.`>=>`(fallible, continuation)
artifact(composed)
        "#,
    )
    .unwrap();

    let program = compile_file_with_inputs_and_module_roots_and_options_and_syntax(
        &root,
        "main.mag",
        json!({}),
        std::slice::from_ref(&mag_lib),
        CompilerOptions::default(),
        SyntaxMode::New,
    )
    .unwrap();

    assert_eq!(program["id"], "fallible");
    assert_eq!(program["actors"], json!([]));
    assert!(program.get("junctions").is_none());
    assert_eq!(
        program["nodes"],
        json!([{"path": ["continuation-result"], "members": []}])
    );
    let flows = program["output"]["boundary"]["through"].as_array().unwrap();
    assert_eq!(flows.len(), 2);
    for flow in flows {
        assert_eq!(flow["steps"][0]["constructor"], "Unpack");
        assert_eq!(flow["steps"][1]["constructor"], "Pack");
    }
}

#[test]
fn data_is_not_a_source_type_or_cast_target() {
    let root = workspace("removed-data");
    let declaration = compile("type Payload {value: Data}\nartifact {}", &root)
        .unwrap_err()
        .to_string();
    assert!(
        declaration.contains("unresolved symbol: Data"),
        "{declaration}"
    );

    let cast = compile("artifact(Data {value: 1})", &root)
        .unwrap_err()
        .to_string();
    assert!(cast.contains("unresolved symbol: Data"), "{cast}");
}

#[test]
fn type_tags_are_typed_canonical_witnesses() {
    let root = workspace("type-tags");
    let artifact = compile(
        "type Payload {value: Int}\nlet tag_of<T>: fn(T) -> TypeTag<T> = |value| => type_tag<T>()\nartifact(tag_of(Payload {value: 1}))",
        &root,
    )
    .unwrap();
    assert_eq!(
        artifact,
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

    let unknown = compile("artifact(type_tag<Missing>())", &root)
        .unwrap_err()
        .to_string();
    assert!(unknown.contains("Missing"), "{unknown}");
}

#[test]
fn repeated_factory_identity_strings_are_ordinary_data() {
    let root = workspace("duplicate-factory-data");
    let artifact = compile("artifact([\"runtime.worker\", \"runtime.worker\"])", &root).unwrap();
    assert_eq!(artifact, json!(["runtime.worker", "runtime.worker"]));
}

#[test]
fn nested_callbacks_capture_enclosing_generic_binders() {
    let root = workspace("nested-generic-binders");
    let artifact = compile(
        "let contains<T>: fn(List<T>, T) -> Bool = |values, value| => (=)(count(filter(((|candidate| => (=)(candidate, value)): fn(T) -> Bool), values)), 1)\nartifact(contains([1, 2], 1))",
        &root,
    )
    .unwrap();
    assert_eq!(artifact, json!(true));
}

#[test]
fn generic_calls_unify_arguments_inside_the_same_nominal_type() {
    let root = workspace("nominal-generic-unification");
    let artifact = compile(
        "type Port<T> {actor: String}\nlet identity_port<O>: fn(Port<O>) -> Port<O> = |port| => port\nlet forward_port<T>: fn(Port<T>) -> Port<T> = |port| => identity_port(port)\nartifact(forward_port(Port<Int> {actor: \"worker\"}))",
        &root,
    )
    .unwrap();
    assert_eq!(artifact, json!({"actor":"worker"}));
}

#[test]
fn type_schema_preserves_qualified_nominals_and_substitutes_generics() {
    let root = workspace("type_schema");
    fs::write(
        root.join("core/types.mag"),
        "type Box<T> {value: T}\nlet schema = type_schema(type_tag<Box<List<String>>>())",
    )
    .unwrap();
    fs::write(
        root.join("main.mag"),
        "import core.types.{}\nartifact(core.types.schema)",
    )
    .unwrap();
    let artifact = compile_file_with_inputs(&root, "main.mag", json!({})).unwrap();
    assert_eq!(artifact["version"], 2);
    assert_eq!(artifact["root"]["kind"], "named");
    assert_eq!(artifact["root"]["name"], "core.types.Box");
    assert_eq!(
        artifact["root"]["body"]["fields"][0]["schema"]["kind"],
        "list"
    );
    assert_eq!(
        artifact["root"]["body"]["fields"][0]["schema"]["item"]["kind"],
        "string"
    );
}

#[test]
fn type_schema_rejects_non_data() {
    let root = workspace("type_schema-errors");
    for (ty, expected) in [
        (
            "fn(String) -> String",
            "Fn cannot enter a concrete semantic descriptor",
        ),
        (
            "Artifact",
            "Artifact cannot enter a concrete semantic descriptor",
        ),
        (
            "TypeTag<String>",
            "TypeTag cannot enter a concrete semantic descriptor",
        ),
    ] {
        let source = format!("artifact(type_schema(type_tag<{ty}>()))");
        let error = compile(&source, &root).unwrap_err().to_string();
        assert!(error.contains(expected), "{ty}: {error}");
    }
}

#[test]
fn generic_list_inference_preserves_element_evidence() {
    let root = workspace("generic-list-inference");
    for values in ["[1, 2]", "values", "([1, 2]: List<Int>)"] {
        let source = format!(
            r#"
          let values = [1, 2]
let element_tag<T>: fn(List<T>) -> TypeTag<T> = |values| => type_tag<T>()
artifact([type_evidence(element_tag({values})), type_evidence(type_tag<Int>())])
        "#
        );
        let evidence = compile(&source, &root).unwrap();
        assert_eq!(evidence[0], evidence[1], "{values}");
    }
}

#[test]
fn concrete_data_equality_is_recursive_and_float_bit_exact() {
    let root = workspace("concrete-equality");
    let artifact = compile(
        r#"
        type Comparable {a: List<Int>, b: List<Bool>}
let mapped = map(((|x| => x): fn(Int) -> Int), [1, 2])
artifact {list: (=)(mapped, [1, 2]), record: (=)(Comparable {b: [true, false], a: mapped}, Comparable {a: [1, 2], b: [true, false]}), ordered: not((=)([1, 2], [2, 1])), close: not((=)(1.0, 1.0000000000000002)), signed_zero: not((=)(0.0, -0.0)), same: (=)(-0.0, -0.0)}
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(
        artifact,
        json!({"list":true,"record":true,"ordered":true,"close":true,"signed_zero":true,"same":true})
    );
}

#[test]
fn native_maps_and_sets_are_unordered_concrete_data() {
    let root = workspace("native-unordered-data");
    let artifact = compile(
        r#"
        type IdKey {id: Int}
let empty = __map_empty(type_tag<IdKey>(), type_tag<String>())
let left = __map_insert(__map_insert(empty, IdKey {id: 2}, "two"), IdKey {id: 1}, "one")
let right = __map_insert(__map_insert(empty, IdKey {id: 1}, "one"), IdKey {id: 2}, "two")
let set_empty = __set_empty(type_tag<Int>())
let a = __set_insert(__set_insert(set_empty, 2), 1)
let b = __set_insert(__set_insert(set_empty, 1), 2)
let keyed = __map_insert(__map_empty(type_tag<Set<Int>>(), type_tag<Bool>()), a, true)
artifact {map: left, set: a, map_equal: (=)(left, right), set_equal: (=)(a, b), lookup: __map_get(left, IdKey {id: 1}), set_key: __map_get(keyed, b), map_count: __map_count(left), set_count: __set_count(a), map_has: __map_contains(left, IdKey {id: 2}), map_missing: __map_contains(left, IdKey {id: 3}), set_has: __set_contains(a, 1), set_missing: __set_contains(a, 3)}
        "#,
        &root,
    )
    .unwrap();
    assert_eq!(
        artifact,
        json!({
            "map":{"$mag":"map","entries":[[{"id":1},"one"],[{"id":2},"two"]]},
            "set":{"$mag":"set","items":[1,2]},
            "map_equal":true,"set_equal":true,"lookup":"one","set_key":true,
            "map_count":2,"set_count":2,"map_has":true,"map_missing":false,"set_has":true,"set_missing":false
        })
    );
}

#[test]
fn native_collection_duplicates_and_missing_lookup_fail() {
    let root = workspace("native-duplicate-data");
    for (source, message) in [
        (
            r#"let m = __map_empty(type_tag<Int>(), type_tag<String>())
artifact(__map_insert(__map_insert(m, 1, "first"), 1, "second"))"#,
            "duplicate Map key",
        ),
        (
            r#"type SetItem {x: Int}
let s = __set_empty(type_tag<SetItem>())
artifact(__set_insert(__set_insert(s, SetItem {x: 1}), SetItem {x: 1}))"#,
            "duplicate Set member",
        ),
        (
            r#"artifact(__map_get(__map_empty(type_tag<Int>(), type_tag<String>()), 1))"#,
            "Map key not found",
        ),
        (r#"artifact {same: 1, same: 2}"#, "duplicate"),
        (
            r#"type Bad {same: Int, same: String}
artifact {}"#,
            "duplicate",
        ),
    ] {
        let error = compile(source, &root).unwrap_err().to_string();
        assert!(error.contains(message), "{source}: {error}");
    }
}

#[test]
fn maps_and_sets_have_no_ordered_collection_or_record_surface() {
    let root = workspace("native-no-enumeration");
    for expression in [
        "keys(m)",
        "get(m, \"key\")",
        "assoc(m, \"key\", 1)",
        "first(m)",
        "remove_at(m, 0)",
        "map(((|x| => x): fn(Int) -> Int), m)",
        "keys(s)",
        "first(s)",
        "remove_at(s, 0)",
        "fold(((|a, b| => a): fn(Int, Int) -> Int), 0, s)",
    ] {
        let source = format!(
            r#"
            let m = __map_empty(type_tag<String>(), type_tag<Int>())
let s = __set_empty(type_tag<Int>())
artifact({expression})"#
        );
        assert!(compile(&source, &root).is_err(), "{expression}");
    }
    assert_eq!(
        compile(
            r#"
        type Pair {a: Int, b: Int}
type Single {a: Int}
let pair = Pair {b: 2, a: 1}
let single = Single {a: 1}
artifact {keys: keys(pair), get: get(single, "a"), assoc: assoc(single, "a", 2)}"#,
            &root
        )
        .unwrap(),
        json!({"keys":["a","b"],"get":1,"assoc":{"a":2}})
    );
}

#[test]
fn equality_rejects_behavior_in_nested_and_generic_positions() {
    let root = workspace("equality-obligations");
    for source in [
        "let f: fn(Int) -> Int = |x| => x\nartifact((=)(f, f))",
        "let f: fn(Int) -> Int = |x| => x\nartifact((=)([f], [f]))",
        "let f: fn(Int) -> Int = |x| => x\nartifact((=)({f: f}, {f: f}))",
        "type Box<T> = Box(T)\nlet f: fn(Int) -> Int = |x| => x\nlet b = Box<fn(Int) -> Int>.Box(f)\nartifact((=)(b, b))",
        "let eq<T>: fn(T, T) -> Bool = |a, b| => (=)(a, b)\nlet f: fn(Int) -> Int = |x| => x\nartifact(eq(f, f))",
        "let eq<T>: fn(T, T) -> Bool = |a, b| => (=)(a, b)\nlet alias = eq\nlet f: fn(Int) -> Int = |x| => x\nartifact(alias(f, f))",
        "let eq<T>: fn(T, T) -> Bool = |a, b| => (=)(a, b)\nlet twice<T>: fn(T) -> Bool = |a| => eq(a, a)\nlet f: fn(Int) -> Int = |x| => x\nartifact(twice(f))",
        "let eq<T>: fn(T, T) -> Bool = |a, b| => (=)(a, b)\nlet f: fn(Int) -> Int = |x| => x\nartifact(if true then true else eq(f, f))",
        "artifact(__set_empty(type_tag<fn(Int) -> Int>()))",
        "artifact(__map_empty(type_tag<`:f`<fn(Int) -> Int>>(), type_tag<String>()))",
    ] {
        assert!(compile(source, &root).is_err(), "accepted {source}");
    }
    assert_eq!(
        compile(
            r#"
        type DataBox {data: List<Int>}
let eq<T>: fn(T, T) -> Bool = |a, b| => (=)(a, b)
let twice<T>: fn(T) -> Bool = |a| => eq(a, a)
let identity<T>: fn(T) -> T = |a| => a
let f = identity(((|x| => x): fn(Int) -> Int))
artifact {equal: twice(DataBox {data: [1, 2]}), function: f(7)}"#,
            &root
        )
        .unwrap(),
        json!({"equal":true,"function":7})
    );
}

#[test]
fn native_collection_wire_and_schema_agree_recursively() {
    use nefor_mag::{
        env::Env,
        json::{json_to_typed_value, value_to_json},
        schema::TypeSchema,
        types::MagType,
    };
    let env = Env::new();
    let cases = [
        (
            MagType::Map(Box::new(MagType::Int), Box::new(MagType::String)),
            json!({"$mag":"map","entries":[]}),
        ),
        (
            MagType::Map(Box::new(MagType::String), Box::new(MagType::Int)),
            json!({}),
        ),
        (
            MagType::Set(Box::new(MagType::Int)),
            json!({"$mag":"set","items":[]}),
        ),
        (
            MagType::Map(
                Box::new(MagType::Set(Box::new(MagType::Int))),
                Box::new(MagType::Bool),
            ),
            json!({"$mag":"map","entries":[[{"$mag":"set","items":[1,2]},true]]}),
        ),
    ];
    for (ty, wire) in cases {
        let schema = TypeSchema::reify(&env, &ty).unwrap();
        assert!(schema.validate_json(&wire.to_string()).ok, "{ty}: {wire}");
        let value = json_to_typed_value(&env, &wire, &ty).unwrap();
        assert_eq!(value_to_json(&env, &value).unwrap(), wire, "{ty}");
    }
    for (ty, wire) in [
        (
            MagType::Set(Box::new(MagType::Set(Box::new(MagType::Int)))),
            json!({"$mag":"set","items":[{"$mag":"set","items":[1,2]},{"$mag":"set","items":[2,1]}]}),
        ),
        (
            MagType::Map(
                Box::new(MagType::Set(Box::new(MagType::Int))),
                Box::new(MagType::Bool),
            ),
            json!({"$mag":"map","entries":[[{"$mag":"set","items":[1,2]},true],[{"$mag":"set","items":[2,1]},false]]}),
        ),
        (
            MagType::Set(Box::new(MagType::Float)),
            json!({"$mag":"set","items":[1,1.0]}),
        ),
    ] {
        assert!(
            json_to_typed_value(&env, &wire, &ty).is_err(),
            "decoder accepted {wire}"
        );
        assert!(
            !TypeSchema::reify(&env, &ty)
                .unwrap()
                .validate_json(&wire.to_string())
                .ok,
            "schema accepted {wire}"
        );
    }
    let signed_zeros = json!({"$mag":"set","items":[0.0,-0.0]});
    let ty = MagType::Set(Box::new(MagType::Float));
    assert!(json_to_typed_value(&env, &signed_zeros, &ty).is_ok());
    assert!(
        TypeSchema::reify(&env, &ty)
            .unwrap()
            .validate_json(&signed_zeros.to_string())
            .ok
    );
}

#[test]
fn empty_native_collections_keep_their_wire_type() {
    let root = workspace("empty-native-wire");
    assert_eq!(
        compile(
            r#"artifact {map: __map_empty(type_tag<Int>(), type_tag<Bool>()), strings: __map_empty(type_tag<String>(), type_tag<Int>()), set: __set_empty(type_tag<String>())}"#,
            &root
        )
        .unwrap(),
        json!({
            "map":{"$mag":"map","entries":[]},"strings":{},"set":{"$mag":"set","items":[]}
        })
    );
}

#[test]
fn nominal_constructor_and_product_keys_remain_distinct_data() {
    let root = workspace("native-key-constructor");
    assert_eq!(compile(r#"
        type Key = Left(Int) | Right(Int)
let m = __map_empty(type_tag<Key>(), type_tag<String>())
let m1 = __map_insert(m, Key.Left(1), "left")
let m2 = __map_insert(m1, Key.Right(1), "right")
let tuple = ([1, "a"]: (Int, String))
let tuples = __set_insert(__set_empty(type_tag<(Int, String)>()), tuple)
artifact {left: __map_get(m2, Key.Left(1)), right: __map_get(m2, Key.Right(1)), tuple: __set_contains(tuples, ([1, "a"]: (Int, String)))}"#, &root).unwrap(), json!({"left":"left","right":"right","tuple":true}));
}

#[test]
fn ordinary_core_modules_expose_unordered_maps_and_sets() {
    let root = workspace("core-native-collections");
    fs::write(
        root.join("main.mag"),
        r#"
        import core.map.{}
import core.set.{}
let map = core.map.insert((core.map.empty<Int, String>(): Map<Int, String>), 1, "one")
let set = core.set.insert(core.set.empty<String>(), "ready")
artifact {map_value: core.map.get(map, 1), map_count: core.map.count(map), set_member: core.set.contains(set, "ready"), set_count: core.set.count(set)}
        "#,
    )
    .unwrap();
    let module_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mag/lib");
    assert_eq!(
        compile_file_with_inputs_and_module_roots_and_options_and_syntax(
            &root,
            "main.mag",
            json!({}),
            std::slice::from_ref(&module_root),
            CompilerOptions::default(),
            SyntaxMode::New,
        )
        .unwrap(),
        json!({"map_value":"one", "map_count":1, "set_member":true, "set_count":1})
    );

    for source in [
        r#"import core.map.{}
let map = core.map.insert((core.map.empty<Int, String>(): Map<Int, String>), 1, "one")
artifact(core.map.insert(map, 1, "again"))"#,
        r#"import core.set.{}
let set = core.set.insert(core.set.empty<String>(), "ready")
artifact(core.set.insert(set, "ready"))"#,
    ] {
        fs::write(root.join("main.mag"), source).unwrap();
        assert!(
            compile_file_with_inputs_and_module_roots_and_options_and_syntax(
                &root,
                "main.mag",
                json!({}),
                std::slice::from_ref(&module_root),
                CompilerOptions::default(),
                SyntaxMode::New,
            )
            .is_err()
        );
    }
}

#[test]
fn generic_equality_obligations_are_static_in_higher_order_dead_code() {
    let root = workspace("equality-higher-order-static");
    for invocation in [
        "apply(eq, f)",
        "apply(if true then eq else eq, f)",
        "apply(get(CompareHolder {equal: eq}, \"equal\"), f)",
    ] {
        let source = format!(
            r#"
            type CompareHolder {{equal: fn(Int, Int) -> Bool}}
let eq<T>: fn(T, T) -> Bool = |a, b| => (=)(a, b)
let apply<T>: fn(fn(T, T) -> Bool, T) -> Bool = |compare, value| => compare(value, value)
let f: fn(Int) -> Int = |x| => x
artifact(if true then true else {invocation})"#
        );
        let valid = source.replace(invocation, &invocation.replace(", f)", ", 7)"));
        assert_eq!(
            compile(&valid, &root).unwrap_or_else(|error| panic!("{invocation}: {error}")),
            json!(true),
            "valid {invocation}"
        );
        assert!(compile(&source, &root).is_err(), "accepted {invocation}");
    }
}

#[test]
fn native_collections_support_ordinary_generic_wrappers() {
    let root = workspace("native-generic-wrappers");
    let source = r#"
        let empty<K, V>: fn(TypeTag<K>, TypeTag<V>) -> Map<K, V> = |key, value| => __map_empty(key, value)
let insert<K, V>: fn(Map<K, V>, K, V) -> Map<K, V> = |map, key, value| => __map_insert(map, key, value)
let lookup<K, V>: fn(Map<K, V>, K) -> V = |map, key| => __map_get(map, key)
let same<T>: fn(T, T) -> Bool = |left, right| => (=)(left, right)
let table = insert(empty(type_tag<String>(), type_tag<Int>()), ":key", 1)
artifact {lookup: lookup(table, ":key"), table: table, keyword: (=)(":key", ":key"), equal: same(table, insert(empty(type_tag<String>(), type_tag<Int>()), ":key", 1))}
    "#;
    assert_eq!(
        compile(source, &root).unwrap(),
        json!({"lookup":1,"table":{":key":1},"keyword":true,"equal":true})
    );
}

#[test]
fn equality_obligations_survive_forward_computed_function_bindings() {
    let root = workspace("equality-forward-computed");
    let source = r#"
        let compare = ((if true then eq else eq): fn(fn(Int) -> Int, fn(Int) -> Int) -> Bool)
let eq<T>: fn(T, T) -> Bool = |a, b| => (=)(a, b)
let f: fn(Int) -> Int = |x| => x
artifact(if true then true else compare(f, f))
    "#;
    assert!(compile(source, &root).is_err());
    assert_eq!(
        compile(
            r#"
        let compare = ((if true then eq else eq): fn(Int, Int) -> Bool)
let eq<T>: fn(T, T) -> Bool = |a, b| => (=)(a, b)
artifact(compare(1, 1))"#,
            &root
        )
        .unwrap(),
        json!(true)
    );
}
