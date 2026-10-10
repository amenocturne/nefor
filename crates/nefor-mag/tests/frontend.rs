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
    fs::write(
        root.join("a.mag"),
        "let value = 1\nlet bomb = fail(\"must not evaluate before collision resolution\")\n",
    )
    .unwrap();
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
type Sw = On(Int) | Off(Unit)
type Box<T> = Full(T) | Empty(Unit)
type Pt = At {x: Int, y: Int} | Pair(Int, Int)
type Holder {sw: Sw}
type Plain {value: Int}
let show: fn(Sw) -> Int = |sw| => match sw { case On(n) => n, case Off(unit) => 0 }
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
type Mine = Yes(Int) | No(Unit)
let selective = Sw.On(1)
let renamed = Renamed.On(2)
let qualified = support.Sw.On(3)
let namespaced = s.Sw.On(4)
let through_facade = Switch.On(5)
let through_local_alias = Local.On(6)
let unit = Switch.Off()
let generic = Box<Int>.Full(7)
let generic_qualified = support.Box<String>.Full("x")
let generic_facade = Crate<Int>.Empty()
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
let list = [Sw.On(4), Switch.Off()]
let tuple = (Sw.On(5), "label")
let lambda: fn(Int) -> Sw = |n| => Sw.On(n)
let branch: fn(Bool) -> Sw = |flag| => if flag then Sw.On(6) else Sw.Off()
let flip: fn(Sw) -> Sw = |sw| => match sw { case On(n) => Sw.Off(), case Off(unit) => Sw.On(7) }
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
  flip: [show(flip(Sw.On(1))), show(flip(Sw.Off()))],
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
type BuildStatus = Accepted(String) | Failed(Unit)
let review = Review.Accepted(Accepted {score: 9})
let build = BuildStatus.Accepted("green")
let show_review: fn(Review) -> String = |value| => match value {
  case Accepted(accepted) => str(get(accepted, "score")),
  case Rejected(reason) => reason,
}
let show_build: fn(BuildStatus) -> String = |value| => match value {
  case Accepted(label) => label,
  case Failed(unit) => "failed",
}
artifact([show_review(review), show_build(build), show_build(BuildStatus.Failed())])
"#,
    );
    assert_eq!(artifact, serde_json::json!(["9", "green", "failed"]));

    let wrong_owner = constructor_error(
        "constructor-scopes-mismatch",
        r#"
type Review = Accepted(Int) | Rejected(String)
type BuildStatus = Accepted(Int) | Failed(Unit)
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
type Mine = On(Int) | Off(Unit)
let On: fn(Int) -> Int = |n| => int_mul(n, 100)
let local_call = On(2)
let local_constructor = Mine.On(2)
let imported_constructor = Sw.On(3)
let describe: fn(Mine) -> Int = |value| => match value { case On(n) => n, case Off(unit) => 0 }
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
let flip: fn(Sw) -> Sw = |sw| => match sw { case On(n) => Sw.Off(), case Off(unit) => Sw.On(1) }
artifact([On("text"), On(false), str(show(Sw.On(5))), str(show(flip(Sw.Off())))])
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
        "type Sw = On(Int) | Off(Unit)\nlet value = On(1)\nartifact(1)\n",
    );
    assert!(bare.contains("unresolved symbol: On"), "{bare}");

    let annotated = constructor_error(
        "constructor-bare-annotated",
        "type Sw = On(Int) | Off(Unit)\nlet value: Sw = On(1)\nartifact(1)\n",
    );
    assert!(annotated.contains("unresolved symbol: On"), "{annotated}");

    let named = constructor_error(
        "constructor-named-removed",
        "type Sw = On(Int) | Off(Unit)\nlet one = 1\nlet value = named(one, one, one)\nartifact(1)\n",
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

    let unapplied = constructor_error(
        "constructor-unapplied-generic",
        "import support.{Box}\nlet value: Box<Int> = Box.Full(1)\nartifact(1)\n",
    );
    assert!(
        unapplied.contains("support.Box expects 1 type arguments, got 0")
            && unapplied.contains("Box<...>.Full(...)"),
        "{unapplied}"
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
type Other = On(Int) | Off(Unit)
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
let imported: fn(Sw) -> Int = |sw| => match sw { case On(n) => n, case Off(unit) => 0 }
let aliased: fn(Switch) -> Int = |sw| => match sw { case Switch.On(n) => n, case support.Sw.Off(unit) => 0 }
let generic: fn(Box<Int>) -> Int = |box| => match box { case Full(n) => n, case Empty(unit) => 0 }
let generic_alias: fn(Crate<String>) -> String = |box| => match box { case Full(text) => text, case Empty(unit) => "" }
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
        "import support.{Sw}\ntype Mine = On(Int) | Off(Unit)\nlet show: fn(Sw) -> Int = |sw| => match sw { case Mine.On(n) => n, case Off(unit) => 0 }\nartifact(1)\n",
    );
    assert!(
        foreign.contains("does not match scrutinee owner"),
        "{foreign}"
    );
}
