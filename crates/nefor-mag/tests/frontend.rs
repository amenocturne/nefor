use nefor_mag::{compile_file_with_syntax, compile_with_syntax, SyntaxMode};
use std::fs;
use std::path::Path;

fn workspace(name: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("nefor-mag-frontend-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn new_and_lisp_frontends_preserve_nominal_artifact_semantics() {
    let new = compile_with_syntax(
        "type Box { value: Int }\nartifact(Box { value: 42 })\n",
        Path::new("."),
        SyntaxMode::New,
    )
    .unwrap();
    let lisp = compile_with_syntax(
        "(type Box {:value Int})\n(artifact (as Box {:value 42}))\n",
        Path::new("."),
        SyntaxMode::Lisp,
    )
    .unwrap();

    assert_eq!(new, lisp);
    assert_eq!(new, serde_json::json!({"value": 42}));
}

#[test]
fn infix_and_prefix_forms_make_equivalent_binary_calls_while_nary_calls_remain_intact() {
    let artifact = compile_with_syntax(
        r#"
infixl 5 (<>)
let (<>): fn(String, String) -> String = |left, right| => str("(", left, "<>", right, ")")
let join: fn(String, String, String) -> String = |left, middle, right| => str(left, middle, right)
artifact {
  infix_value: "left" <> "right",
  prefix_value: (<>)("left", "right"),
  nary_value: join("left", ":", "right"),
}
"#,
        Path::new("."),
        SyntaxMode::New,
    )
    .unwrap();

    assert_eq!(
        artifact,
        serde_json::json!({
            "infix_value": "(left<>right)",
            "prefix_value": "(left<>right)",
            "nary_value": "left:right",
        })
    );
}

#[test]
fn infix_binary_calls_preserve_declared_precedence_and_associativity() {
    let artifact = compile_with_syntax(
        r#"
infixl 4 (<+>)
infixl 5 (<*>)
infixr 6 (<^>)
let (<+>): fn(String, String) -> String = |left, right| => str("(", left, "+", right, ")")
let (<*>): fn(String, String) -> String = |left, right| => str("(", left, "*", right, ")")
let (<^>): fn(String, String) -> String = |left, right| => str("(", left, "^", right, ")")
artifact {
  precedence: "a" <+> "b" <*> "c",
  left: "a" <+> "b" <+> "c",
  right: "a" <^> "b" <^> "c",
}
"#,
        Path::new("."),
        SyntaxMode::New,
    )
    .unwrap();

    assert_eq!(
        artifact,
        serde_json::json!({
            "precedence": "(a+(b*c))",
            "left": "((a+b)+c)",
            "right": "(a^(b^c))",
        })
    );
}

#[test]
fn imported_arrow_operator_is_infix_only_in_expression_context() {
    let root = workspace("arrow-term-operator");
    fs::write(
        root.join("operators.mag"),
        "let (->): fn(Int, Int) -> Int = |left, right| => left\n",
    )
    .unwrap();
    fs::write(
        root.join("main.mag"),
        "import operators.{`->`}\ninfixr 4 (->)\nlet identity: Int -> Int = |value| => value\nartifact(identity(1 -> 2))\n",
    )
    .unwrap();

    let artifact = compile_file_with_syntax(&root, "main.mag", SyntaxMode::New).unwrap();
    assert_eq!(artifact, serde_json::json!(1));
}

#[test]
fn generic_adt_match_and_expression_block_use_existing_semantics() {
    let artifact = compile_with_syntax(
        r#"
type Score {name: String, accepted: Bool}
type Result<E, T> = Error(E) | Ok(T)

infixr 5 append
let append: fn(String, String) -> String = |left, right| => str(left, right)
let score = Score {name: "review", accepted: true}
let outcome: Result<String, Score> = Result<String, Score>.Ok(score)
let label: String = {
  let suffix = match outcome {
    case Error(reason) => reason,
    case Ok(value) => get(value, "name"),
  }
  "result: " append suffix
}
artifact(label)
"#,
        Path::new("."),
        SyntaxMode::New,
    )
    .unwrap();

    assert_eq!(artifact, serde_json::json!("result: review"));
}

#[test]
fn generic_functions_can_construct_generic_nominal_records() {
    let artifact = compile_with_syntax(
        r#"
type Box<T> {value: T}
let boxed<T>: fn(T) -> Box<T> = |value| => Box<T> {value: value}
artifact(boxed("value"))
"#,
        Path::new("."),
        SyntaxMode::New,
    )
    .unwrap();

    assert_eq!(artifact, serde_json::json!({"value": "value"}));
}

#[test]
fn ordinary_binary_functions_can_be_used_infix() {
    let artifact = compile_with_syntax(
        r#"
let take_left: fn(Int, Int) -> Int = |left, right| => left
artifact(1 take_left 2)
"#,
        Path::new("."),
        SyntaxMode::New,
    )
    .unwrap();

    assert_eq!(artifact, serde_json::json!(1));
}

#[test]
fn new_entry_can_load_an_explicit_lisp_module() {
    let root = workspace("new-loads-lisp");
    fs::write(root.join("support.magl"), "(let value \"mixed\")\n").unwrap();
    fs::write(
        root.join("main.mag"),
        "import support.{}\nartifact(support.value)\n",
    )
    .unwrap();

    let artifact = compile_file_with_syntax(&root, "main.mag", SyntaxMode::New).unwrap();
    assert_eq!(artifact, serde_json::json!("mixed"));
}

#[test]
fn lisp_entry_can_load_a_new_syntax_module() {
    let root = workspace("lisp-loads-new");
    fs::write(root.join("support.mag"), "let value = \"mixed\"\n").unwrap();
    fs::write(
        root.join("main.magl"),
        "(require \"support\")\n(artifact support.value)\n",
    )
    .unwrap();

    let artifact = compile_file_with_syntax(&root, "main.magl", SyntaxMode::Lisp).unwrap();
    assert_eq!(artifact, serde_json::json!("mixed"));
}

#[test]
fn imported_adts_construct_through_qualified_and_selective_owners() {
    let root = workspace("imported-adts");
    fs::write(
        root.join("support.mag"),
        "type Pair<T> = Pair(T, T)\ntype Outcome = Ok(Int) | Error {message: String}\n",
    )
    .unwrap();
    fs::write(
        root.join("main.mag"),
        r#"
import support.{Outcome}
import support.{}
let left = 1
let right = 2
let pair: support.Pair<Int> = support.Pair<Int>.Pair(left, right)
let outcome: support.Outcome = Outcome.Error(message: "failed")
let message = match outcome { case Ok(value) => str(value), case Error(error) => get(error, "message") }
artifact(message)
"#,
    )
    .unwrap();

    let artifact = compile_file_with_syntax(&root, "main.mag", SyntaxMode::New).unwrap();
    assert_eq!(artifact, serde_json::json!("failed"));
}

#[test]
fn selectively_imported_values_keep_dot_field_access() {
    let root = workspace("imported-field-access");
    fs::write(
        root.join("support.mag"),
        "type Box {value: Int}\nlet box = Box {value: 42}\n",
    )
    .unwrap();
    fs::write(
        root.join("main.mag"),
        "import support.{box}\nartifact(box.value)\n",
    )
    .unwrap();

    let artifact = compile_file_with_syntax(&root, "main.mag", SyntaxMode::New).unwrap();
    assert_eq!(artifact, serde_json::json!(42));
}

#[test]
fn both_module_suffixes_are_ambiguous() {
    let root = workspace("ambiguous-suffix");
    fs::write(root.join("support.mag"), "let value = 1\n").unwrap();
    fs::write(root.join("support.magl"), "(let value 1)\n").unwrap();
    fs::write(root.join("main.mag"), "import support.{}\nartifact(())\n").unwrap();

    let error = compile_file_with_syntax(&root, "main.mag", SyntaxMode::New).unwrap_err();
    assert!(error.to_string().contains("ambiguous"));
    assert!(error.to_string().contains("support.mag"));
    assert!(error.to_string().contains("support.magl"));
}

#[test]
fn import_exposure_modes_are_explicit_and_non_transitive() {
    let root = workspace("import-modes");
    fs::write(
        root.join("base.mag"),
        "type Box {value: Int}\nlet value = 42\nlet other = 7\n",
    )
    .unwrap();
    fs::write(
        root.join("middle.mag"),
        "import base.{}\nlet copied = base.value\n",
    )
    .unwrap();

    for (name, source, expected) in [
        ("open", "import base\nartifact(value)\n", 42),
        ("qualified", "import base.{}\nartifact(base.value)\n", 42),
        (
            "selected",
            "import base.{value as selected}\nartifact(selected)\n",
            42,
        ),
        ("aliased", "import base as b\nartifact(b.value)\n", 42),
        (
            "suppressed",
            "import base\nimport base.{value as _}\nartifact(other)\n",
            7,
        ),
    ] {
        let entry = format!("{name}.mag");
        fs::write(root.join(&entry), source).unwrap();
        assert_eq!(
            compile_file_with_syntax(&root, &entry, SyntaxMode::New).unwrap(),
            serde_json::json!(expected)
        );
    }

    fs::write(
        root.join("missing-direct.mag"),
        "import middle.{}\nartifact(base.value)\n",
    )
    .unwrap();
    let missing_direct =
        compile_file_with_syntax(&root, "missing-direct.mag", SyntaxMode::New).unwrap_err();
    let missing_direct = missing_direct.to_string();
    assert!(missing_direct.contains("base.value"), "{missing_direct}");
    assert!(
        missing_direct.contains("import base.{}"),
        "{missing_direct}"
    );
    fs::write(
        root.join("suppressed-missing.mag"),
        "import base\nimport base.{value as _}\nartifact(value)\n",
    )
    .unwrap();
    assert!(compile_file_with_syntax(&root, "suppressed-missing.mag", SyntaxMode::New).is_err());
}

#[test]
fn transparent_aliases_erase_while_newtypes_require_explicit_boundaries() {
    let transparent = compile_with_syntax(
        "type Label = String\nlet label: Label = \"ok\"\nartifact(type_id(type_evidence(type_tag<Label>())))\n",
        Path::new("."),
        SyntaxMode::New,
    ).unwrap();
    let target = compile_with_syntax(
        "artifact(type_id(type_evidence(type_tag<String>())))\n",
        Path::new("."),
        SyntaxMode::New,
    )
    .unwrap();
    assert_eq!(transparent, target);

    let explicit = compile_with_syntax(
        "newtype UserId = String\nlet raw = \"u-1\"\nlet id: UserId = (raw: UserId)\nlet erased: String = (id: String)\nartifact(erased)\n",
        Path::new("."),
        SyntaxMode::New,
    ).unwrap();
    assert_eq!(explicit, serde_json::json!("u-1"));

    let implicit = compile_with_syntax(
        "newtype UserId = String\nlet raw = \"u-1\"\nlet id: UserId = raw\nartifact(id)\n",
        Path::new("."),
        SyntaxMode::New,
    );
    assert!(implicit.is_err());
}

#[test]
fn unresolved_names_get_static_non_evaluating_import_suggestions() {
    let root = workspace("import-suggestions");
    fs::write(
        root.join("candidate.mag"),
        "let desired = fail(\"must not run\")\nlet absent = read(\"missing.txt\")\n",
    )
    .unwrap();
    fs::write(root.join("main.mag"), "artifact(desired)\n").unwrap();

    let error = compile_file_with_syntax(&root, "main.mag", SyntaxMode::New).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("desired"), "{message}");
    assert!(message.contains("import candidate.{desired}"), "{message}");
    assert!(!message.contains("must not run"));
    assert!(!message.contains("missing.txt"));
}

#[test]
fn import_collisions_use_canonical_declarations_and_preserve_overload_sets() {
    let root = workspace("import-declaration-identity");
    fs::write(root.join("a.mag"), "let value = 1\n").unwrap();
    fs::write(root.join("b.mag"), "let value = 1\n").unwrap();
    fs::write(
        root.join("collision.mag"),
        "import a\nimport b\nartifact(value)\n",
    )
    .unwrap();

    let error = compile_file_with_syntax(&root, "collision.mag", SyntaxMode::New)
        .unwrap_err()
        .to_string();
    assert!(error.contains("a.value"), "{error}");
    assert!(error.contains("b.value"), "{error}");
    assert!(error.contains("same type Int"), "{error}");

    // A collision that involves a type is decided by spelling alone, before
    // any imported module is evaluated.
    fs::write(
        root.join("typed.mag"),
        "type Shared {value: Int}\nlet bomb = fail(\"must not evaluate before collision resolution\")\n",
    )
    .unwrap();
    fs::write(root.join("other.mag"), "type Shared {value: Int}\n").unwrap();
    fs::write(
        root.join("type-collision.mag"),
        "import typed\nimport other\nartifact(1)\n",
    )
    .unwrap();
    let error = compile_file_with_syntax(&root, "type-collision.mag", SyntaxMode::New)
        .unwrap_err()
        .to_string();
    assert!(error.contains("import collision at 'Shared'"), "{error}");
    assert!(!error.contains("must not evaluate"), "{error}");

    fs::write(
        root.join("overloads.mag"),
        "let convert: fn(Int) -> String = |value| => str(value)\nlet convert: fn(String) -> Int = |value| => count([value])\n",
    )
    .unwrap();
    fs::write(
        root.join("overload-main.mag"),
        "import overloads\nartifact([convert(1), str(convert(\"x\"))])\n",
    )
    .unwrap();
    assert_eq!(
        compile_file_with_syntax(&root, "overload-main.mag", SyntaxMode::New).unwrap(),
        serde_json::json!(["1", "1"])
    );
}

#[test]
fn imports_do_not_reexport_dependency_bindings_but_keep_type_closure() {
    let root = workspace("direct-export-and-type-closure");
    fs::write(
        root.join("base.mag"),
        "type Record {value: Int}\nlet hidden = 7\n",
    )
    .unwrap();
    fs::write(
        root.join("facade.mag"),
        "import base\ntype Public = base.Record\nlet make: fn(Int) -> Public = |value| => base.Record {value: value}\n",
    )
    .unwrap();
    fs::write(
        root.join("main.mag"),
        "import facade.{}\nartifact(type_id(type_evidence(type_tag<facade.Public>())))\n",
    )
    .unwrap();
    assert!(compile_file_with_syntax(&root, "main.mag", SyntaxMode::New).is_ok());

    fs::write(root.join("leak.mag"), "import facade\nartifact(hidden)\n").unwrap();
    assert!(compile_file_with_syntax(&root, "leak.mag", SyntaxMode::New).is_err());
}

#[test]
fn newtype_ascriptions_reject_nested_conversion_and_accept_call_results() {
    let call = compile_with_syntax(
        "newtype UserId = String\nlet raw: fn(Unit) -> String = |value| => \"u-1\"\nlet id = (raw(nil): UserId)\nartifact((id: String))\n",
        Path::new("."),
        SyntaxMode::New,
    )
    .unwrap();
    assert_eq!(call, serde_json::json!("u-1"));

    for source in [
        "newtype UserId = String\nlet raw = [\"u-1\"]\nlet ids = (raw: List<UserId>)\nartifact(ids)\n",
        "newtype FirstId = String\nnewtype SecondId = String\nlet first: List<FirstId> = [((\"u-1\": FirstId))]\nlet second = (first: List<SecondId>)\nartifact(second)\n",
    ] {
        assert!(compile_with_syntax(source, Path::new("."), SyntaxMode::New).is_err());
    }
}

#[test]
fn newtype_grammar_never_declares_records_or_sums() {
    for source in [
        "newtype Wrong = Variant(Int)\nartifact(())\n",
        "newtype AlsoWrong = {value: Int}\nartifact(())\n",
        "newtype WrongBar = | Variant(Int)\nartifact(())\n",
        "newtype WrongFields = Variant {value: Int}\nartifact(())\n",
    ] {
        assert!(compile_with_syntax(source, Path::new("."), SyntaxMode::New).is_err());
    }
}

#[cfg(unix)]
#[test]
fn import_suggestions_do_not_follow_cycles_or_escape_module_roots() {
    use std::os::unix::fs::symlink;

    let root = workspace("safe-import-suggestions");
    let outside = workspace("outside-import-suggestions");
    fs::write(outside.join("escaped.mag"), "let desired = 1\n").unwrap();
    symlink(".", root.join("loop")).unwrap();
    symlink(&outside, root.join("escape")).unwrap();
    fs::write(root.join("valid.mag"), "type Desired {value: Int}\n").unwrap();
    fs::write(
        root.join("main.mag"),
        "type Alias = Desired\nartifact(())\n",
    )
    .unwrap();

    let error = compile_file_with_syntax(&root, "main.mag", SyntaxMode::New)
        .unwrap_err()
        .to_string();
    assert!(error.contains("import valid.{Desired}"), "{error}");
    assert!(!error.contains("escape"), "{error}");
}

#[test]
fn namespace_alias_collisions_are_order_independent() {
    let root = workspace("namespace-alias-collision");
    fs::write(root.join("a.mag"), "let value = 1\n").unwrap();
    fs::write(root.join("b.mag"), "let value = 2\n").unwrap();
    for (name, imports) in [
        ("forward.mag", "import a as x\nimport b as x\n"),
        ("reverse.mag", "import b as x\nimport a as x\n"),
    ] {
        fs::write(root.join(name), format!("{imports}artifact(x.value)\n")).unwrap();
        let error = compile_file_with_syntax(&root, name, SyntaxMode::New)
            .unwrap_err()
            .to_string();
        assert!(error.contains("namespace alias x for a"), "{error}");
        assert!(error.contains("namespace alias x for b"), "{error}");
    }
}

#[test]
fn wildcard_import_is_not_language_syntax() {
    let root = workspace("wildcard-removed");
    fs::write(root.join("base.mag"), "let value = 1\n").unwrap();
    fs::write(root.join("main.mag"), "import base.*\nartifact(value)\n").unwrap();
    assert!(compile_file_with_syntax(&root, "main.mag", SyntaxMode::New)
        .unwrap_err()
        .to_string()
        .contains("wildcard imports are unsupported"));
}

fn constructor_workspace(name: &str) -> std::path::PathBuf {
    let root = workspace(name);
    fs::write(
        root.join("support.mag"),
        r#"
type Sw = On(Int) | Off
type Box<T> = Full(T) | Empty
type Pt = At {x: Int, y: Int} | Pair(Int, Int)
type Holder {sw: Sw}
type Plain {value: Int}
let show: fn(Sw) -> Int = |sw| => match sw { case On(n) => n, case Off => 0 }
let On: fn(String) -> String = |text| => str("fn:", text)
let On: fn(Bool) -> String = |flag| => str("fn:", flag)
"#,
    )
    .unwrap();
    fs::write(
        root.join("facade.mag"),
        "import support.{}\ntype Switch = support.Sw\ntype Crate<T> = support.Box<T>\n",
    )
    .unwrap();
    root
}

fn constructor_error(name: &str, source: &str) -> String {
    let root = constructor_workspace(name);
    fs::write(root.join("main.mag"), source).unwrap();
    compile_file_with_syntax(&root, "main.mag", SyntaxMode::New)
        .unwrap_err()
        .to_string()
}

fn constructor_artifact(name: &str, source: &str) -> serde_json::Value {
    let root = constructor_workspace(name);
    fs::write(root.join("main.mag"), source).unwrap();
    compile_file_with_syntax(&root, "main.mag", SyntaxMode::New).unwrap()
}

#[test]
fn constructors_resolve_through_every_owner_spelling_without_annotations() {
    let artifact = constructor_artifact(
        "constructor-owners",
        r#"
import support.{Sw, Sw as Renamed, Box}
import support.{}
import support as s
import facade.{Switch, Crate}
type Local = support.Sw
type Mine = Yes(Int) | No
let selective = Sw.On(1)
let renamed = Renamed.On(2)
let qualified = support.Sw.On(3)
let namespaced = s.Sw.On(4)
let through_facade = Switch.On(5)
let through_local_alias = Local.On(6)
let unit = Switch.Off
let generic = Box<Int>.Full(7)
let generic_qualified = support.Box<String>.Full("x")
let generic_facade = Crate<Int>.Empty
let local = Mine.Yes(8)
artifact {
  shown: map(support.show, [selective, renamed, qualified, namespaced, through_facade, through_local_alias, unit]),
  generic: generic,
  generic_qualified: generic_qualified,
  generic_facade: generic_facade,
  local: local,
}
"#,
    );
    assert_eq!(
        artifact,
        serde_json::json!({
            "shown": [1, 2, 3, 4, 5, 6, 0],
            "generic": {"constructor": "Full", "value": 7},
            "generic_qualified": {"constructor": "Full", "value": "x"},
            "generic_facade": {"constructor": "Empty", "value": null},
            "local": {"constructor": "Yes", "value": 8},
        })
    );
}

#[test]
fn constructors_work_in_every_expression_position() {
    let artifact = constructor_artifact(
        "constructor-positions",
        r#"
import support.{Sw, Holder, Pt, Box, show}
import facade.{Switch}
let unannotated = Sw.On(1)
let argument = show(Sw.On(2))
let field = Holder {sw: Switch.On(3)}
let list = [Sw.On(4), Switch.Off]
let tuple = (Sw.On(5), "label")
let lambda: fn(Int) -> Sw = |n| => Sw.On(n)
let branch: fn(Bool) -> Sw = |flag| => if flag then Sw.On(6) else Sw.Off
let flip: fn(Sw) -> Sw = |sw| => match sw { case On(n) => Sw.Off, case Off => Sw.On(7) }
let wrap<T>: fn(T) -> Box<T> = |value| => Box<T>.Full(value)
let several = Pt.Pair(8, 9)
let labelled = Pt.At(x: 10, y: 11)
let braced = Pt.At {x: 12, y: 13}
let nested = wrap<Sw>(Sw.On(14))
artifact {
  unannotated: show(unannotated),
  argument: argument,
  field: show(get(field, "sw")),
  list: map(show, list),
  tuple: tuple,
  lambda: show(lambda(15)),
  branch: [show(branch(true)), show(branch(false))],
  flip: [show(flip(Sw.On(1))), show(flip(Sw.Off))],
  wrap: wrap<Int>(16),
  several: several,
  labelled: labelled,
  braced: braced,
  nested: nested,
}
"#,
    );
    assert_eq!(
        artifact,
        serde_json::json!({
            "unannotated": 1,
            "argument": 2,
            "field": 3,
            "list": [4, 0],
            "tuple": [{"constructor": "On", "value": 5}, "label"],
            "lambda": 15,
            "branch": [6, 0],
            "flip": [0, 7],
            "wrap": {"constructor": "Full", "value": 16},
            "several": {"constructor": "Pair", "value": [8, 9]},
            "labelled": {"constructor": "At", "value": {"x": 10, "y": 11}},
            "braced": {"constructor": "At", "value": {"x": 12, "y": 13}},
            "nested": {"constructor": "Full", "value": {"constructor": "On", "value": 14}},
        })
    );
}

#[test]
fn constructors_are_scoped_by_their_type() {
    // Two types share a constructor name, and a constructor shares its name
    // with a type; each is found in its own owner's scope.
    let artifact = constructor_artifact(
        "constructor-scopes",
        r#"
type Accepted {score: Int}
type Review = Accepted(Accepted) | Rejected(String)
type BuildStatus = Accepted(String) | Failed
let review = Review.Accepted(Accepted {score: 9})
let build = BuildStatus.Accepted("green")
let show_review: fn(Review) -> String = |value| => match value {
  case Accepted(accepted) => str(get(accepted, "score")),
  case Rejected(reason) => reason,
}
let show_build: fn(BuildStatus) -> String = |value| => match value {
  case Accepted(label) => label,
  case Failed => "failed",
}
artifact([show_review(review), show_build(build), show_build(BuildStatus.Failed)])
"#,
    );
    assert_eq!(artifact, serde_json::json!(["9", "green", "failed"]));

    let wrong_owner = constructor_error(
        "constructor-scopes-mismatch",
        r#"
type Review = Accepted(Int) | Rejected(String)
type BuildStatus = Accepted(Int) | Failed
let review: Review = BuildStatus.Accepted(1)
artifact(1)
"#,
    );
    assert!(wrong_owner.contains("main.Review"), "{wrong_owner}");
    assert!(wrong_owner.contains("main.BuildStatus"), "{wrong_owner}");
    assert!(!wrong_owner.contains("initializing"), "{wrong_owner}");
}

#[test]
fn functions_and_constructors_with_one_simple_name_coexist() {
    // `On` is a local function, an imported overload set and a constructor of
    // two types. Calls resolve in the module scope, constructions in the
    // owner type's scope, and patterns against the scrutinee.
    let artifact = constructor_artifact(
        "constructor-function-coexistence",
        r#"
import support.{Sw}
import support.{}
import support as s
type Mine = On(Int) | Off
let On: fn(Int) -> Int = |n| => int_mul(n, 100)
let local_call = On(2)
let local_constructor = Mine.On(2)
let imported_constructor = Sw.On(3)
let describe: fn(Mine) -> Int = |value| => match value { case On(n) => n, case Off => 0 }
artifact {
  local_call: local_call,
  local_constructor: describe(local_constructor),
  imported_constructor: support.show(imported_constructor),
  overloads: [support.On("text"), support.On(true), s.On("alias")],
  qualified_constructor: support.show(support.Sw.On(4)),
}
"#,
    );
    assert_eq!(
        artifact,
        serde_json::json!({
            "local_call": 200,
            "local_constructor": 2,
            "imported_constructor": 3,
            "overloads": ["fn:text", "fn:true", "fn:alias"],
            "qualified_constructor": 4,
        })
    );

    let imported = constructor_artifact(
        "constructor-imported-function-coexistence",
        r#"
import support.{Sw, On, show}
let flip: fn(Sw) -> Sw = |sw| => match sw { case On(n) => Sw.Off, case Off => Sw.On(1) }
artifact([On("text"), On(false), str(show(Sw.On(5))), str(show(flip(Sw.Off)))])
"#,
    );
    assert_eq!(
        imported,
        serde_json::json!(["fn:text", "fn:false", "5", "1"])
    );

    let no_overload = constructor_error(
        "constructor-overload-mismatch",
        "import support.{}\nartifact(support.On(1))\n",
    );
    assert!(no_overload.contains("support.On"), "{no_overload}");
    assert!(!no_overload.contains("constructor"), "{no_overload}");
}

#[test]
fn bare_constructors_and_named_are_not_construction_spellings() {
    let bare = constructor_error(
        "constructor-bare",
        "type Sw = On(Int) | Off\nlet value = On(1)\nartifact(1)\n",
    );
    assert!(bare.contains("unresolved symbol: On"), "{bare}");

    let annotated = constructor_error(
        "constructor-bare-annotated",
        "type Sw = On(Int) | Off\nlet value: Sw = On(1)\nartifact(1)\n",
    );
    assert!(annotated.contains("unresolved symbol: On"), "{annotated}");

    let named = constructor_error(
        "constructor-named-removed",
        "type Sw = On(Int) | Off\nlet one = 1\nlet value = named(one, one, one)\nartifact(1)\n",
    );
    assert!(named.contains("unresolved symbol: named"), "{named}");

    let old_spelling = constructor_error(
        "constructor-named-old-spelling",
        "import support.{Box}\nlet value = named(Box<Int>, Full, 1)\nartifact(1)\n",
    );
    assert!(!old_spelling.is_empty());

    let ordinary = constructor_artifact(
        "constructor-named-identifier",
        "let named: fn(Int, Int) -> Int = |left, right| => int_mul(left, right)\nartifact(named(1, 2))\n",
    );
    assert_eq!(ordinary, serde_json::json!(2));
}

#[test]
fn construction_diagnostics_name_the_type_and_its_constructors() {
    let unknown = constructor_error(
        "constructor-unknown",
        "import facade.{Switch}\nlet value = Switch.Bogus(1)\nartifact(1)\n",
    );
    assert!(
        unknown.contains(
            "constructor Bogus is not a member of support.Sw; available constructors: On, Off"
        ),
        "{unknown}"
    );

    let record = constructor_error(
        "constructor-on-record",
        "import support.{Plain}\nlet value = Plain.Make(1)\nartifact(1)\n",
    );
    assert!(
        record.contains("support.Plain is not an ADT and has no constructor Make"),
        "{record}"
    );

    let scalar = constructor_error(
        "constructor-on-scalar",
        "let value = Int.Make(1)\nartifact(1)\n",
    );
    assert!(
        scalar.contains("Int is not an ADT and has no constructor Make"),
        "{scalar}"
    );

    let missing_payload = constructor_error(
        "constructor-missing-payload",
        "import support.{Sw}\nlet value = Sw.On()\nartifact(1)\n",
    );
    assert!(
        missing_payload
            .contains("constructor support.Sw.On takes a payload of type Int, got 0 arguments"),
        "{missing_payload}"
    );

    let extra_payload = constructor_error(
        "constructor-extra-payload",
        "import support.{Sw}\nlet value = Sw.On(1, 2)\nartifact(1)\n",
    );
    assert!(
        extra_payload
            .contains("constructor support.Sw.On takes a payload of type Int, got 2 arguments"),
        "{extra_payload}"
    );

    let wrong_payload = constructor_error(
        "constructor-wrong-payload",
        "import support.{Sw}\nlet value = [Sw.On(\"x\")]\nartifact(1)\n",
    );
    assert!(
        wrong_payload.contains("expected Int, got String"),
        "{wrong_payload}"
    );

    let wrong_fields = constructor_error(
        "constructor-wrong-fields",
        "import support.{Pt}\nlet value = Pt.At(x: 1)\nartifact(1)\n",
    );
    assert!(
        wrong_fields.contains("constructor fields must exactly match"),
        "{wrong_fields}"
    );

    let labelled_function = constructor_error(
        "constructor-labelled-function",
        "import support.{}\nlet value = support.show(sw: 1)\nartifact(1)\n",
    );
    assert!(
        labelled_function.contains("named arguments are only valid for constructor payloads"),
        "{labelled_function}"
    );

    let duplicate = constructor_error(
        "constructor-duplicate",
        "type Twice = Same(Int) | Same(String)\nartifact(1)\n",
    );
    assert!(
        duplicate.contains("duplicate constructor Same in Twice"),
        "{duplicate}"
    );
}

#[test]
fn annotations_statically_reject_a_construction_of_another_type() {
    let error = constructor_error(
        "constructor-annotation-mismatch",
        r#"
import support.{Sw}
type Other = On(Int) | Off
let later = fail(["must not evaluate"])
let value: Other = Sw.On(1)
artifact(1)
"#,
    );
    assert!(error.contains("main.Other"), "{error}");
    assert!(error.contains("support.Sw"), "{error}");
    assert!(!error.contains("initializing"), "{error}");
    assert!(!error.contains("must not evaluate"), "{error}");
}

#[test]
fn match_patterns_resolve_against_imported_aliased_and_generic_scrutinees() {
    let artifact = constructor_artifact(
        "constructor-patterns",
        r#"
import support.{Sw, Box}
import support.{}
import facade.{Switch, Crate}
type Mine = On(String) | Full(Int)
let imported: fn(Sw) -> Int = |sw| => match sw { case On(n) => n, case Off => 0 }
let aliased: fn(Switch) -> Int = |sw| => match sw { case Switch.On(n) => n, case support.Sw.Off => 0 }
let generic: fn(Box<Int>) -> Int = |box| => match box { case Full(n) => n, case Empty => 0 }
let generic_alias: fn(Crate<String>) -> String = |box| => match box { case Full(text) => text, case Empty => "" }
let mine: fn(Mine) -> String = |value| => match value { case On(text) => text, case Full(n) => str(n) }
artifact {
  imported: imported(Switch.On(1)),
  aliased: aliased(Sw.On(2)),
  generic: generic(Crate<Int>.Full(3)),
  generic_alias: generic_alias(Box<String>.Full("four")),
  mine: [mine(Mine.On("five")), mine(Mine.Full(6))],
}
"#,
    );
    assert_eq!(
        artifact,
        serde_json::json!({
            "imported": 1,
            "aliased": 2,
            "generic": 3,
            "generic_alias": "four",
            "mine": ["five", "6"],
        })
    );

    let foreign = constructor_error(
        "constructor-foreign-pattern",
        "import support.{Sw}\ntype Mine = On(Int) | Off(Unit)\nlet show: fn(Sw) -> Int = |sw| => match sw { case Mine.On(n) => n, case Off => 0 }\nartifact(1)\n",
    );
    assert!(
        foreign.contains("does not match scrutinee owner"),
        "{foreign}"
    );
}

#[test]
fn generic_constructions_infer_type_arguments_from_payload_and_expected_type() {
    let artifact = constructor_artifact(
        "constructor-inference",
        r#"
import support.{Box, Sw}
import facade.{Crate}
type Result<E, T> = Ok(T) | Error(E)
type IntResult<T> = Result<Int, T>
type Pair<A, B> = Both {left: A, right: B} | Neither
type Holder {box: Box<Int>}
let size: fn(Box<Int>) -> Int = |box| => match box { case Full(n) => n, case Empty => 0 }
let from_payload = Box.Full(42)
let from_annotation: Box<Int> = Box.Empty
let from_argument = size(Box.Empty)
let from_field = Holder {box: Box.Empty}
let from_return: fn(Bool) -> Box<Int> = |flag| => if flag then Box.Empty else Box.Full(1)
let from_list_annotation: List<Box<Int>> = [Box.Empty, Box.Full(2)]
let from_earlier_element = [Box.Full(3), Box.Empty]
let from_later_element = [Box.Empty, Box.Full(4)]
let from_branch = if true then Box.Empty else Box.Full(5)
let from_arm = match Sw.Off { case On(n) => Box.Full(n), case Off => Box.Empty }
let combined: Result<String, Int> = Result.Ok(6)
let through_alias = Crate.Full("aliased")
let partial_alias = IntResult.Ok("partial")
let partial_alias_expected: IntResult<String> = IntResult.Error(7)
let labelled = Pair.Both(left: 8, right: "right")
let braced = Pair.Both {left: true, right: [9]}
let nested = Box.Full(Box.Full(10))
let nested_expected: Box<Box<Int>> = Box.Full(Box.Empty)
let explicit = Box<Int>.Full(11)
let wrap<T>: fn(T) -> Box<T> = |value| => Box.Full(value)
let empty<T>: fn() -> Box<T> = | | => Box.Empty
let generic_argument<U>: fn(Box<U>, U) -> U = |box, default| => match box { case Full(value) => value, case Empty => default }
artifact {
  from_payload: size(from_payload),
  from_annotation: size(from_annotation),
  from_argument: from_argument,
  from_field: size(get(from_field, "box")),
  from_return: [size(from_return(true)), size(from_return(false))],
  from_list_annotation: map(size, from_list_annotation),
  from_earlier_element: map(size, from_earlier_element),
  from_later_element: map(size, from_later_element),
  from_branch: size(from_branch),
  from_arm: size(from_arm),
  combined: combined,
  through_alias: through_alias,
  partial_alias: partial_alias,
  partial_alias_expected: partial_alias_expected,
  labelled: labelled,
  braced: braced,
  nested: nested,
  nested_expected: nested_expected,
  explicit: size(explicit),
  wrap: wrap("wrapped"),
  empty: size(empty<Int>()),
  generic_argument: generic_argument(Box.Empty, 12),
}
"#,
    );
    assert_eq!(
        artifact,
        serde_json::json!({
            "from_payload": 42,
            "from_annotation": 0,
            "from_argument": 0,
            "from_field": 0,
            "from_return": [0, 1],
            "from_list_annotation": [0, 2],
            "from_earlier_element": [3, 0],
            "from_later_element": [0, 4],
            "from_branch": 0,
            "from_arm": 0,
            "combined": {"constructor": "Ok", "value": 6},
            "through_alias": {"constructor": "Full", "value": "aliased"},
            "partial_alias": {"constructor": "Ok", "value": "partial"},
            "partial_alias_expected": {"constructor": "Error", "value": 7},
            "labelled": {"constructor": "Both", "value": {"left": 8, "right": "right"}},
            "braced": {"constructor": "Both", "value": {"left": true, "right": [9]}},
            "nested": {"constructor": "Full", "value": {"constructor": "Full", "value": 10}},
            "nested_expected": {"constructor": "Full", "value": {"constructor": "Empty", "value": null}},
            "explicit": 11,
            "wrap": {"constructor": "Full", "value": "wrapped"},
            "empty": 0,
            "generic_argument": 12,
        })
    );
}

#[test]
fn undetermined_and_conflicting_type_arguments_are_rejected() {
    let undetermined = constructor_error(
        "constructor-inference-undetermined",
        "import support.{Box}\nlet value = Box.Empty\nartifact(1)\n",
    );
    assert!(
        undetermined.contains("cannot infer type argument T of support.Box")
            && undetermined.contains("Box<...>.Empty"),
        "{undetermined}"
    );

    let one_of_two = constructor_error(
        "constructor-inference-partly-undetermined",
        "type Result<E, T> = Ok(T) | Error(E)\nlet value = Result.Ok(1)\nartifact(1)\n",
    );
    assert!(
        one_of_two.contains("cannot infer type argument E of main.Result")
            && one_of_two.contains("Result<...>.Ok(...)"),
        "{one_of_two}"
    );

    let in_tuple = constructor_error(
        "constructor-inference-undetermined-tuple",
        "import support.{Box}\nlet value = (Box.Empty, 1)\nartifact(1)\n",
    );
    assert!(
        in_tuple.contains("cannot infer type argument T of support.Box"),
        "{in_tuple}"
    );

    let through_alias = constructor_error(
        "constructor-inference-undetermined-alias",
        "type Result<E, T> = Ok(T) | Error(E)\ntype IntResult<T> = Result<Int, T>\nlet value = IntResult.Error(1)\nartifact(1)\n",
    );
    assert!(
        through_alias.contains("cannot infer type argument T of main.IntResult"),
        "{through_alias}"
    );

    let conflict = constructor_error(
        "constructor-inference-conflict",
        "import support.{Box}\nlet value: Box<String> = Box.Full(1)\nartifact(1)\n",
    );
    assert!(conflict.contains("expected String, got Int"), "{conflict}");

    let explicit_conflict = constructor_error(
        "constructor-inference-explicit-conflict",
        "import support.{Box}\nlet value = Box<String>.Full(1)\nartifact(1)\n",
    );
    assert!(
        explicit_conflict.contains("expected String, got Int"),
        "{explicit_conflict}"
    );

    let explicit_against_annotation = constructor_error(
        "constructor-inference-explicit-annotation",
        "import support.{Box}\nlet value: Box<String> = Box<Int>.Full(1)\nartifact(1)\n",
    );
    assert!(
        explicit_against_annotation.contains("String")
            && explicit_against_annotation.contains("Int"),
        "{explicit_against_annotation}"
    );
}

#[test]
fn annotated_bindings_infer_generic_call_results_from_their_annotation() {
    // Constructions share the generic-call machinery: an annotation is the
    // expected type for a return-only type parameter in both.
    let artifact = constructor_artifact(
        "generic-call-annotation",
        r#"
import support.{Box}
let empty<T>: fn() -> Box<T> = | | => Box<T>.Empty
let size: fn(Box<Int>) -> Int = |box| => match box { case Full(n) => n, case Empty => 0 }
let annotated: Box<Int> = empty()
let listed: List<Box<Int>> = [empty(), Box.Full(1)]
artifact([size(annotated), size(first(listed)), size(empty())])
"#,
    );
    assert_eq!(artifact, serde_json::json!([0, 0, 0]));
}

#[test]
fn positional_arguments_fill_named_fields_in_declared_order() {
    let root = constructor_workspace("constructor-positional-fields");
    fs::write(
        root.join("shapes.mag"),
        "type Pt = At {y: Int, x: String} | Pair(Int, Int) | One {only: Int}\ntype Gen<A, B> = Both {right: B, left: A} | Neither\n",
    )
    .unwrap();
    fs::write(
        root.join("main.mag"),
        r#"
import shapes.{Pt, Gen}
type Local = Mk {b: Int, a: String}
let unannotated = Pt.At(1, "declared order")
artifact {
  imported: unannotated,
  single: Pt.One(2),
  generic: Gen.Both("right", 3),
  generic_explicit: Gen<Int, String>.Both("right", 4),
  local: Local.Mk(5, "a"),
  labelled: Pt.At(x: "labelled", y: 6),
  braced: Pt.At {x: "braced", y: 7},
  tuple: Pt.Pair(8, 9),
}
"#,
    )
    .unwrap();
    assert_eq!(
        compile_file_with_syntax(&root, "main.mag", SyntaxMode::New).unwrap(),
        serde_json::json!({
            "imported": {"constructor": "At", "value": {"x": "declared order", "y": 1}},
            "single": {"constructor": "One", "value": {"only": 2}},
            "generic": {"constructor": "Both", "value": {"left": 3, "right": "right"}},
            "generic_explicit": {"constructor": "Both", "value": {"left": 4, "right": "right"}},
            "local": {"constructor": "Mk", "value": {"a": "a", "b": 5}},
            "labelled": {"constructor": "At", "value": {"x": "labelled", "y": 6}},
            "braced": {"constructor": "At", "value": {"x": "braced", "y": 7}},
            "tuple": {"constructor": "Pair", "value": [8, 9]},
        })
    );

    let error = |name: &str, body: &str| {
        fs::write(
            root.join("main.mag"),
            format!("import shapes.{{Pt}}\nlet value = {body}\nartifact(1)\n"),
        )
        .unwrap();
        compile_file_with_syntax(&root, "main.mag", SyntaxMode::New)
            .map(|artifact| format!("{name} unexpectedly compiled: {artifact}"))
            .unwrap_or_else(|error| error.to_string())
    };
    let too_few = error("too-few", "Pt.At(1)");
    assert!(
        too_few.contains("constructor shapes.Pt.At takes 2 fields (y, x), got 1 arguments"),
        "{too_few}"
    );
    let too_many = error("too-many", "Pt.At(1, \"s\", 3)");
    assert!(
        too_many.contains("constructor shapes.Pt.At takes 2 fields (y, x), got 3 arguments"),
        "{too_many}"
    );
    let swapped = error("swapped", "Pt.At(\"s\", 1)");
    assert!(
        swapped.contains("field y of constructor shapes.Pt.At: expected Int, got String"),
        "{swapped}"
    );
    let labelled_mismatch = error("labelled-mismatch", "Pt.At(x: 1, y: 2)");
    assert!(
        labelled_mismatch.contains("field x of constructor shapes.Pt.At: expected String, got Int"),
        "{labelled_mismatch}"
    );
    let mixed = error("mixed", "Pt.At(1, x: \"s\")");
    assert!(
        mixed.contains("constructor arguments must be either all positional or all named"),
        "{mixed}"
    );
}

#[test]
fn payloadless_constructors_are_written_as_bare_names() {
    let root = constructor_workspace("constructor-payloadless");
    fs::write(
        root.join("shapes.mag"),
        "type Timeout = Unlimited | Seconds(Int) | Explicit(Unit)\ntype Maybe<T> = Just(T) | Nothing\ntype Inner {depth: Int}\ntype Outer {inner: Inner, label: String}\nlet outer = Outer {inner: Inner {depth: 7}, label: \"outer\"}\n",
    )
    .unwrap();
    fs::write(
        root.join("main.mag"),
        r#"
import shapes.{Timeout, Maybe, outer}
import shapes.{}
import shapes as s
type Local = Idle | Busy(Int)
type Single = | Only
type Alias = Timeout
let unannotated = Timeout.Unlimited
let show: fn(Timeout) -> String = |timeout| => match timeout {
  case Unlimited => "unlimited",
  case Seconds(n) => str(n),
  case Explicit(unit) => "explicit",
}
let local: fn(Local) -> Int = |value| => match value { case Idle => 0, case Busy(n) => n }
let size: fn(Maybe<Int>) -> Int = |maybe| => match maybe { case Just(n) => n, case shapes.Maybe.Nothing => 0 }
let annotated: Maybe<Int> = Maybe.Nothing
let nested: fn(Maybe<shapes.Outer>) -> Int = |maybe| => match maybe { case Just(found) => found.inner.depth, case Nothing => 0 }
artifact {
  owners: map(show, [unannotated, shapes.Timeout.Unlimited, s.Timeout.Unlimited, Alias.Unlimited]),
  payloads: [show(Timeout.Seconds(3)), show(Timeout.Explicit(nil))],
  local: [local(Local.Idle), local(Local.Busy(5))],
  single: Single.Only,
  generic: [size(annotated), size(Maybe.Nothing), size(Maybe<Int>.Nothing), size(Maybe.Just(4))],
  list: map(size, [Maybe.Just(1), Maybe.Nothing]),
  value: Timeout.Unlimited,
  unit_parameter: Maybe<Unit>.Just(nil),
  fields: [str(outer.inner.depth), outer.label, str(shapes.outer.inner.depth), str(nested(Maybe.Just(outer)))],
}
"#,
    )
    .unwrap();
    assert_eq!(
        compile_file_with_syntax(&root, "main.mag", SyntaxMode::New).unwrap(),
        serde_json::json!({
            "owners": ["unlimited", "unlimited", "unlimited", "unlimited"],
            "payloads": ["3", "explicit"],
            "local": [0, 5],
            "single": {"constructor": "Only", "value": null},
            "generic": [0, 0, 0, 4],
            "list": [1, 0],
            "value": {"constructor": "Unlimited", "value": null},
            "unit_parameter": {"constructor": "Just", "value": null},
            "fields": ["7", "outer", "7", "7"],
        })
    );

    let error = |body: &str| {
        fs::write(
            root.join("main.mag"),
            format!("import shapes.{{Timeout, Maybe, outer}}\n{body}\nartifact(1)\n"),
        )
        .unwrap();
        compile_file_with_syntax(&root, "main.mag", SyntaxMode::New)
            .map(|artifact| format!("unexpectedly compiled: {artifact}"))
            .unwrap_or_else(|error| error.to_string())
    };
    for (body, expected) in [
        (
            "let value = Timeout.Unlimited()",
            "constructor shapes.Timeout.Unlimited has no payload and takes no argument list; write Timeout.Unlimited",
        ),
        (
            "let value = Timeout.Unlimited(nil)",
            "constructor shapes.Timeout.Unlimited has no payload and takes no argument list; write Timeout.Unlimited",
        ),
        (
            "let value = Maybe<Int>.Nothing()",
            "constructor shapes.Maybe.Nothing has no payload and takes no argument list; write Maybe.Nothing",
        ),
        (
            "let value = Timeout.Seconds",
            "constructor shapes.Timeout.Seconds takes a payload of type Int; write Timeout.Seconds(...)",
        ),
        (
            "let value = Timeout.Explicit",
            "constructor shapes.Timeout.Explicit takes a payload of type Unit; write Timeout.Explicit(...)",
        ),
        (
            "let value = Timeout.Explicit()",
            "constructor shapes.Timeout.Explicit takes a payload of type Unit, got 0 arguments",
        ),
        (
            "let value = Maybe<Int>.Just",
            "constructor shapes.Maybe.Just takes a payload of type Int; write Maybe.Just(...)",
        ),
        (
            "let value = Maybe.Nothing",
            "cannot infer type argument T of shapes.Maybe from the payload or the expected type; write Maybe<...>.Nothing with explicit type arguments",
        ),
        (
            "let value = Timeout.Bogus",
            "constructor Bogus is not a member of shapes.Timeout; available constructors: Unlimited, Seconds, Explicit",
        ),
        (
            "let show: fn(Timeout) -> Int = |t| => match t { case Unlimited(unit) => 0, case Seconds(n) => n, case Explicit(unit) => 1 }",
            "constructor shapes.Timeout.Unlimited has no payload to bind; write case Unlimited =>",
        ),
        (
            "let show: fn(Timeout) -> Int = |t| => match t { case Unlimited => 0, case Seconds => 2, case Explicit(unit) => 1 }",
            "constructor shapes.Timeout.Seconds has a payload of type Int; write case Seconds(name) =>",
        ),
        (
            "let show: fn(Timeout) -> Int = |t| => match t { case Unlimted => 0, case Seconds(n) => n, case Explicit(unit) => 1 }",
            "constructor Unlimted is not a member of shapes.Timeout; available constructors: Unlimited, Seconds, Explicit",
        ),
        (
            "let show: fn(Timeout) -> Int = |t| => match t { case Unlimited => 0, case Seconds(n) => n }",
            "non-exhaustive match; missing Explicit",
        ),
        (
            "let show: fn(Timeout) -> Int = |t| => match t { case Unlimited() => 0, case Seconds(n) => n, case Explicit(unit) => 1 }",
            "a constructor without a payload is matched as the bare name",
        ),
        ("let value = outer.missing", "missing"),
    ] {
        let message = error(body);
        assert!(message.contains(expected), "{body}: {message}");
    }

    // `C()` is not a declaration form, and a bare name alone is an alias.
    let empty = compile_with_syntax(
        "type Flag = Yes() | No\nartifact(1)\n",
        Path::new("."),
        SyntaxMode::New,
    )
    .unwrap_err()
    .to_string();
    assert!(
        empty.contains("constructor Yes() declares no payload; write it as the bare name Yes"),
        "{empty}"
    );
    let alias = compile_with_syntax(
        "type Label = String\nlet label: Label = \"plain\"\nartifact(label)\n",
        Path::new("."),
        SyntaxMode::New,
    )
    .unwrap();
    assert_eq!(alias, serde_json::json!("plain"));
}

#[test]
fn bare_and_unit_payload_declarations_share_one_identity() {
    let identity = |declaration: &str, construction: &str| {
        compile_with_syntax(
            &format!(
                "{declaration}\ntype Evidence {{id: SemanticTypeId, descriptor: TypeDescriptor, schema: TypeSchema, value: Flag}}\nartifact(Evidence {{id: type_id(type_evidence(type_tag<Flag>())), descriptor: type_evidence(type_tag<Flag>()), schema: type_schema(type_tag<Flag>()), value: {construction}}})\n"
            ),
            Path::new("."),
            SyntaxMode::New,
        )
        .unwrap()
    };
    assert_eq!(
        identity("type Flag = Off | On(Int)", "Flag.Off"),
        identity("type Flag = Off(Unit) | On(Int)", "Flag.Off(nil)")
    );
}

#[test]
fn lisp_construct_states_its_payload_including_nil() {
    let artifact = compile_with_syntax(
        "(type Flag (adt [Yes Unit] [No Unit]))\n(artifact (construct Flag Yes nil))\n",
        Path::new("."),
        SyntaxMode::Lisp,
    )
    .unwrap();
    assert_eq!(
        artifact,
        serde_json::json!({"constructor": "Yes", "value": null})
    );
}

fn overload_workspace(name: &str) -> std::path::PathBuf {
    let root = workspace(name);
    for (module, source) in [
        (
            "ints",
            "let render: fn(Int) -> String = |value| => str(\"int:\", value)\nlet limit = 10\ntype Shared {value: Int}\n",
        ),
        (
            "strings",
            "let render: fn(String) -> String = |value| => str(\"string:\", value)\nlet limit = \"ten\"\n",
        ),
        (
            "more_ints",
            "let render: fn(Int) -> String = |value| => str(\"other-int:\", value)\n",
        ),
        (
            "generic",
            "let render<T>: fn(List<T>) -> String = |values| => str(\"list:\", count(values))\n",
        ),
        ("shared_alias", "import ints.{}\ntype Shared = ints.Shared\n"),
        ("relay", "import ints\nlet relayed = render(1)\n"),
    ] {
        fs::write(root.join(format!("{module}.mag")), source).unwrap();
    }
    root
}

fn overload_result(root: &Path, source: &str) -> Result<serde_json::Value, String> {
    fs::write(root.join("main.mag"), source).unwrap();
    compile_file_with_syntax(root, "main.mag", SyntaxMode::New).map_err(|error| error.to_string())
}

#[test]
fn imports_of_one_name_with_different_types_form_an_overload_set() {
    let root = overload_workspace("import-overloads");
    for (form, source) in [
        (
            "open",
            "import ints\nimport strings\nartifact([render(1), render(\"x\")])\n",
        ),
        (
            "selective",
            "import ints.{render}\nimport strings.{render}\nartifact([render(1), render(\"x\")])\n",
        ),
        (
            "renamed",
            "import ints.{render as show}\nimport strings.{render as show}\nartifact([show(1), show(\"x\")])\n",
        ),
        (
            "mixed open and selective",
            "import ints\nimport strings.{render}\nartifact([render(1), render(\"x\")])\n",
        ),
        (
            "selective import after its first use",
            "import ints.{render}\nlet early = render(1)\nimport strings.{render}\nartifact([early, render(\"x\")])\n",
        ),
        (
            "unannotated bindings",
            "import ints\nimport strings\nlet first = render(1)\nlet second = render(\"x\")\nartifact([first, second])\n",
        ),
    ] {
        assert_eq!(
            overload_result(&root, source),
            Ok(serde_json::json!(["int:1", "string:x"])),
            "{form}"
        );
    }

    assert_eq!(
        overload_result(
            &root,
            "import ints\nimport strings\nimport generic\nartifact([render(1), render(\"x\"), render([1, 2])])\n",
        ),
        Ok(serde_json::json!(["int:1", "string:x", "list:2"]))
    );
    // Data values overload by type as functions do.
    assert_eq!(
        overload_result(
            &root,
            "import ints\nimport strings\nlet number: Int = limit\nlet text: String = limit\nartifact([str(number), text])\n",
        ),
        Ok(serde_json::json!(["10", "ten"]))
    );
    // A namespace alias keeps each module's overloads apart.
    assert_eq!(
        overload_result(
            &root,
            "import ints as i\nimport more_ints as m\nartifact([i.render(1), m.render(1)])\n",
        ),
        Ok(serde_json::json!(["int:1", "other-int:1"]))
    );
    // Suppressing one source removes it from the set.
    assert_eq!(
        overload_result(
            &root,
            "import ints\nimport more_ints\nimport more_ints.{render as _}\nartifact(render(1))\n",
        ),
        Ok(serde_json::json!("int:1"))
    );
    // Imports stay non-transitive: `relay` uses `ints.render` without exporting it.
    let hidden =
        overload_result(&root, "import relay\nartifact([relayed, render(1)])\n").unwrap_err();
    assert!(hidden.contains("unresolved symbol: render"), "{hidden}");

    let no_match = overload_result(
        &root,
        "import ints\nimport strings\nartifact(render(true))\n",
    )
    .unwrap_err();
    assert!(
        no_match.contains("no overload render matches call"),
        "{no_match}"
    );
    assert!(no_match.contains("expected Int, got Bool"), "{no_match}");
    assert!(no_match.contains("expected String, got Bool"), "{no_match}");

    let ambiguous = overload_result(
        &root,
        "import ints\nimport strings\nlet chosen = limit\nartifact(1)\n",
    )
    .unwrap_err();
    assert!(ambiguous.contains("limit"), "{ambiguous}");
}

#[test]
fn an_imported_and_a_local_value_of_one_name_overload_by_type() {
    let root = overload_workspace("import-local-overloads");
    assert_eq!(
        overload_result(
            &root,
            "import ints.{render}\nlet render: fn(Bool) -> String = |flag| => str(\"bool:\", flag)\nartifact([render(1), render(true)])\n",
        ),
        Ok(serde_json::json!(["int:1", "bool:true"]))
    );
    assert_eq!(
        overload_result(
            &root,
            "import ints\nlet limit = \"local\"\nlet number: Int = limit\nlet text: String = limit\nartifact([str(number), text])\n",
        ),
        Ok(serde_json::json!(["10", "local"]))
    );

    let selective = overload_result(
        &root,
        "import ints.{render}\nlet render: fn(Int) -> String = |value| => \"local\"\nartifact(render(1))\n",
    )
    .unwrap_err();
    assert!(
        selective.contains(
            "import collision at 'render'; candidates: imported export ints.render, local let render; same type (Fn Int String); repairs: remove or rename selector `ints.{render}`"
        ),
        "{selective}"
    );

    let open = overload_result(&root, "import ints\nlet limit = 3\nartifact(limit)\n").unwrap_err();
    assert!(
        open.contains("import collision at 'limit'")
            && open.contains("same type Int")
            && open.contains("add `import ints.{limit as _}`"),
        "{open}"
    );
}

#[test]
fn imports_of_one_name_with_the_same_type_collide() {
    let root = overload_workspace("import-same-type");
    for (form, source, repair) in [
        (
            "open",
            "import ints\nimport more_ints\nartifact(render(1))\n",
            "add `import more_ints.{render as _}`",
        ),
        (
            "selective",
            "import ints.{render}\nimport more_ints.{render}\nartifact(render(1))\n",
            "remove or rename selector `more_ints.{render}`",
        ),
        (
            "renamed",
            "import ints.{render as show}\nimport more_ints.{render as show}\nartifact(show(1))\n",
            "remove or rename selector `ints.{render}`",
        ),
        (
            "among overloads",
            "import ints\nimport strings\nimport more_ints\nartifact(render(1))\n",
            "add `import ints.{render as _}`",
        ),
    ] {
        let error = overload_result(&root, source).unwrap_err();
        assert!(
            error.contains("imported export ints.render")
                && error.contains("imported export more_ints.render")
                && error.contains("same type (Fn Int String)")
                && error.contains(repair),
            "{form}: {error}"
        );
    }
}

#[test]
fn types_do_not_overload_across_imports() {
    let root = overload_workspace("import-type-collisions");
    // The same nominal type under one name through two paths is still a
    // collision by spelling.
    let alias = overload_result(
        &root,
        "import ints.{Shared}\nimport shared_alias.{Shared}\nartifact(1)\n",
    )
    .unwrap_err();
    assert!(
        alias.contains("import collision at 'Shared'; candidates: imported export ints.Shared, imported export shared_alias.Shared"),
        "{alias}"
    );
    let type_and_value =
        overload_result(&root, "import ints.{Shared}\nlet Shared = 1\nartifact(1)\n").unwrap_err();
    assert!(
        type_and_value.contains("import collision at 'Shared'"),
        "{type_and_value}"
    );
}
