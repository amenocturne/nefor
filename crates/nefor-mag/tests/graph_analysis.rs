use nefor_mag::{
    compile_file_with_inputs_and_module_roots_and_options_and_syntax, CompilerOptions, SyntaxMode,
};
use serde_json::{json, Value};
use std::fs;

fn workspace(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "nefor-mag-graph-analysis-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(path.join("core")).unwrap();
    path
}

fn contracts(extra: Value) -> Value {
    let mut values = vec![json!({
        "identity": "nefor.factory.source",
        "type_scheme": {
            "input_tags": ["mag.Unit"],
            "outputs": ["nefor.graph.Value"]
        }
    })];
    values.extend(extra.as_array().unwrap().iter().cloned());
    json!({"factory_contracts": values})
}

fn run(name: &str, source: &str, inputs: Value) -> Value {
    let root = workspace(name);
    fs::create_dir_all(root.join("nefor")).unwrap();
    fs::write(
        root.join("nefor/toolsets.json"),
        r#"{"read_only":[],"general":[]}"#,
    )
    .unwrap();
    fs::write(root.join("main.mag"), source).unwrap();
    let mag_lib = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mag/lib");
    compile_file_with_inputs_and_module_roots_and_options_and_syntax(
        &root,
        "main.mag",
        inputs,
        &[root.clone(), mag_lib],
        CompilerOptions::default(),
        SyntaxMode::New,
    )
    .unwrap()
}

fn run_error(name: &str, source: &str) -> String {
    let root = workspace(name);
    fs::create_dir_all(root.join("nefor")).unwrap();
    fs::write(
        root.join("nefor/toolsets.json"),
        r#"{"read_only":[],"general":[]}"#,
    )
    .unwrap();
    fs::write(root.join("main.mag"), source).unwrap();
    let mag_lib = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mag/lib");
    compile_file_with_inputs_and_module_roots_and_options_and_syntax(
        &root,
        "main.mag",
        json!({}),
        &[root.clone(), mag_lib],
        CompilerOptions::default(),
        SyntaxMode::New,
    )
    .unwrap_err()
    .to_string()
}

#[test]
fn analysis_preserves_order_and_rejects_changed_node_execution_definition() {
    let artifact = run(
        "ordered-analysis",
        r#"
type MessageContent {content: String}

import core.validated.{}
import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
import nefor.mag.{}

let test_operation<T>: fn(String, nefor.graph.Port<T>) -> nefor.mag.ProgramOperation = |id, on| =>
  nefor.graph.instantiate_delta_template(
    id,
    on,
    (core.map.empty<String, nefor.mag.TypedCapture>(): Map<String, nefor.mag.TypedCapture>),
    ([]: List<nefor.mag.Expression>),
    nefor.mag.DeltaTemplate {
      types: (core.map.empty<String, TypeDescriptor>(): Map<String, TypeDescriptor>),
      actors: [], routes: [], messages: [], nodes: [], actor_reference_relocations: []
    },
  )

let validation_message: fn(core.validated.Validated<String, nefor.graph.Graph>) -> String = |checked| =>
  match checked {
    case Valid(accepted) => "valid",
    case Invalid(rejected) => first(get(rejected, "errors")),
  }
let start_base = nefor.graph.source("z-start", MessageContent {content: "start"})
let middle_input = nefor.graph.port("m-middle", type_tag<MessageContent>(), "test.Value")
let middle_output = nefor.graph.port("m-middle", type_tag<MessageContent>(), "test.Value")
let middle_actor = nefor.graph.actor("m-middle", "test.identity", [type_evidence(type_tag<MessageContent>())], (), nefor.graph.store_port(middle_input), [nefor.graph.store_port(middle_output)])
let middle_base = nefor.graph.node("m-middle", "ordinary", [middle_actor], [], [], middle_input, middle_output)
let result = nefor.graph.output<MessageContent>("a-result")
let tail_input = nefor.graph.port("a-tail", type_tag<MessageContent>(), "test.Value")
let tail_output = nefor.graph.port("a-tail", type_tag<MessageContent>(), "test.Value")
let tail_actor = nefor.graph.actor("a-tail", "test.identity", [type_evidence(type_tag<MessageContent>())], (), nefor.graph.store_port(tail_input), [nefor.graph.store_port(tail_output)])
let tail = nefor.graph.node("a-tail", "ordinary", [tail_actor], [], [], tail_input, tail_output)
let middle = nefor.graph.with_operation(
  nefor.graph.with_operation(middle_base, test_operation("middle-1", get(middle_base, "output"))),
  test_operation("middle-2", get(middle_base, "output")),
)
let start = nefor.graph.with_operation(start_base, test_operation("start-1", get(start_base, "output")))
let first_edge = nefor.graph.edge(start, middle)
let middle_tail = nefor.graph.edge(middle, tail)
let tail_result = nefor.graph.edge(tail, result)
let topology = nefor.graph.graph([first_edge, middle_tail, tail_result, first_edge])
let analysis = nefor.graph.analyze_graph(topology)
let changed_middle = nefor.graph.node_with_operations_and_nodes("m-middle", "ordinary", get(middle, "actors"), get(middle, "routes"), get(middle, "messages"), [test_operation("changed", get(middle, "output"))], get(middle, "nodes"), get(middle, "input"), get(middle, "output"))
let changed_definition = nefor.graph.graph([first_edge, nefor.graph.edge(start, changed_middle), middle_tail, tail_result])
let changed_analysis = nefor.graph.analyze_graph(changed_definition)
let reordered_middle = nefor.graph.with_operation(
  nefor.graph.with_operation(middle_base, test_operation("middle-2", get(middle_base, "output"))),
  test_operation("middle-1", get(middle_base, "output")),
)
let reordered_definition = nefor.graph.graph([first_edge, nefor.graph.edge(start, reordered_middle), middle_tail, tail_result])
let plain = nefor.graph.graph([first_edge, middle_tail, tail_result])
let stored_node_id: fn(nefor.graph.StoredNode) -> String = |candidate| => get(candidate, "id")
let actor_id: fn(nefor.graph.Actor) -> String = |candidate| => get(candidate, "id")
let message_target: fn(nefor.graph.Message) -> String = |candidate| => nefor.graph.endpoint_id(get(get(candidate, "to"), "endpoint"))
let operation_id: fn(nefor.mag.ProgramOperation) -> String = |candidate| => get(candidate, "id")
artifact {
  edge_count: count(get(analysis, "edges")),
  node_ids: map(stored_node_id, get(analysis, "nodes")),
  middle_occurrences: count(core.map.get(get(analysis, "nodes_by_id"), "m-middle")),
  actor_ids: map(actor_id, get(analysis, "actors")),
  message_targets: map(message_target, get(analysis, "messages")),
  rule_ids: map(operation_id, get(analysis, "graph_operations")),
  duplicate_lowers_identically: (=)(canonical(nefor.graph.lower(topology)), canonical(nefor.graph.lower(plain))),
  reordered_definition: validation_message(nefor.graph.validate(reordered_definition, host_input("factory_contracts", type_tag<List<nefor.graph.FactoryContract>>()))),
  changed_rule_ids: map(operation_id, get(changed_analysis, "graph_operations")),
  changed_definition: validation_message(nefor.graph.validate(changed_definition, host_input("factory_contracts", type_tag<List<nefor.graph.FactoryContract>>())))
}
"#,
        contracts(
            json!([{"identity": "test.identity", "type_scheme": {"input_tags": ["test.Value"], "outputs": ["test.Value"]}}]),
        ),
    );

    assert_eq!(artifact["edge_count"], 3);
    assert_eq!(
        artifact["node_ids"],
        json!(["z-start", "m-middle", "a-tail", "a-result"])
    );
    assert_eq!(artifact["middle_occurrences"], 2);
    assert_eq!(
        artifact["actor_ids"],
        json!(["z-start", "m-middle", "a-tail"])
    );
    assert_eq!(artifact["message_targets"], json!(["z-start"]));
    assert_eq!(
        artifact["rule_ids"],
        json!(["start-1", "middle-1", "middle-2"])
    );
    assert_eq!(artifact["duplicate_lowers_identically"], true);
    assert_eq!(artifact["reordered_definition"], "valid");
    assert_eq!(
        artifact["changed_rule_ids"],
        json!(["start-1", "middle-1", "middle-2"])
    );
    let conflict = artifact["changed_definition"].as_str().unwrap();
    assert!(conflict.contains(r#""entity":"stored node""#), "{conflict}");
    assert!(conflict.contains(r#""field":"operations""#), "{conflict}");
    assert!(conflict.contains("changed"), "{conflict}");
}

#[test]
fn shared_node_definitions_union_without_extra_activation() {
    let artifact = run(
        "shared-node-union",
        r#"
import nefor.graph.{}
import nefor.node.{}

let shared = nefor.graph.source("shared", 7)
let left = nefor.node.compose("left", shared, nefor.graph.identity<Int>("left-value"))
let right = nefor.node.compose("right", shared, nefor.graph.identity<Int>("right-value"))
let branched = nefor.node.fanout("branched", left, right)
let result = nefor.graph.output<(Int, Int)>("result")
let topology = nefor.graph.graph([nefor.graph.edge(branched, result)])
let analysis = nefor.graph.analyze_graph(topology)
let actor_id: fn(nefor.graph.Actor) -> String = |candidate| => get(candidate, "id")
artifact {
  actor_ids: map(actor_id, get(analysis, "actors")),
  shared_actor_count: count(filter(((|candidate| => (=)(get(candidate, "id"), "shared")) : fn(nefor.graph.Actor) -> Bool), get(analysis, "actors"))),
  messages: count(get(analysis, "messages")),
  conflicts: get(analysis, "identity_conflicts")
}
"#,
        json!({}),
    );

    assert_eq!(artifact["shared_actor_count"], 1);
    assert_eq!(artifact["messages"], 1);
    assert_eq!(artifact["conflicts"], json!([]));
    let actor_ids = artifact["actor_ids"].as_array().unwrap();
    let distinct = actor_ids.iter().collect::<std::collections::HashSet<_>>();
    assert_eq!(actor_ids.len(), distinct.len());
}

#[test]
fn nested_conflicts_preserve_independent_child_provenance() {
    let artifact = run(
        "nested-identity-conflict-provenance",
        r#"
import nefor.contracts.{}
import nefor.graph.{}
import nefor.node.{}
import nefor.shell.{}

let first_base = nefor.shell.script("operation", nefor.shell.ShellScriptParams {script: "printf first", cwd: ".", timeout: named(nefor.contracts.Timeout, Unlimited, nil)})
let second_base = nefor.shell.script("operation", nefor.shell.ShellScriptParams {script: "printf second", cwd: ".", timeout: named(nefor.contracts.Timeout, Unlimited, nil)})
let first = nefor.graph.node_with_operations_and_nodes("left-child", "ordinary", get(first_base, "actors"), get(first_base, "routes"), get(first_base, "messages"), get(first_base, "operations"), [nefor.graph.logical_node(["left-child"], ["operation"])], get(first_base, "input"), get(first_base, "output"))
let second = nefor.graph.node_with_operations_and_nodes("right-child", "ordinary", get(second_base, "actors"), get(second_base, "routes"), get(second_base, "messages"), get(second_base, "operations"), [nefor.graph.logical_node(["right-child"], ["operation"])], get(second_base, "input"), get(second_base, "output"))
let left_peer = nefor.shell.script("left-peer", nefor.shell.ShellScriptParams {script: "printf peer", cwd: ".", timeout: named(nefor.contracts.Timeout, Unlimited, nil)})
let right_peer = nefor.shell.script("right-peer", nefor.shell.ShellScriptParams {script: "printf peer", cwd: ".", timeout: named(nefor.contracts.Timeout, Unlimited, nil)})
let left = nefor.node.fanout("left-wrapper", first, left_peer)
let right = nefor.node.fanout("right-wrapper", second, right_peer)
let conflicting = nefor.node.fanout("outer", left, right)
artifact(nefor.graph.node_identity_conflicts(conflicting))
"#,
        json!({}),
    );

    let errors = artifact.as_array().unwrap();
    let actor_error = errors
        .iter()
        .filter_map(Value::as_str)
        .find(|error| error.contains(r#""entity":"actor""#))
        .expect("actor conflict");
    let description: Value = serde_json::from_str(
        actor_error
            .strip_prefix("identity conflict: ")
            .expect("conflict prefix"),
    )
    .unwrap();
    assert_eq!(description["identity"], "operation");
    assert_eq!(
        description["first_provenance"]["containing_node"],
        "left-child"
    );
    assert_eq!(
        description["conflicting_provenance"]["containing_node"],
        "right-child"
    );
    assert_eq!(
        description["first_provenance"]["logical_paths"],
        json!([["outer", "left-wrapper", "left-child"]])
    );
    assert_eq!(
        description["conflicting_provenance"]["logical_paths"],
        json!([["outer", "right-wrapper", "right-child"]])
    );
    assert_eq!(
        description["first_provenance"]["composition_path"],
        json!([])
    );
    assert_eq!(
        description["conflicting_provenance"]["composition_path"],
        json!([])
    );
}

#[test]
fn every_identity_class_unions_exact_repeats_and_rejects_divergence() {
    let artifact = run(
        "identity-class-conflicts",
        r#"
import core.map.{}
import nefor.graph.{}
import nefor.mag.{}

let empty_template = nefor.mag.DeltaTemplate {types: (core.map.empty<String, TypeDescriptor>(): Map<String, TypeDescriptor>), actors: [], routes: [], messages: [], nodes: [], actor_reference_relocations: []}
let a = nefor.graph.port("a", type_tag<Int>(), "test.Value")
let b = nefor.graph.port("b", type_tag<Int>(), "test.Value")
let c = nefor.graph.port("c", type_tag<Int>(), "test.Value")
let route_first = nefor.graph.StoredRoute {id: "same-route", from: nefor.graph.store_port(a), to: nefor.graph.store_port(b), transforms: []}
let route_second = nefor.graph.StoredRoute {id: "same-route", from: nefor.graph.store_port(a), to: nefor.graph.store_port(c), transforms: []}
let operation_first = nefor.graph.instantiate_delta_template("same-operation", a, (core.map.empty<String, nefor.mag.TypedCapture>(): Map<String, nefor.mag.TypedCapture>), [], empty_template)
let operation_second = nefor.graph.instantiate_delta_template("same-operation", b, (core.map.empty<String, nefor.mag.TypedCapture>(): Map<String, nefor.mag.TypedCapture>), [], empty_template)
let route_node = nefor.graph.node_with_operations_and_nodes("route-node", "ordinary", [], [route_first, route_second], [], [], [], a, b)
let operation_node = nefor.graph.node_with_operations_and_nodes("operation-node", "ordinary", [], [], [], [operation_first, operation_second], [], a, b)
let logical_node = nefor.graph.node_with_operations_and_nodes("logical-node", "ordinary", [], [], [], [], [nefor.graph.logical_node(["same-path"], ["a"]), nefor.graph.logical_node(["same-path"], ["b"])], a, b)
let message = nefor.graph.typed_message(a, 1)
let other_message = nefor.graph.typed_message(a, 2)
let message_node = nefor.graph.node_with_operations_and_nodes("message-node", "ordinary", [], [], [message, message, other_message], [], [], a, b)
let boundary = nefor.graph.identity<Int>("boundary")
let changed_boundary = nefor.graph.node_with_operations_and_nodes("boundary", "source", get(boundary, "actors"), get(boundary, "routes"), get(boundary, "messages"), get(boundary, "operations"), get(boundary, "nodes"), get(boundary, "input"), get(boundary, "output"))
let start = nefor.graph.source("start", 1)
let result = nefor.graph.output<Int>("result")
let topology = nefor.graph.graph([nefor.graph.edge(start, boundary), nefor.graph.edge(start, changed_boundary), nefor.graph.edge(boundary, result)])
artifact {
  route: nefor.graph.node_identity_conflicts(route_node),
  operation: nefor.graph.node_identity_conflicts(operation_node),
  logical: nefor.graph.node_identity_conflicts(logical_node),
  node: get(nefor.graph.analyze_graph(topology), "identity_conflicts"),
  message_count: count(get(message_node, "messages"))
}
"#,
        json!({}),
    );

    for (field, entity, differing_field) in [
        ("route", "route", "to"),
        ("operation", "operation", "on"),
        ("logical", "logical node", "members"),
    ] {
        let errors = artifact[field].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{field}: {errors:?}");
        let error = errors[0].as_str().unwrap();
        assert!(
            error.contains(&format!(r#""entity":"{entity}""#)),
            "{field}: {error}"
        );
        assert!(
            error.contains(&format!(r#""field":"{differing_field}""#)),
            "{field}: {error}"
        );
    }
    assert_eq!(artifact["message_count"], 2);
}

#[test]
fn anonymous_compound_does_not_shadow_a_child_identity() {
    let artifact = run(
        "compound-shadows-child",
        r#"
import nefor.graph.{}
import nefor.node.{}
let child = nefor.graph.source("same-id", 1)
let next = nefor.graph.identity<Int>("next")
let compound = nefor.node.compose("same-id", child, next)
artifact(nefor.graph.node_identity_conflicts(compound))
"#,
        json!({}),
    );
    let errors = artifact.as_array().unwrap();
    assert_eq!(errors, &[] as &[Value]);
}

#[test]
fn actor_identity_checks_every_execution_contract_field() {
    let artifact = run(
        "actor-contract-fields",
        r#"
import nefor.graph.{}
let input = nefor.graph.port("actor", type_tag<Int>(), "input")
let output = nefor.graph.port("actor", type_tag<Int>(), "output")
let base = nefor.graph.closed_actor("actor", "test.identity", [], 1, nefor.graph.store_port(input), [nefor.graph.store_port(output)], [])
let input_port = get(base, "input")
let output_port = first(get(base, "outputs"))
let string_type = type_evidence(type_tag<String>())
let variants: List<nefor.graph.Actor> = [
  (assoc(base, "factory", "test.other"): nefor.graph.Actor),
  (assoc(base, "type_arguments", [string_type]): nefor.graph.Actor),
  (assoc(base, "params", pack(2)): nefor.graph.Actor),
  (assoc(base, "input", (assoc(input_port, "type", string_type): nefor.graph.StoredPort)): nefor.graph.Actor),
  (assoc(base, "input", (assoc(input_port, "type_id", type_id(string_type)): nefor.graph.StoredPort)): nefor.graph.Actor),
  (assoc(base, "input", (assoc(input_port, "wire", "other"): nefor.graph.StoredPort)): nefor.graph.Actor),
  (assoc(base, "outputs", [(assoc(output_port, "type", string_type): nefor.graph.StoredPort)]): nefor.graph.Actor),
  (assoc(base, "outputs", [(assoc(output_port, "type_id", type_id(string_type)): nefor.graph.StoredPort)]): nefor.graph.Actor),
  (assoc(base, "outputs", [(assoc(output_port, "wire", "other"): nefor.graph.StoredPort)]): nefor.graph.Actor),
  (assoc(base, "templateability", named(nefor.graph.Templateability, Unsupported, "not closed")): nefor.graph.Actor),
  (assoc(base, "templateability", named(nefor.graph.Templateability, Closed, [nefor.graph.Relocation {path: ["peer"], shape: "actor_id"}])): nefor.graph.Actor),
]
let errors: fn(nefor.graph.Actor) -> List<String> = |variant| => nefor.graph.node_identity_conflicts(nefor.graph.node("container", "ordinary", [base, variant], [], [], input, output))
artifact(map(errors, variants))
"#,
        json!({}),
    );
    for (errors, field) in artifact.as_array().unwrap().iter().zip([
        "factory",
        "type_arguments",
        "params",
        "input",
        "input",
        "input",
        "outputs",
        "outputs",
        "outputs",
        "templateability",
        "templateability",
    ]) {
        let errors = errors.as_array().unwrap();
        assert_eq!(errors.len(), 1, "{field}: {errors:?}");
        let diagnostic: Value = serde_json::from_str(
            errors[0]
                .as_str()
                .unwrap()
                .strip_prefix("identity conflict: ")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(diagnostic["entity"], "actor");
        assert_eq!(diagnostic["identity"], "actor");
        assert_eq!(diagnostic["differences"][0]["field"], field);
        assert_ne!(
            diagnostic["differences"][0]["first_value"],
            diagnostic["differences"][0]["conflicting_value"]
        );
    }
}

#[test]
fn route_assignment_sorts_product_buckets_but_lowers_original_route_order() {
    let artifact = run(
        "route-order",
        r#"
type MessageContent {content: String}

import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
import nefor.mag.{}

let start = nefor.graph.source("start", MessageContent {content: "start"})
let emitter_input = nefor.graph.port("emitter", type_tag<MessageContent>(), "test.Value")
let emitter_output = nefor.graph.port("emitter", type_tag<MessageContent>(), "test.Value")
let join_input = nefor.graph.port("join", type_tag<(MessageContent, MessageContent)>(), "test.Value")
let join_output = nefor.graph.port("join", type_tag<MessageContent>(), "test.Value")
let emitter = nefor.graph.actor("emitter", "test.actor", [type_evidence(type_tag<MessageContent>())], (), nefor.graph.store_port(emitter_input), [nefor.graph.store_port(emitter_output)])
let join = nefor.graph.actor("join", "test.actor", [type_evidence(type_tag<MessageContent>())], (), nefor.graph.store_port(join_input), [nefor.graph.store_port(join_output)])
let route_z = nefor.graph.StoredRoute {id: "z-route", from: nefor.graph.store_port(emitter_output), to: nefor.graph.store_port(join_input), transforms: []}
let route_a = nefor.graph.StoredRoute {id: "a-route", from: nefor.graph.store_port(emitter_output), to: nefor.graph.store_port(join_input), transforms: []}
let composite = nefor.graph.node("composite", "ordinary", [emitter, join], [route_z, route_a], ([]: List<nefor.graph.Message>), emitter_input, join_output)
let result = nefor.graph.output<MessageContent>("result")
let topology = nefor.graph.graph([nefor.graph.edge(start, composite), nefor.graph.edge(composite, result)])
let analysis = nefor.graph.analyze_graph(topology)
let input_key = nefor.graph.port_address_key(nefor.graph.store_port(join_input))
let sorted_bucket = core.map.get(get(analysis, "routes_by_input"), input_key)
let lowered = nefor.graph.lower(topology)
let from_emitter: fn(nefor.graph.LowerRoute) -> Bool = |candidate| => (=)(nefor.graph.endpoint_id(get(get(candidate, "from"), "endpoint")), "emitter")
let destinations = filter(from_emitter, get(lowered, "routes"))
let route_id: fn(nefor.graph.StoredRoute) -> String = |candidate| => get(candidate, "id")
let destination_id: fn(nefor.graph.LowerRoute) -> String = |candidate| => get(candidate, "id")
artifact {
  bucket_ids: map(route_id, sorted_bucket),
  lowered_edge_ids: map(destination_id, destinations),
  transforms: map(((|candidate| => get(candidate, "transforms")) : fn(nefor.graph.LowerRoute) -> List<nefor.mag.Transform>), destinations)
}
"#,
        json!({}),
    );

    assert_eq!(artifact["bucket_ids"], json!(["a-route", "z-route"]));
    assert_eq!(artifact["lowered_edge_ids"], json!(["z-route", "a-route"]));
    assert_eq!(artifact["transforms"], json!([[], []]));
}

#[test]
fn route_assignment_uses_each_same_address_destination_descriptor() {
    for (name, product_id, component_id) in [
        ("product-route-first", "a-product", "z-component"),
        ("component-route-first", "z-product", "a-component"),
    ] {
        let source = format!(
            r#"
import nefor.graph.{{}}

type A {{value: Int}}
type B {{value: String}}

let source_a = nefor.graph.port("source-a", type_tag<A>(), "test.Value")
let source_b = nefor.graph.port("source-b", type_tag<B>(), "test.Value")
let product_input = nefor.graph.port("join", type_tag<(A, B)>(), "test.Value")
let component_input = nefor.graph.port("join", type_tag<B>(), "test.Value")
let product_route = nefor.graph.StoredRoute {{
  id: "{product_id}",
  from: nefor.graph.store_port(source_a),
  to: nefor.graph.store_port(product_input),
  transforms: []
}}
let component_route = nefor.graph.StoredRoute {{
  id: "{component_id}",
  from: nefor.graph.store_port(source_b),
  to: nefor.graph.store_port(component_input),
  transforms: []
}}
artifact(nefor.graph.lower_delta(nefor.graph.delta(
  ([]: List<nefor.graph.Actor>),
  [product_route, component_route],
  ([]: List<nefor.graph.Message>),
  ([]: List<String>),
)))
"#
        );

        let lowered = run(name, &source, json!({}));
        let routes = lowered["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 2, "{name}: {lowered}");
        assert_eq!(routes[0]["id"], product_id);
        assert_eq!(routes[1]["id"], component_id);
        assert_eq!(routes[0]["to"]["type"]["kind"], "product");
        assert_eq!(routes[1]["to"]["type"]["kind"], "named");
        assert_eq!(routes[0]["transforms"], json!([]));
        assert_eq!(routes[1]["transforms"], json!([]));
    }
}

#[test]
fn indexed_reachability_handles_cycles_and_preserves_dead_path_diagnostics() {
    let artifact = run(
        "reachability",
        r#"
type MessageContent {content: String}

import core.validated.{}
import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
import nefor.mag.{}

let test_operation<T>: fn(String, nefor.graph.Port<T>) -> nefor.mag.ProgramOperation = |id, on| =>
  nefor.graph.instantiate_delta_template(id, on,
    (core.map.empty<String, nefor.mag.TypedCapture>(): Map<String, nefor.mag.TypedCapture>),
    ([]: List<nefor.mag.Expression>),
    nefor.mag.DeltaTemplate {
      types: (core.map.empty<String, TypeDescriptor>(): Map<String, TypeDescriptor>),
      actors: [], routes: [], messages: [], nodes: [], actor_reference_relocations: []
    })
let validation_message: fn(core.validated.Validated<String, nefor.graph.Graph>) -> String = |checked| =>
  match checked {
    case Valid(accepted) => "valid",
    case Invalid(rejected) => first(get(rejected, "errors")),
  }
let contracts = host_input("factory_contracts", type_tag<List<nefor.graph.FactoryContract>>())
let start = nefor.graph.source("start", (MessageContent {content: "start"}, MessageContent {content: "start"}))
let cycle_input = nefor.graph.port("cycle-a", type_tag<(MessageContent, MessageContent)>(), "test.Value")
let cycle_output = nefor.graph.port("cycle-a", type_tag<(MessageContent, MessageContent)>(), "test.Value")
let cycle_actor = nefor.graph.actor("cycle-a", "test.cycle", [type_evidence(type_tag<(MessageContent, MessageContent)>())], (), nefor.graph.store_port(cycle_input), [nefor.graph.store_port(cycle_output)])
let cycle_a = nefor.graph.node("cycle-a", "ordinary", [cycle_actor], ([]: List<nefor.graph.StoredRoute>), ([]: List<nefor.graph.Message>), cycle_input, cycle_output)
let cycle_b_input = nefor.graph.port("cycle-b", type_tag<(MessageContent, MessageContent)>(), "test.Value")
let cycle_b_output = nefor.graph.port("cycle-b", type_tag<(MessageContent, MessageContent)>(), "test.Value")
let cycle_b_actor = nefor.graph.actor("cycle-b", "test.cycle", [type_evidence(type_tag<(MessageContent, MessageContent)>())], (), nefor.graph.store_port(cycle_b_input), [nefor.graph.store_port(cycle_b_output)])
let cycle_b = nefor.graph.node("cycle-b", "ordinary", [cycle_b_actor], [], [], cycle_b_input, cycle_b_output)
let result = nefor.graph.output<(MessageContent, MessageContent)>("result")
let reachable_cycle = nefor.graph.graph([nefor.graph.edge(start, cycle_a), nefor.graph.edge(cycle_a, cycle_b), nefor.graph.edge(cycle_b, cycle_a), nefor.graph.edge(cycle_b, result)])
let orphan_a_input = nefor.graph.port("orphan-a", type_tag<MessageContent>(), "test.Value")
let orphan_a_output = nefor.graph.port("orphan-a", type_tag<MessageContent>(), "test.Value")
let orphan_a_actor = nefor.graph.actor("orphan-a", "test.cycle", [type_evidence(type_tag<MessageContent>())], (), nefor.graph.store_port(orphan_a_input), [nefor.graph.store_port(orphan_a_output)])
let orphan_a = nefor.graph.node("orphan-a", "ordinary", [orphan_a_actor], [], [], orphan_a_input, orphan_a_output)
let orphan_b_input = nefor.graph.port("orphan-b", type_tag<MessageContent>(), "test.Value")
let orphan_b_output = nefor.graph.port("orphan-b", type_tag<MessageContent>(), "test.Value")
let orphan_b_actor = nefor.graph.actor("orphan-b", "test.cycle", [type_evidence(type_tag<MessageContent>())], (), nefor.graph.store_port(orphan_b_input), [nefor.graph.store_port(orphan_b_output)])
let orphan_b = nefor.graph.node("orphan-b", "ordinary", [orphan_b_actor], [], [], orphan_b_input, orphan_b_output)
let disconnected_cycle = nefor.graph.graph([nefor.graph.edge(start, result), nefor.graph.edge(orphan_a, orphan_b), nefor.graph.edge(orphan_b, orphan_a)])
let branch_input = nefor.graph.port("branch", type_tag<MessageContent>(), "test.Value")
let branch_output = nefor.graph.port("branch", type_tag<MessageContent>(), "test.Value")
let branch_actor = nefor.graph.actor("branch", "test.cycle", [type_evidence(type_tag<MessageContent>())], (), nefor.graph.store_port(branch_input), [nefor.graph.store_port(branch_output)])
let branch_base = nefor.graph.node("branch", "ordinary", [branch_actor], [], [], branch_input, branch_output)
let branch = nefor.graph.with_operation(branch_base, test_operation("observe-branch", get(branch_base, "output")))
let dead_start = nefor.graph.source("dead-start", MessageContent {content: "dead-start"})
let dead_branch = nefor.graph.graph([nefor.graph.edge(start, result), nefor.graph.edge(dead_start, branch)])
artifact {
  reachable_cycle: validation_message(nefor.graph.validate(reachable_cycle, contracts)),
  disconnected_cycle: validation_message(nefor.graph.validate(disconnected_cycle, contracts)),
  dead_branch: validation_message(nefor.graph.validate(dead_branch, contracts))
}
"#,
        contracts(json!([
            {
                "identity": "test.cycle",
                "type_scheme": {
                    "input_tags": ["test.Value"],
                    "outputs": ["test.Value"]
                }
            }
        ])),
    );

    assert_eq!(artifact["reachable_cycle"], "valid");
    assert_eq!(
        artifact["disconnected_cycle"],
        "every node must be reachable from a root node whose input accepts Unit"
    );
    assert_eq!(
        artifact["dead_branch"],
        "every node must have a path to the output node"
    );
}

#[test]
fn duplicate_precedence_contract_selection_and_sequence_order_are_stable() {
    let artifact = run(
        "duplicates-and-sequence",
        r#"
type MessageContent {content: String}

import core.validated.{}
import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
import nefor.node.{}

let validation_message: fn(core.validated.Validated<String, nefor.graph.Graph>) -> String = |checked| =>
  match checked {
    case Valid(accepted) => "valid",
    case Invalid(rejected) => first(get(rejected, "errors")),
  }
let start = nefor.graph.source("start", MessageContent {content: "start"})
let result = nefor.graph.output<MessageContent>("result")
let duplicated_result = nefor.graph.node("result", "output", concat(get(start, "actors"), get(result, "actors")), get(result, "routes"), get(result, "messages"), get(result, "input"), get(result, "output"))
let duplicate_actor_graph = nefor.graph.graph([nefor.graph.edge(start, duplicated_result)])
let no_output = nefor.graph.graph([nefor.graph.edge(start, nefor.graph.identity<MessageContent>("ordinary"))])
let first_contract = nefor.graph.FactoryContract {identity: "nefor.factory.source", type_scheme: nefor.graph.FactoryTypeScheme {input_tags: ["wrong"], outputs: ["nefor.graph.Value"]}}
let second_contract = nefor.graph.FactoryContract {identity: "nefor.factory.source", type_scheme: nefor.graph.FactoryTypeScheme {input_tags: ["mag.Unit"], outputs: ["nefor.graph.Value"]}}
let valid_graph = nefor.graph.graph([nefor.graph.edge(start, result)])
let first_child = nefor.graph.source("first-child", MessageContent {content: "first"})
let second_child = nefor.graph.source("second-child", MessageContent {content: "second"})
let sequence = nefor.node.sequence([first_child, second_child])
let actor_id: fn(nefor.graph.Actor) -> String = |candidate| => get(candidate, "id")
artifact {
  duplicate_actor: validation_message(nefor.graph.validate(duplicate_actor_graph, host_input("factory_contracts", type_tag<List<nefor.graph.FactoryContract>>()))),
  no_output: validation_message(nefor.graph.validate(no_output, host_input("factory_contracts", type_tag<List<nefor.graph.FactoryContract>>()))),
  first_contract: validation_message(nefor.graph.validate(valid_graph, [first_contract, second_contract])),
  sequence_id: get(sequence, "id"),
  sequence_actors: map(actor_id, get(sequence, "actors")),
  sequence_output: nefor.graph.store_boundary(get(sequence, "output"))
}
"#,
        contracts(json!([])),
    );

    assert_eq!(
        artifact["duplicate_actor"],
        "every runtime actor must belong to exactly one logical node"
    );
    assert_eq!(
        artifact["no_output"],
        "graph needs exactly one concrete output node"
    );
    assert!(artifact["first_contract"]
        .as_str()
        .unwrap()
        .contains("rejects input wire"));
    assert_eq!(
        artifact["sequence_actors"],
        json!(["first-child", "second-child"])
    );
    assert_eq!(
        artifact["sequence_output"]["leaves"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        artifact["sequence_output"]["leaves"][0]["steps"][0]["value"]["slot"],
        0
    );
    assert_eq!(
        artifact["sequence_output"]["leaves"][1]["steps"][0]["value"]["slot"],
        1
    );
}

#[test]
fn nefor_artifact_emits_exact_versioned_program_and_delta_envelopes() {
    let program = run(
        "program-envelope",
        r#"
type MessageContent {content: String}

import nefor.artifact.{}
import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
let start = nefor.graph.source("start", MessageContent {content: "hello"})
let result = nefor.graph.output<MessageContent>("result")
let close: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, result)])
nefor.artifact.compile(close)
"#,
        contracts(json!([])),
    );
    assert_eq!(program["format"], "nefor.mag");
    assert_eq!(program["version"], 4);
    assert_eq!(program["kind"], "program");
    assert_eq!(program["program"]["operations"], json!([]));
    let initial = &program["program"]["initial"];
    assert!(initial.get("rules").is_none());
    assert!(initial.get("result").is_some());
    assert!(program.get("actors").is_none());

    let delta = run(
        "delta-envelope",
        r#"
import nefor.artifact.{}
import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
nefor.artifact.delta(nefor.graph.delta([], [], [], []))
"#,
        json!({}),
    );
    assert_eq!(delta["format"], "nefor.mag");
    assert_eq!(delta["version"], 4);
    assert_eq!(delta["kind"], "delta");
    assert!(delta["delta"].get("result").is_none());
    assert!(delta["delta"].get("operations").is_none());
    assert!(delta["delta"].get("rules").is_none());
}

#[test]
fn compile_graph_closes_one_terminal_through_existing_graph_values() {
    let explicit = run(
        "compile-graph-explicit",
        r#"
import nefor.artifact.{}
import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
let start = nefor.graph.source("start", "hello")
let result = nefor.graph.output_for("result", start)
let close: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| =>
  nefor.graph.add_edges(graph, [nefor.graph.edge(start, result)])
nefor.artifact.compile(close)
"#,
        contracts(json!([])),
    );
    let convenient = run(
        "compile-graph-convenient",
        r#"
import nefor.artifact.{}
import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
let start = nefor.graph.source("start", "hello")
nefor.artifact.compile_graph(start)
"#,
        contracts(json!([])),
    );

    assert_eq!(convenient, explicit);
    assert_eq!(convenient["format"], "nefor.mag");
    assert_eq!(convenient["version"], 4);
    assert_eq!(convenient["kind"], "program");
    assert_eq!(
        convenient["program"]["initial"]["actors"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        convenient["program"]["initial"]["result"]["from"]["leaves"][0]["port"]["endpoint"]
            ["value"]["id"],
        "start"
    );
    assert!(convenient["program"]["initial"].get("junctions").is_none());

    let collision = run(
        "compile-graph-collision",
        r#"
import nefor.artifact.{}
import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
let result = nefor.graph.source("result", "hello")
nefor.artifact.compile_graph(result)
"#,
        contracts(json!([])),
    );
    assert_eq!(
        collision["program"]["initial"]["result"]["from"]["leaves"][0]["port"]["endpoint"]["value"]
            ["id"],
        "result"
    );
}

#[test]
fn nefor_facade_supports_one_import_shell_and_map_error() {
    let artifact = run(
        "nefor-facade-shell",
        r#"
import nefor
compile_graph(script("hello", ShellScriptParams {script: "printf 'hello\\n'", cwd: ".", timeout: named(Timeout, Milliseconds, 30000)}))
"#,
        contracts(json!([
            {"identity": "nefor.factory.shell-script", "type_scheme": {"input_tags": ["nefor.process.Input"], "outputs": ["nefor.process.Result"]}}
        ])),
    );

    assert_eq!(artifact["program"]["initial"]["actors"][0]["id"], "hello");
    assert_eq!(
        artifact["program"]["initial"]["actors"][0]["params"]["value"]["timeout"]["milliseconds"],
        30000
    );

    let mapped = run(
        "nefor-facade-map-error",
        r#"
import core.types.{}
import nefor
let input = identity<core.types.Result<String, Int>>("input")
let mapper = identity<String>("mapper")
artifact(get(map_error(input, mapper), "id"))
"#,
        json!({}),
    );
    assert_eq!(mapped, json!("input"));
}

#[test]
fn only_exact_unit_ports_are_root_capable() {
    let artifact = run(
        "exact-unit-root",
        r#"
import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
type Maybe = Ready(Unit) | Waiting(String)
let exact = nefor.graph.store_port(nefor.graph.port("exact", type_tag<Unit>(), "wire"))
let sum = nefor.graph.store_port(nefor.graph.port("sum", type_tag<Maybe>(), "wire"))
let product = nefor.graph.store_port(nefor.graph.port("product", type_tag<(Unit, String)>(), "wire"))
artifact {exact: nefor.graph.unit_input(exact), sum: nefor.graph.unit_input(sum), product: nefor.graph.unit_input(product)}
"#,
        json!({}),
    );
    assert_eq!(
        artifact,
        json!({"exact": true, "sum": false, "product": false})
    );
}

#[test]
fn empty_sequence_is_an_anonymous_empty_list_transformation() {
    let artifact = run(
        "empty-structural-sequence",
        r#"
import nefor.graph.{}
import nefor.node.{}
let empty = nefor.node.sequence(([]: List<nefor.graph.Node<String, Int>>))
artifact(empty)
"#,
        json!({}),
    );
    assert_eq!(artifact["actors"], json!([]));
    assert_eq!(artifact["nodes"], json!([]));
    assert_eq!(
        artifact["output"]["boundary"]["through"][0]["steps"][0]["constructor"],
        "EmptyList"
    );
}

#[test]
fn exact_unit_root_cache_preserves_actual_activation_evidence() {
    use nefor_mag::project_cache::{build_with_syntax, CachePolicy, CacheStatus};
    let root = workspace("exact-unit-cache");
    let roots = [std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mag/lib")];
    fs::write(
        root.join("main.mag"),
        r#"
import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
import nefor.artifact.{}
type Arm {text: String}
let start = nefor.graph.identity<Unit>("start")
let result = nefor.graph.output_for("result", start)
let close: fn(nefor.graph.Graph) -> nefor.graph.Graph = |g| => nefor.graph.add_edges(g, [nefor.graph.edge(start, result)])
nefor.artifact.compile(close)
"#,
    )
    .unwrap();
    let compile = |policy| {
        build_with_syntax(
            nefor_mag::FileCompileRequest {
                source_dir: &root,
                entry: "main.mag",
                inputs: contracts(json!([])),
                module_roots: &roots,
                options: Default::default(),
            },
            1,
            policy,
            None,
            SyntaxMode::New,
        )
        .unwrap()
    };
    let miss = compile(CachePolicy::Use);
    let hit = compile(CachePolicy::Use);
    let bypass = compile(CachePolicy::Bypass);
    assert!(matches!(miss.cache.status, CacheStatus::Miss));
    assert!(matches!(hit.cache.status, CacheStatus::Hit));
    assert!(matches!(bypass.cache.status, CacheStatus::Bypass));
    assert_eq!(miss.bytes, hit.bytes);
    assert_eq!(miss.bytes, bypass.bytes);
    let artifact: Value = serde_json::from_slice(&hit.bytes).unwrap();
    let initial = &artifact["program"]["initial"];
    assert_eq!(initial["messages"], json!([]));
    assert_eq!(initial["result"]["from"]["leaves"], json!([]));
    assert_eq!(
        initial["result"]["from"]["through"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(initial["result"]["from"]["through"][0]["steps"], json!([]));
    assert_eq!(
        initial["result"]["from"]["type"],
        json!({"kind":"primitive","name":"Unit"})
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn delta_bootstrap_uses_exact_unit_and_full_input_address() {
    let artifact = run(
        "delta-bootstrap-address",
        r#"
import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
type Arm {text: String}
let unit = nefor.graph.source("unit", nil)
let sum = nefor.graph.source("sum", nil)
let base = nefor.graph.merge_delta(nefor.graph.node_delta(unit), nefor.graph.node_delta(sum))
let actual = get(unit, "input")
let unrelated = nefor.graph.port("unit", type_tag<Unit>(), "other-wire")
let routed = nefor.graph.delta_route(base, get(sum, "output"), actual)
artifact {
  base: nefor.graph.lower_delta(base),
  authored: nefor.graph.lower_delta(nefor.graph.delta_message(base, actual, ())),
  unrelated: nefor.graph.lower_delta(nefor.graph.delta_message(base, unrelated, ())),
  routed: nefor.graph.lower_delta(routed)
}
"#,
        json!({}),
    );
    // Every unfed exact-Unit delta endpoint activates once. An authored message
    // or a full-address route suppresses only its exact target.
    assert_eq!(artifact["base"]["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        artifact["authored"]["messages"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        artifact["authored"]["messages"][0]["to"]["endpoint"]["value"]["id"],
        "unit"
    );
    assert_eq!(
        artifact["unrelated"]["messages"].as_array().unwrap().len(),
        3
    );
    assert_eq!(
        artifact["unrelated"]["messages"][0]["content"]["value"]["kind"],
        "other-wire"
    );
    assert_eq!(artifact["routed"]["messages"].as_array().unwrap().len(), 1);
}

#[test]
fn indexed_output_diagnostics_include_explicit_operations_and_each_port() {
    let artifact = run(
        "operation-diagnostics",
        r#"
import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
import nefor.mag.{}
import core.validated.{}
type Arm {text: String}
type Other {number: Int}
type EmptyParams {}
let input = nefor.graph.port("worker", type_tag<Unit>(), "in")
let one = nefor.graph.port("worker", type_tag<Arm>(), "one")
let two = nefor.graph.port("worker", type_tag<Arm>(), "two")
let three = nefor.graph.port("worker", type_tag<Arm>(), "three")
let worker = nefor.graph.actor("worker", "custom", [], EmptyParams {}, nefor.graph.store_port(input), [nefor.graph.store_port(one), nefor.graph.store_port(two), nefor.graph.store_port(three)])
let factory_contract = nefor.graph.FactoryContract {identity: "custom", type_scheme: nefor.graph.FactoryTypeScheme {input_tags: ["in"], outputs: ["one", "two", "three"]}}
let node = nefor.graph.node("worker", "ordinary", [worker], [], [], input, three)
let result = nefor.graph.output<Arm>("result")
let graph = nefor.graph.graph([nefor.graph.edge(node, result)])
let on = nefor.graph.port("worker", type_tag<Arm>(), "one")
let operation = nefor.graph.instantiate_delta_template("explicit-arm", on,
  (core.map.empty<String, nefor.mag.TypedCapture>(): Map<String, nefor.mag.TypedCapture>), [],
  nefor.mag.DeltaTemplate {
    types: (core.map.empty<String, TypeDescriptor>(): Map<String, TypeDescriptor>),
    actors: [], routes: [], messages: [], nodes: [], actor_reference_relocations: []
  })
let validation = nefor.graph.validate_with_operations(graph, [operation], [factory_contract])
artifact(match validation {
  case Invalid(invalid) => get(invalid, "errors"),
  case Valid(valid) => [],
})
"#,
        json!({}),
    );
    let errors = artifact.as_array().unwrap();
    assert_eq!(errors.len(), 1, "{artifact}");
    let details: Vec<Value> = errors
        .iter()
        .map(|error| {
            serde_json::from_str(
                error
                    .as_str()
                    .unwrap()
                    .strip_prefix("output coverage failed: ")
                    .unwrap(),
            )
            .unwrap()
        })
        .collect();
    assert_eq!(details[0]["output"], "worker.two");
    assert_eq!(details[0]["available_handlers"], json!([]));
    let handlers = details[0]["available_handlers"].as_array().unwrap();
    assert_eq!(handlers.len(), 0);
}

#[test]
fn input_diagnostics_keep_raw_occurrences_and_actual_message_evidence() {
    let artifact = run(
        "input-diagnostic-order",
        r#"
import nefor.graph.{}
import nefor.contracts.{}
import core.map.{}
let start = nefor.graph.source("start", ())
let target_input = nefor.graph.port("target", type_tag<(Unit, Unit)>(), "in")
let target_output = nefor.graph.port("target", type_tag<Unit>(), "out")
let target_actor = nefor.graph.actor("target", "test.target", [], (), nefor.graph.store_port(target_input), [nefor.graph.store_port(target_output)])
let target = nefor.graph.node("target", "ordinary", [target_actor], [], [], target_input, target_output)
let first_route = (assoc(nefor.graph.stored_route(nefor.graph.port("raw-first", type_tag<Unit>(), "out"), get(target, "input")), "id", "z"): nefor.graph.StoredRoute)
let second = (assoc(nefor.graph.stored_route(nefor.graph.port("raw-second", type_tag<Unit>(), "out"), get(target, "input")), "id", "a"): nefor.graph.StoredRoute)
let third = (assoc(second, "id", "b"): nefor.graph.StoredRoute)
let node = nefor.graph.node_with_operations_and_nodes("wrapper", "ordinary", get(target, "actors"), [first_route, second, third], [], [], get(target, "nodes"), get(target, "input"), get(target, "output"))
let graph = nefor.graph.graph([nefor.graph.edge(start, node)])
let analysis = nefor.graph.analyze_graph(graph)
let actor = first(get(target, "actors"))
let input = get(actor, "input")
let message = (assoc(nefor.graph.stored_message(input, nefor.graph.MessageContent<Unit> {kind: "nefor.graph.Value", value: ()}), "semantic_type", type_evidence(type_tag<Unit>())): nefor.graph.Message)
let with_message = (assoc(analysis, "messages_by_input", core.map.put(get(analysis, "messages_by_input"), nefor.graph.port_address_key(input), [message])): nefor.graph.GraphAnalysis)
let route_id: fn(nefor.graph.StoredRoute) -> String = |r| => get(r, "id")
artifact {
  sources: nefor.graph.actor_input_sources(with_message, actor),
  covered: nefor.graph.actor_input_coverage_valid(with_message, actor),
  assignment_order: map(route_id, core.map.get_or(get(analysis, "routes_by_input"), nefor.graph.port_address_key(input), ([]: List<nefor.graph.StoredRoute>)))
}
"#,
        json!({}),
    );
    let sources = artifact["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 5);
    for (index, expected) in [
        "raw-first.out",
        "raw-second.out",
        "raw-second.out",
        "start.nefor.graph.Value",
    ]
    .iter()
    .enumerate()
    {
        let source: Value = serde_json::from_str(sources[index].as_str().unwrap()).unwrap();
        assert_eq!(source["from"], *expected);
    }
    assert_eq!(
        sources[4],
        "initial message type {\"kind\":\"primitive\",\"name\":\"Unit\"}"
    );
    assert_eq!(artifact["covered"], false);
    let order = artifact["assignment_order"].as_array().unwrap();
    assert_eq!(&order[..3], &[json!("a"), json!("b"), json!("z")]);
}

#[test]
fn core_nefor_api_uses_inferred_evidence_explicit_ids_and_unit_sequences() {
    let artifact = run(
        "core-nefor-api-reshape",
        r#"
import core.map.{}
import core.types.{}
import nefor.actors.{}
import nefor.human.{}
import nefor.contracts.{}
import nefor.dynamic.{}
import nefor.graph.{}
import nefor.node.{}
import nefor.result.{}

type Input {value: Int}
type Output {value: String}

let source = nefor.graph.source("source", Input {value: 1})
let int_identity = nefor.graph.identity<Int>("int")
let int_identity_2 = nefor.graph.identity<Int>("int-2")
let string_identity = nefor.graph.identity<String>("string")
let unit_source = nefor.graph.source("unit-source", nil)
let explicit_compose = nefor.node.compose("compose", int_identity, int_identity_2)
let operator_compose = nefor.node.`>>>`(int_identity, int_identity_2)
let explicit_then = nefor.node.then("then", int_identity, unit_source)
let operator_then = nefor.node.`*>`(int_identity, unit_source)
let operator_before = nefor.node.`<*`(int_identity, unit_source)
let explicit_fanout = nefor.node.fanout("fanout", int_identity, int_identity_2)
let operator_fanout = nefor.node.`&&&`(int_identity, int_identity_2)
let explicit_parallel = nefor.node.parallel("parallel", int_identity, string_identity)
let operator_parallel = nefor.node.`***`(int_identity, string_identity)
let explicit_choose = nefor.node.choose("choose", int_identity, string_identity)
let operator_choose = nefor.node.`+++`(int_identity, string_identity)
let first_source = nefor.graph.source("first", 1)
let second_source = nefor.graph.source("second", 2)
let nonempty_sequence = nefor.node.sequence([first_source, second_source])
let repeated_sequence = nefor.node.sequence([first_source, second_source])
let reordered_sequence = nefor.node.sequence([second_source, first_source])
let collision_sequence_left = nefor.node.sequence([nefor.graph.source("a", 1), nefor.graph.source("bsequencec", 2)])
let collision_sequence_right = nefor.node.sequence([nefor.graph.source("asequenceb", 1), nefor.graph.source("c", 2)])

let result_input = nefor.graph.identity<core.types.Result<String, Int>>("result-input")
let result_value = nefor.graph.identity<Int>("result-value")
let lifted = nefor.result.lift<Int, String, Int>("lifted", result_value)
let mapped = nefor.result.map(result_input, result_value)
let error_mapper = nefor.node.discard<String>("error-mapper")
let mapped_error = nefor.result.map_error(result_input, error_mapper)
let bound = nefor.result.and_then("bound", result_input, lifted)
let operator_bound = nefor.result.`>=>`(result_input, lifted)
let policy = named(nefor.contracts.ToolApprovalPolicy, Rules, nefor.contracts.ToolApprovalRules {rules: core.map.insert((core.map.empty<String, String>(): Map<String, String>), "bash", "deny")})
let no_policy = named(nefor.contracts.ToolApprovalPolicy, Default, nil)
let resolve_model: fn(String) -> nefor.actors.AuthoredModel = |model| => named(nefor.actors.AuthoredModel, ResolvedModel, nefor.actors.ResolvedModel {provider: "test", model: model, reasoning_effort: nefor.actors.no_reasoning_effort})
let agent = nefor.actors.agent<String, Input, Output>("agent", resolve_model, nefor.actors.AgentConfig<String> {model: "mock", system: "test", tools: [], tool_approval_policy: policy, max_corrections: 1})
let dynamic_agent = nefor.actors.agent<String, Input, nefor.dynamic.DynamicList<Output>>("dynamic-agent", resolve_model, nefor.actors.AgentConfig<String> {model: "mock", system: "test", tools: [], tool_approval_policy: no_policy, max_corrections: 1})
let approval = nefor.human.approval_gate("approval", nefor.human.ApprovalConfig {prompt: "Approve?"})
let retry = nefor.actors.retry_gate<Input>("retry", nefor.actors.RetryGateConfig {max_retries: 2})
let agent_actors: List<nefor.graph.Actor> = get(agent, "actors")
let run_tool_actor = first(filter(((|candidate| => (=)(get(candidate, "id"), "agent.run-tool")): fn(nefor.graph.Actor) -> Bool), agent_actors))

artifact {
  source_type_matches: (=)(canonical(type_id(type_evidence(get(get(source, "output"), "type")))), canonical(type_id(type_evidence(type_tag<Input>())))),
  compose_ids: [get(explicit_compose, "id"), get(operator_compose, "id")],
  then_ids: [get(explicit_then, "id"), get(operator_then, "id"), get(operator_before, "id")],
  fanout_ids: [get(explicit_fanout, "id"), get(operator_fanout, "id")],
  parallel_ids: [get(explicit_parallel, "id"), get(operator_parallel, "id")],
  choose_ids: [get(explicit_choose, "id"), get(operator_choose, "id")],
  sequence_ids: [get(nonempty_sequence, "id"), get(repeated_sequence, "id"), get(reordered_sequence, "id"), get(collision_sequence_left, "id"), get(collision_sequence_right, "id")],
  result_ids: [get(lifted, "id"), get(mapped, "id"), get(mapped_error, "id"), get(bound, "id"), get(operator_bound, "id")],
  map_error_output_matches: (=)(canonical(type_id(type_evidence(get(get(mapped_error, "output"), "type")))), canonical(type_id(type_evidence(type_tag<core.types.Result<Unit, Int>>())))),
  actor_ids: [get(agent, "id"), get(dynamic_agent, "id"), get(approval, "id"), get(retry, "id")],
  run_tool_params: get(run_tool_actor, "params"),
  tool_approval_policy: policy
}
"#,
        json!({}),
    );

    assert_eq!(artifact["source_type_matches"], json!(true));
    assert_eq!(artifact["compose_ids"], json!(["compose", "int"]));
    assert_eq!(artifact["then_ids"], json!(["then", "int", "int"]));
    assert_eq!(artifact["fanout_ids"], json!(["fanout", "int"]));
    assert_eq!(artifact["parallel_ids"], json!(["parallel", "int"]));
    assert_eq!(artifact["choose_ids"], json!(["choose", "int"]));
    assert_eq!(
        artifact["sequence_ids"],
        json!(["first", "first", "second", "a", "asequenceb"])
    );
    assert_eq!(
        artifact["result_ids"],
        json!([
            "lifted",
            "result-input",
            "result-input",
            "bound",
            "result-input"
        ])
    );
    assert_eq!(artifact["map_error_output_matches"], json!(true));
    assert_eq!(
        artifact["actor_ids"],
        json!(["agent", "dynamic-agent", "approval", "retry"])
    );
    assert_eq!(
        artifact["tool_approval_policy"],
        json!({"constructor": "Rules", "value": {"rules": {"bash": "deny"}}})
    );
    assert_eq!(
        artifact["run_tool_params"]["value"]["tool_approval_policy"],
        json!({"rules": {"bash": "deny"}})
    );
}

#[test]
fn generic_list_inference_supports_unit_sequence_with_error_union() {
    for (name, nodes) in [
        ("inline", "[runtime, configs]"),
        ("bound", "workers"),
        ("annotated", "([runtime, configs]: List<nefor.graph.Node<Unit, core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>>)"),
    ] {
        let source = format!(r#"
type InvestigationInput {{prompt: String}}

import core.types.{{}}
import nefor.contracts.{{}}
import nefor.graph.{{}}
import nefor.node.{{}}
let agent_shaped: fn(String) -> nefor.graph.Node<Unit, core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>> = |id| =>
  nefor.graph.node(id, "test", [], [], [],
    nefor.graph.port(id, type_tag<Unit>(), "mag.Unit"),
    nefor.graph.port(id, type_tag<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>(), "nefor.graph.Value"))
let runtime = agent_shaped("runtime")
let configs = agent_shaped("configs")
let workers = [runtime, configs]
let task = nefor.graph.source("task", InvestigationInput {{prompt: "Investigate"}})
let work = nefor.node.then("task-traces", task, nefor.node.sequence({nodes}))
let actual_id = str(type_id(type_evidence(get(get(work, "output"), "type"))))
let expected_id = str(type_id(type_evidence(type_tag<List<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>>())))
artifact((=)(actual_id, expected_id))
"#);
        let artifact = run(&format!("generic-list-{name}"), &source, json!({}));
        assert_eq!(artifact, json!(true), "{name}");
    }
}

#[test]
fn agent_output_contract_recognizes_only_nefor_dynamic_list_and_preserves_other_outputs() {
    let artifact = run(
        "agent-output-contracts",
        r#"
import core.types.{}
import nefor.actors.{}
import nefor.human.{}
import nefor.contracts.{}
import nefor.dynamic.{}
import nefor.graph.{}

type Nested {label: String}
type Record {nested: Nested}
type RecordAlias = Record
type DynamicList<T> {value: T}

let resolve: fn(String) -> nefor.actors.AuthoredModel = |model| => named(nefor.actors.AuthoredModel, ResolvedModel, nefor.actors.ResolvedModel {provider: "test", model: model, reasoning_effort: nefor.actors.no_reasoning_effort})
let config = nefor.actors.AgentConfig<String> {model: "mock", system: "test", tools: [], tool_approval_policy: named(nefor.contracts.ToolApprovalPolicy, Default, nil), max_corrections: 1}
let dynamic = nefor.actors.agent<String, Unit, nefor.dynamic.DynamicList<RecordAlias>>("dynamic", resolve, config)
let lookalike = nefor.actors.agent<String, Unit, DynamicList<Record>>("lookalike", resolve, config)
let ordinary = nefor.actors.agent<String, Unit, Record>("ordinary", resolve, config)
let text = nefor.actors.agent<String, Unit, nefor.contracts.TextAnswer>("text", resolve, config)
let actor_for: fn(nefor.graph.Node<Unit, core.types.Result<nefor.contracts.AgentError, nefor.dynamic.DynamicList<RecordAlias>>>, String) -> nefor.graph.Actor = |node, id| => first(filter(((|actor| => (=)(get(actor, "id"), id)): fn(nefor.graph.Actor) -> Bool), get(node, "actors")))
let dynamic_actor = actor_for(dynamic, "dynamic.llm")
let lookalike_actor = first(filter(((|actor| => (=)(get(actor, "id"), "lookalike.llm")): fn(nefor.graph.Actor) -> Bool), get(lookalike, "actors")))
let ordinary_actor = first(filter(((|actor| => (=)(get(actor, "id"), "ordinary.llm")): fn(nefor.graph.Actor) -> Bool), get(ordinary, "actors")))
let text_actor = first(filter(((|actor| => (=)(get(actor, "id"), "text.llm")): fn(nefor.graph.Actor) -> Bool), get(text, "actors")))
artifact {
  dynamic_factory: get(dynamic_actor, "factory"),
  dynamic_params: get(dynamic_actor, "params"),
  lookalike_factory: get(lookalike_actor, "factory"),
  lookalike_params: get(lookalike_actor, "params"),
  ordinary_params: get(ordinary_actor, "params"),
  text_factory: get(text_actor, "factory")
}
"#,
        json!({}),
    );

    assert_eq!(
        artifact["dynamic_factory"],
        "nefor.factory.structured-output"
    );
    assert_eq!(artifact["dynamic_params"]["value"]["dynamic"], true);
    let schema = &artifact["dynamic_params"]["value"]["schema"];
    assert_eq!(schema["root"]["kind"], "list");
    assert_eq!(schema["root"]["item"]["kind"], "named");
    assert_eq!(schema["root"]["item"]["name"], "main.Record");
    assert_eq!(
        schema["root"]["item"]["body"]["fields"][0]["schema"]["name"],
        "main.Nested"
    );
    assert_eq!(
        artifact["dynamic_params"]["value"]["dynamic_item_descriptor"]["name"],
        "main.Record"
    );
    assert_eq!(
        artifact["lookalike_factory"],
        "nefor.factory.structured-output"
    );
    assert_eq!(artifact["lookalike_params"]["value"]["dynamic"], false);
    assert_eq!(
        artifact["lookalike_params"]["value"]["schema"]["root"]["name"],
        "main.DynamicList"
    );
    assert_eq!(artifact["ordinary_params"]["value"]["dynamic"], false);
    assert_eq!(
        artifact["ordinary_params"]["value"]["schema"]["root"]["name"],
        "main.Record"
    );
    assert_eq!(artifact["text_factory"], "nefor.factory.llm");
}

#[test]
fn traverse_converts_ordinary_node_compositions_into_closed_templates() {
    let artifact = run(
        "traverse-ordinary-compositions",
        r#"
import nefor.actors.{}
import nefor.human.{}
import nefor.contracts.{}
import nefor.dynamic.{}
import nefor.graph.{}
import nefor.mag.{}
import nefor.node.{}

type Input {value: String}
type Output {value: String}
let resolve: fn(String) -> nefor.actors.AuthoredModel = |model| => named(nefor.actors.AuthoredModel, ResolvedModel, nefor.actors.ResolvedModel {provider: "test", model: model, reasoning_effort: nefor.actors.no_reasoning_effort})
let config = nefor.actors.AgentConfig<String> {model: "mock", system: "test", tools: [], tool_approval_policy: named(nefor.contracts.ToolApprovalPolicy, Default, nil), max_corrections: 1}
let identity = nefor.dynamic.traverse("identity-traverse", nefor.graph.identity<Input>("identity"))
let composition = nefor.dynamic.traverse("composition-traverse", nefor.node.compose("composition", nefor.graph.identity<Input>("composition-left"), nefor.graph.identity<Input>("composition-right")))
let fanout = nefor.dynamic.traverse("fanout-traverse", nefor.node.fanout("fanout", nefor.graph.identity<Input>("fanout-left"), nefor.graph.identity<Input>("fanout-right")))
let sequence = nefor.dynamic.traverse("sequence-traverse", nefor.node.sequence([nefor.graph.identity<Input>("sequence-left"), nefor.graph.identity<Input>("sequence-right")]))
let shared = nefor.graph.source("reuse-shared", Input {value: "shared"})
let reuse_left = nefor.node.compose("reuse-left", shared, nefor.graph.identity<Input>("reuse-left-value"))
let reuse_right = nefor.node.compose("reuse-right", shared, nefor.graph.identity<Input>("reuse-right-value"))
let reuse = nefor.dynamic.traverse("reuse-traverse", nefor.node.fanout("reuse-worker", reuse_left, reuse_right))
let agent = nefor.dynamic.traverse("agent-traverse", nefor.actors.agent<String, Input, Output>("agent", resolve, config))
let only_operation<I, O>: fn(nefor.graph.Node<nefor.dynamic.DynamicList<I>, nefor.dynamic.DynamicList<O>>) -> nefor.mag.ProgramOperation = |node| => first(get(node, "operations"))
artifact {
  identity: only_operation(identity),
  composition: only_operation(composition),
  fanout: only_operation(fanout),
  sequence: only_operation(sequence),
  reuse: only_operation(reuse),
  agent: only_operation(agent)
}
"#,
        json!({}),
    );

    for name in [
        "identity",
        "composition",
        "fanout",
        "sequence",
        "reuse",
        "agent",
    ] {
        let operation = &artifact[name];
        assert_eq!(operation["id"], format!("{name}-traverse.expand"));
        assert_eq!(operation["on"]["wire"], "nefor.dynamic.Indexed");
        assert!(!operation["template"]["messages"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(operation["template"].get("junctions").is_none());
        assert!(operation["expressions"].as_array().unwrap().len() >= 4);
    }

    let identity = &artifact["identity"]["template"];
    assert_eq!(identity["actors"].as_array().unwrap().len(), 1);
    assert_eq!(identity["actors"][0]["slot"], "traverse.result");
    assert_eq!(identity["messages"][0]["transforms"], json!([]));

    let composition = &artifact["composition"]["template"];
    assert_eq!(composition["actors"].as_array().unwrap().len(), 1);
    assert_eq!(composition["messages"][0]["transforms"], json!([]));

    let fanout = &artifact["fanout"]["template"];
    assert_eq!(fanout["actors"].as_array().unwrap().len(), 1);
    assert_eq!(fanout["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        fanout["messages"][0]["transforms"][0]["constructor"],
        "Assemble"
    );
    assert_eq!(fanout["messages"][0]["transforms"][0]["value"]["slot"], 0);
    assert_eq!(fanout["messages"][1]["transforms"][0]["value"]["slot"], 1);
    assert_eq!(fanout["actor_reference_relocations"], json!([]));

    let sequence = &artifact["sequence"]["template"];
    assert_eq!(sequence["actors"].as_array().unwrap().len(), 1);
    assert_eq!(sequence["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        sequence["messages"][0]["transforms"][0]["value"]["kind"],
        "list"
    );
    assert_eq!(sequence["messages"][0]["transforms"][0]["value"]["slot"], 0);
    assert_eq!(sequence["messages"][1]["transforms"][0]["value"]["slot"], 1);

    let reuse = &artifact["reuse"]["template"];
    let reuse_slots: Vec<&str> = reuse["actors"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|actor| actor["slot"].as_str())
        .collect();
    assert_eq!(
        reuse_slots
            .iter()
            .filter(|slot| **slot == "reuse-shared")
            .count(),
        1
    );
    let unique_reuse_slots = reuse_slots
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(reuse_slots.len(), unique_reuse_slots.len());

    let agent = &artifact["agent"]["template"];
    assert_eq!(agent["actors"].as_array().unwrap().len(), 5);
    assert_eq!(
        agent["actor_reference_relocations"],
        json!([{
            "actor": {"slot": "agent.run-tool"},
            "path": ["conversation_peer"],
            "shape": "actor_id"
        }])
    );
    let run_tool = agent["actors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|actor| actor["slot"] == "agent.run-tool")
        .unwrap();
    assert_eq!(
        run_tool["params"]["value"]["conversation_peer"],
        "agent.llm"
    );
}

#[test]
fn traverse_rejects_streaming_boundaries_inside_compound_types() {
    for ty in [
        "nefor.dynamic.DynamicList<String>",
        "List<nefor.dynamic.DynamicList<String>>",
        "Set<nefor.dynamic.DynamicList<String>>",
        "Map<String, nefor.dynamic.DynamicList<String>>",
        "(String, nefor.dynamic.DynamicList<String>)",
        "Wrapped",
        "Choice",
        "Distinct",
    ] {
        let error = run_error(
            "traverse-nested-streaming",
            &format!(
                r#"
import nefor.dynamic.{{}}
import nefor.graph.{{}}
type Wrapped {{values: nefor.dynamic.DynamicList<String>}}
type Choice = Value(Wrapped) | Empty(Unit)
newtype Distinct = Wrapped
artifact(nefor.dynamic.traverse("bad", nefor.graph.identity<{ty}>("worker")))
"#
            ),
        );
        assert!(
            error.contains("streaming worker input is unsupported"),
            "{ty}: {error}"
        );
    }
}

#[test]
fn traverse_rejects_workers_outside_the_closed_ordinary_subset() {
    let cases = [
        (
            "retry-gate",
            r#"
import nefor.actors.{}
import nefor.human.{}
import nefor.dynamic.{}
artifact(nefor.dynamic.traverse("bad", nefor.actors.retry_gate<String>("worker", nefor.actors.RetryGateConfig {max_retries: 2})))
"#,
            "retry gates retain cross-activation state",
        ),
        (
            "approval-gate",
            r#"
import nefor.actors.{}
import nefor.human.{}
import nefor.dynamic.{}
artifact(nefor.dynamic.traverse("bad", nefor.human.approval_gate("worker", nefor.human.ApprovalConfig {prompt: "Approve?"})))
"#,
            "human approval requires interactive lifecycle",
        ),
        (
            "dynamic-producer",
            r#"
import nefor.actors.{}
import nefor.human.{}
import nefor.contracts.{}
import nefor.dynamic.{}
let resolve: fn(String) -> nefor.actors.AuthoredModel = |model| => nefor.actors.AuthoredModel.ModelProfile(nefor.actors.model_profile(model))
let worker = nefor.actors.agent<String, String, nefor.dynamic.DynamicList<String>>("worker", resolve, nefor.actors.AgentConfig<String> {model: "test", system: "Return items", tools: [], tool_approval_policy: named(nefor.contracts.ToolApprovalPolicy, Default, nil), max_corrections: 0})
artifact(nefor.dynamic.traverse("bad", worker))
"#,
            "dynamic producers are streaming workers",
        ),
        (
            "low-level-actor",
            r#"
import nefor.dynamic.{}
import nefor.graph.{}
let input = nefor.graph.port("raw", type_tag<String>(), "in")
let output = nefor.graph.port("raw", type_tag<String>(), "out")
let raw = nefor.graph.node("raw", "ordinary", [nefor.graph.actor("raw", "test.raw", [type_evidence(type_tag<String>())], (), nefor.graph.store_port(input), [nefor.graph.store_port(output)])], [], [], input, output)
artifact(nefor.dynamic.traverse("bad", raw))
"#,
            "arbitrary actor constructor has no closed single-result contract",
        ),
        (
            "nested-operation",
            r#"
import core.map.{}
import nefor.dynamic.{}
import nefor.graph.{}
import nefor.mag.{}
let worker = nefor.graph.source("worker", "value")
let operation = nefor.graph.instantiate_delta_template("nested", get(worker, "output"), (core.map.empty<String, nefor.mag.TypedCapture>(): Map<String, nefor.mag.TypedCapture>), [], nefor.mag.DeltaTemplate {types: (core.map.empty<String, TypeDescriptor>(): Map<String, TypeDescriptor>), actors: [], routes: [], messages: [], nodes: [], actor_reference_relocations: []})
artifact(nefor.dynamic.traverse("bad", nefor.graph.with_operation(worker, operation)))
"#,
            "nested worker operations are unsupported",
        ),
        (
            "external-reference",
            r#"
import nefor.actors.{}
import nefor.human.{}
import nefor.contracts.{}
import nefor.dynamic.{}
import nefor.graph.{}
let input = nefor.graph.port("worker", type_tag<nefor.contracts.ToolCalls>(), "generic-tool.ToolCalls")
let output = nefor.graph.port("worker", type_tag<nefor.contracts.ToolHandle>(), "generic-tool.ToolHandle")
let actor = nefor.graph.closed_actor("worker", "nefor.factory.run-tool", [], nefor.actors.RunToolParams {model: "", provider: "", model_profile: nefor.actors.no_model_profile, conversation_peer: "external", tools: [], tool_approval_policy: nefor.contracts.normalize_tool_approval_policy(named(nefor.contracts.ToolApprovalPolicy, Default, nil))}, nefor.graph.store_port(input), [nefor.graph.store_port(output)], [nefor.graph.Relocation {path: ["conversation_peer"], shape: "actor_id"}])
let worker = nefor.graph.node("worker", "ordinary", [actor], [], [], input, output)
artifact(nefor.dynamic.traverse("bad", worker))
"#,
            "references an actor outside the closed worker",
        ),
        (
            "divergent-actor-id",
            r#"
import nefor.contracts.{}
import nefor.dynamic.{}
import nefor.node.{}
import nefor.shell.{}
let first = nefor.shell.script("duplicate", nefor.shell.ShellScriptParams {script: "printf first", cwd: ".", timeout: named(nefor.contracts.Timeout, Unlimited, nil)})
let second = nefor.shell.script("duplicate", nefor.shell.ShellScriptParams {script: "printf second", cwd: ".", timeout: named(nefor.contracts.Timeout, Unlimited, nil)})
let worker = nefor.node.fanout("worker", first, second)
artifact(nefor.dynamic.traverse("bad", worker))
"#,
            "identity conflict",
        ),
        (
            "malformed-relocations",
            r#"
import nefor.actors.{}
import nefor.human.{}
import nefor.contracts.{}
import nefor.dynamic.{}
import nefor.graph.{}
let input = nefor.graph.port("worker", type_tag<nefor.contracts.ToolCalls>(), "generic-tool.ToolCalls")
let output = nefor.graph.port("worker", type_tag<nefor.contracts.ToolHandle>(), "generic-tool.ToolHandle")
let relocation = nefor.graph.Relocation {path: ["conversation_peer"], shape: "actor_id"}
let actor = nefor.graph.closed_actor("worker", "nefor.factory.run-tool", [], nefor.actors.RunToolParams {model: "", provider: "", model_profile: nefor.actors.no_model_profile, conversation_peer: "worker", tools: [], tool_approval_policy: nefor.contracts.normalize_tool_approval_policy(named(nefor.contracts.ToolApprovalPolicy, Default, nil))}, nefor.graph.store_port(input), [nefor.graph.store_port(output)], [relocation, relocation])
let worker = nefor.graph.node("worker", "ordinary", [actor], [], [], input, output)
artifact(nefor.dynamic.traverse("bad", worker))
"#,
            "missing, duplicate, overlapping or undeclared relocation paths",
        ),
    ];

    for (name, source, expected) in cases {
        let error = run_error(&format!("traverse-rejects-{name}"), source);
        assert!(error.contains(expected), "{name}: {error}");
    }
}

#[test]
fn authored_timeouts_normalize_at_both_node_boundaries() {
    for (index, (variant, payload, expected)) in [
        (
            "Unlimited",
            "nil",
            json!({"present": false, "milliseconds": 0}),
        ),
        (
            "Milliseconds",
            "1",
            json!({"present": true, "milliseconds": 1}),
        ),
        (
            "Seconds",
            "2",
            json!({"present": true, "milliseconds": 2000}),
        ),
        (
            "Minutes",
            "3",
            json!({"present": true, "milliseconds": 180000}),
        ),
        (
            "Milliseconds",
            "9223372036854775807",
            json!({"present": true, "milliseconds": i64::MAX}),
        ),
        (
            "Seconds",
            "9223372036854775",
            json!({"present": true, "milliseconds": 9223372036854775000_i64}),
        ),
        (
            "Minutes",
            "153722867280912",
            json!({"present": true, "milliseconds": 9223372036854720000_i64}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let result = run(
            &format!("timeout-{index}"),
            &format!(
                r#"
import nefor
let duration = named(Timeout, {variant}, {payload})
let process = exec("process", ProcessExecParams {{argv: ["true"], cwd: ".", timeout: duration}})
let shell = script("shell", ShellScriptParams {{script: "true", cwd: ".", timeout: duration}})
artifact([get(first(get(process, "actors")), "params"), get(first(get(shell, "actors")), "params")])
"#
            ),
            json!({}),
        );
        for params in result.as_array().unwrap() {
            assert_eq!(params["value"]["timeout"], expected, "{variant}({payload})");
        }
    }
}

#[test]
fn authored_timeouts_reject_nonpositive_overflowing_and_wrong_payloads() {
    for (variant, payload, diagnostic) in [
        ("Milliseconds", "0", "strictly positive"),
        ("Milliseconds", "-1", "strictly positive"),
        ("Seconds", "0", "strictly positive"),
        ("Seconds", "-1", "strictly positive"),
        ("Minutes", "0", "strictly positive"),
        ("Minutes", "-1", "strictly positive"),
        ("Seconds", "9223372036854776", "int_mul overflow"),
        ("Minutes", "153722867280913", "int_mul overflow"),
        ("Milliseconds", "1.5", "Int"),
        ("Seconds", "\"2\"", "Int"),
        ("Unlimited", "1", "Unit"),
    ] {
        for (module, function, params) in [
            (
                "process",
                "exec",
                "ProcessExecParams {argv: [\"true\"], cwd: \".\", timeout: duration}",
            ),
            (
                "shell",
                "script",
                "ShellScriptParams {script: \"true\", cwd: \".\", timeout: duration}",
            ),
        ] {
            let error = run_error(
                &format!("invalid-timeout-{module}-{variant}-{}", payload.len()),
                &format!(
                    r#"
import nefor
let duration = named(Timeout, {variant}, {payload})
artifact({function}("operation", {params}))
"#
                ),
            );
            assert!(error.contains(diagnostic), "{variant}({payload}): {error}");
        }
    }
}

#[test]
fn file_input_timeout_values_cannot_bypass_node_normalization() {
    let root = workspace("timeout-file-input");
    let mag_lib = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mag/lib");
    fs::write(root.join("main.mag"), r#"
import nefor.contracts.{}
import nefor.shell.{}
let duration = named(nefor.contracts.Timeout, Seconds, (get(read_json("duration.json"), "seconds"): Int))
artifact(nefor.shell.script("script", nefor.shell.ShellScriptParams {script: "true", cwd: ".", timeout: duration}))
"#).unwrap();
    for (seconds, expected_error) in [
        (1_i64, None),
        (0, Some("strictly positive")),
        (-1, Some("strictly positive")),
        (9223372036854776, Some("int_mul overflow")),
    ] {
        fs::write(
            root.join("duration.json"),
            json!({"seconds": seconds}).to_string(),
        )
        .unwrap();
        let result = compile_file_with_inputs_and_module_roots_and_options_and_syntax(
            &root,
            "main.mag",
            json!({}),
            &[root.clone(), mag_lib.clone()],
            CompilerOptions::default(),
            SyntaxMode::New,
        );
        if let Some(expected) = expected_error {
            let error = result.unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        } else {
            let node = result.unwrap();
            assert_eq!(
                node["actors"][0]["params"]["value"]["timeout"],
                json!({"present": true, "milliseconds": 1000})
            );
        }
    }
}

#[test]
fn authored_tool_policies_lower_without_changing_agent_configuration() {
    let result = run("policy-alternatives", r#"
import nefor
import core.map.{}
let resolver: fn(String) -> AuthoredModel = |model| => named(AuthoredModel, ResolvedModel, ResolvedModel {provider: "test", model: model, reasoning_effort: no_reasoning_effort})
let policies = [named(ToolApprovalPolicy, Default, nil), named(ToolApprovalPolicy, Rules, ToolApprovalRules {rules: core.map.insert(core.map.empty<String, String>(), "cargo build", "deny")})]
let config: fn(ToolApprovalPolicy) -> AgentConfig<String> = |policy| => AgentConfig<String> {model: "test", system: "", tools: [], tool_approval_policy: policy, max_corrections: 0}
let configs = map(config, policies)
let nodes = map(((|settings| => agent<String, String, TextAnswer>("worker", resolver, settings)): fn(AgentConfig<String>) -> Node<String, core.types.Result<AgentError, TextAnswer>>), configs)
artifact {configs: configs, actors: map(((|node| => get(node, "actors")): fn(Node<String, core.types.Result<AgentError, TextAnswer>>) -> List<nefor.graph.Actor>), nodes)}
"#.replace("import core.map.{}", "import core.map.{}\nimport core.types.{}\nimport nefor.graph.{}").as_str(), json!({}));
    assert_eq!(
        result["configs"][0]["tool_approval_policy"],
        json!({"constructor": "Default", "value": null})
    );
    assert_eq!(
        result["configs"][1]["tool_approval_policy"],
        json!({"constructor": "Rules", "value": {"rules": {"cargo build": "deny"}}})
    );
    for (index, rules) in [json!({}), json!({"cargo build": "deny"})]
        .into_iter()
        .enumerate()
    {
        let run_tool = result["actors"][index]
            .as_array()
            .unwrap()
            .iter()
            .find(|actor| actor["factory"] == "nefor.factory.run-tool")
            .unwrap();
        assert_eq!(
            run_tool["params"]["value"]["tool_approval_policy"],
            json!({"rules": rules})
        );
    }
}

#[test]
fn human_workflow_has_purpose_owned_nominal_decisions() {
    let result = run(
        "human-workflow-decision",
        r#"
import nefor
let gate = approval_gate("review", ApprovalConfig {prompt: "Accept this workflow result?"})
artifact {gate: gate, approved: named(HumanWorkflowDecision, Approved, HumanWorkflowApproval {content: "accepted"}), rejected: named(HumanWorkflowDecision, Rejected, HumanWorkflowRejection {reason: "revise"})}
"#,
        json!({}),
    );
    assert_eq!(
        result["gate"]["output"]["type"]["name"],
        "nefor.human.HumanWorkflowDecision"
    );
    assert_eq!(
        result["approved"],
        json!({"constructor": "Approved", "value": {"content": "accepted"}})
    );
    assert_eq!(
        result["rejected"],
        json!({"constructor": "Rejected", "value": {"reason": "revise"}})
    );
}

#[test]
fn nefor_facade_exports_exact_authoring_allowlist() {
    let source = fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mag/lib/nefor.mag"),
    )
    .unwrap();
    let actual: std::collections::BTreeSet<_> = source
        .lines()
        .filter_map(|line| {
            let declaration = line
                .strip_prefix("let ")
                .or_else(|| line.strip_prefix("type "))?;
            let name = if let Some(operator) = declaration.strip_prefix('`') {
                operator.split('`').next().unwrap()
            } else {
                declaration.split([' ', '<']).next().unwrap()
            };
            Some(name.to_owned())
        })
        .collect();
    let expected: std::collections::BTreeSet<_> = "Node source identity compile_graph AgentConfig agent AuthoredModel ResolvedModel ModelProfile model_profile OptionalReasoningEffort reasoning_effort no_reasoning_effort read_only_tools general_tools ToolApprovalRules ToolApprovalPolicy ProviderInput TextAnswer OutputViolation ProviderError OutputValidationError AgentErrorReason AgentError ApprovalConfig approval_gate HumanWorkflowApproval HumanWorkflowRejection HumanWorkflowDecision RetryGateConfig retry_gate RetryDecision node_named rename compose >>> discard then *> keep_left before <* fanout &&& parallel *** choose +++ sequence lift and_then >=> result_map_named result_map map_error_named map_error DynamicList Indexed traverse context Timeout ProcessExecParams exec ShellScriptParams script ProcessExited ProcessSignaled ProcessTermination ProcessResult cwd CreateSpec OpenSpec Worktree WorktreeError create open join".split_whitespace().map(str::to_owned).collect();
    assert_eq!(actual, expected);
    for name in [
        "Task",
        "Text",
        "ToolResult",
        "Approved",
        "ApprovalDecision",
        "Graph",
        "RunToolParams",
        "RuntimeTimeout",
        "ToolApprovalRuntimeRules",
        "OptionalIdentifier",
        "ProcessFailure",
        "timeout_ms",
        "no_timeout",
        "tool_approval_policy",
        "no_tool_approval_policy",
        "pack_adt",
        "capture_ref",
        "manifest",
        "compile",
        "adapter_factory",
    ] {
        let error = run_error(
            &format!("facade-hidden-{name}"),
            &format!("import nefor.{{{name}}}\nartifact(nil)"),
        );
        assert!(error.contains(name), "{name}: {error}");
    }
}

#[test]
fn nefor_facade_imports_specialize_and_construct() {
    let lib = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mag/lib");
    let facade = fs::read_to_string(lib.join("nefor.mag")).unwrap();
    let mut source = String::from("import nefor.graph.{}\nimport nefor.contracts.{}\n");
    // Exercise every exported alias as an imported value or concrete type.
    // Calls below additionally check generic forwarding and constructor access.
    for (index, line) in facade.lines().enumerate() {
        if let Some(declaration) = line.strip_prefix("type ") {
            let name = declaration.split(" = ").next().unwrap();
            source.push_str(&format!(
                "import nefor.{{{}}}\n",
                name.split('<').next().unwrap()
            ));
            let concrete = if let Some((name, parameters)) = name.split_once('<') {
                format!(
                    "{name}<{}>",
                    vec!["Int"; parameters.split(',').count()].join(", ")
                )
            } else {
                name.to_owned()
            };
            source.push_str(&format!(
                "let witness_{index}: fn({concrete}) -> {concrete} = |value| => value\n"
            ));
        } else if let Some(declaration) = line.strip_prefix("let ") {
            let (name, _) = declaration.split_once(" = ").unwrap();
            source.push_str(&format!("import nefor.{{{name}}}\n"));
            source.push_str(&format!("let export_{index} = {name}\n"));
        }
    }
    source.push_str(r#"
let timeout = named(Timeout, Seconds, 1)
let policy = named(ToolApprovalPolicy, Default, nil)
let decision = named(HumanWorkflowDecision, Approved, HumanWorkflowApproval {content: "yes"})
let retry = named(RetryDecision<Int>, Continue, 1)
let process = exec("process", ProcessExecParams {argv: ["true"], cwd: ".", timeout: timeout})
let shell = script("shell", ShellScriptParams {script: "true", cwd: ".", timeout: named(Timeout, Unlimited, nil)})
let value = source("value", 1)
let next = identity<Int>("next")
let composed = value >>> next
let lifted = lift<Unit, String, Int>("lifted", composed)
let mapped = result_map(lifted, identity<Int>("mapped"))
let direct = nefor.graph.identity<Int>("direct")
let runtime_policy = nefor.contracts.normalize_tool_approval_policy(policy)
artifact(get(runtime_policy, "rules"))
"#);
    assert_eq!(run("facade-import-smoke", &source, json!({})), json!({}));
}

#[test]
fn fixed_combinators_are_anonymous_and_preserve_equal_typed_destination_slots() {
    let artifact = run(
        "anonymous-fixed-combinators",
        r#"
import nefor.graph.{}
import nefor.node.{}

let shared = nefor.graph.source("shared", 7)
let duplicated = nefor.node.`&&&`(
  nefor.graph.identity<Int>("left"),
  nefor.graph.identity<Int>("right"),
)
let sink_input = nefor.graph.port("sink", type_tag<(Int, Int)>(), "test.Input")
let sink_output = nefor.graph.port("sink", type_tag<(Int, Int)>(), "test.Output")
let sink_actor = nefor.graph.actor("sink", "test.sink", [type_evidence(type_tag<(Int, Int)>())], (), nefor.graph.store_port(sink_input), [nefor.graph.store_port(sink_output)])
let sink = nefor.graph.node("sink", "ordinary", [sink_actor], [], [], sink_input, sink_output)
let duplicated_source: nefor.graph.Node<Unit, (Int, Int)> = nefor.node.compose("duplicate-source", shared, duplicated)
let composed = nefor.node.compose("with-sink", duplicated_source, sink)
artifact(composed)
"#,
        json!({}),
    );

    assert!(artifact.get("junctions").is_none());
    assert_eq!(artifact["actors"].as_array().unwrap().len(), 2);
    assert_eq!(artifact["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(artifact["nodes"][0]["path"], json!(["shared"]));
    assert_eq!(artifact["nodes"][1]["path"], json!(["sink"]));

    let routes = artifact["routes"].as_array().unwrap();
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0]["from"], routes[1]["from"]);
    assert_eq!(routes[0]["to"], routes[1]["to"]);
    assert_ne!(routes[0]["id"], routes[1]["id"]);
    assert_eq!(routes[0]["transforms"][0]["constructor"], "Assemble");
    assert_eq!(routes[1]["transforms"][0]["constructor"], "Assemble");
    assert_eq!(routes[0]["transforms"][0]["value"]["slot"], 0);
    assert_eq!(routes[1]["transforms"][0]["value"]["slot"], 1);
    assert_eq!(routes[0]["transforms"][0]["value"]["path"], json!([0]));
    assert_eq!(routes[1]["transforms"][0]["value"]["path"], json!([0]));
}

#[test]
fn every_fixed_operator_and_nonempty_sequence_add_no_runtime_or_logical_entity() {
    let artifact = run(
        "all-anonymous-fixed-combinators",
        r#"
import core.types.{}
import nefor.graph.{}
import nefor.node.{}

let composed = nefor.node.`>>>`(nefor.graph.source("compose-source", 7), nefor.graph.identity<Int>("compose-right"))
let parallel = nefor.node.`>>>`(
  nefor.graph.source("parallel-source", (7, "x")),
  nefor.node.`***`(nefor.graph.identity<Int>("parallel-left"), nefor.graph.identity<String>("parallel-right")),
)
let then = nefor.node.`*>`(nefor.graph.source("then-source", 7), nefor.graph.identity<Unit>("then-right"))
let before = nefor.node.`<*`(nefor.graph.source("before-source", 7), nefor.graph.identity<Unit>("before-right"))
let chosen = nefor.node.`>>>`(
  nefor.graph.source("choice-source", named(core.types.Either<Int, String>, Left, 7)),
  nefor.node.`+++`(nefor.graph.identity<Int>("choice-left"), nefor.graph.identity<String>("choice-right")),
)
let sequenced = nefor.node.`>>>`(
  nefor.graph.source("sequence-source", 7),
  nefor.node.sequence([nefor.graph.identity<Int>("sequence-first"), nefor.graph.identity<Int>("sequence-second")]),
)
artifact {compose: composed, parallel: parallel, then: then, before: before, choice: chosen, sequence: sequenced}
"#,
        json!({}),
    );

    for (name, source) in [
        ("compose", "compose-source"),
        ("parallel", "parallel-source"),
        ("then", "then-source"),
        ("before", "before-source"),
        ("choice", "choice-source"),
        ("sequence", "sequence-source"),
    ] {
        let node = &artifact[name];
        assert!(node.get("junctions").is_none(), "{name}: {node:#?}");
        assert_eq!(node["actors"].as_array().unwrap().len(), 1, "{name}");
        assert_eq!(node["actors"][0]["id"], source, "{name}");
        assert_eq!(node["nodes"].as_array().unwrap().len(), 1, "{name}");
        assert_eq!(node["nodes"][0]["path"], json!([source]), "{name}");
        assert_eq!(node["nodes"][0]["members"], json!([source]), "{name}");
        assert!(
            serde_json::to_string(node)
                .unwrap()
                .find("nefor.node.composite")
                .is_none(),
            "{name}"
        );
    }

    assert_eq!(
        artifact["compose"]["output"]["boundary"]["type"]["name"],
        "Int"
    );
    assert_eq!(
        artifact["parallel"]["output"]["boundary"]["type"]["kind"],
        "product"
    );
    assert_eq!(
        artifact["then"]["output"]["boundary"]["type"]["name"],
        "Unit"
    );
    assert_eq!(
        artifact["before"]["output"]["boundary"]["type"]["name"],
        "Int"
    );
    assert_eq!(
        artifact["choice"]["output"]["boundary"]["type"]["kind"],
        "adt"
    );
    assert_eq!(
        artifact["sequence"]["output"]["boundary"]["type"]["kind"],
        "list"
    );
}

#[test]
fn compile_graph_emits_a_direct_transformed_result_without_synthetic_entities() {
    let artifact = run(
        "direct-transformed-result",
        r#"
import nefor.artifact.{}
import nefor.graph.{}
import nefor.node.{}

let shared = nefor.graph.source("shared", 7)
let terminal = nefor.node.named("terminal", nefor.node.compose(
  "anonymous-container",
  shared,
  nefor.node.`&&&`(nefor.graph.identity<Int>("left"), nefor.graph.identity<Int>("right")),
))
nefor.artifact.compile_graph(terminal)
"#,
        contracts(json!([])),
    );

    let initial = &artifact["program"]["initial"];
    assert!(initial.get("junctions").is_none());
    assert_eq!(initial["actors"].as_array().unwrap().len(), 1);
    assert_eq!(initial["actors"][0]["id"], "shared");
    assert_eq!(initial["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(initial["nodes"][0]["path"], json!(["terminal"]));
    assert_eq!(initial["nodes"][0]["members"], json!([]));
    assert_eq!(initial["nodes"][1]["path"], json!(["terminal", "shared"]));
    assert!(serde_json::to_string(initial)
        .unwrap()
        .find("nefor.node.composite")
        .is_none());

    let result = &initial["result"]["from"];
    assert_eq!(result["leaves"].as_array().unwrap().len(), 2);
    assert_eq!(result["leaves"][0]["port"], result["leaves"][1]["port"]);
    assert_eq!(result["leaves"][0]["steps"][0]["constructor"], "Assemble");
    assert_eq!(result["leaves"][0]["steps"][0]["value"]["slot"], 0);
    assert_eq!(result["leaves"][1]["steps"][0]["value"]["slot"], 1);
    assert_eq!(result["through"], json!([]));
}

#[test]
fn traverse_templates_embed_anonymous_transform_metadata_without_junctions() {
    let artifact = run(
        "transformed-traverse-template",
        r#"
import nefor.dynamic.{}
import nefor.graph.{}
import nefor.node.{}

type Input {value: String}
let fanout = nefor.dynamic.traverse("fanout", nefor.node.fanout("worker", nefor.graph.identity<Input>("left"), nefor.graph.identity<Input>("right")))
let sequence = nefor.dynamic.traverse("sequence", nefor.node.sequence([nefor.graph.identity<Input>("first"), nefor.graph.identity<Input>("second")]))
artifact {
  fanout: get(first(get(fanout, "operations")), "template"),
  sequence: get(first(get(sequence, "operations")), "template"),
}
"#,
        json!({}),
    );

    for name in ["fanout", "sequence"] {
        let template = &artifact[name];
        assert!(template.get("junctions").is_none());
        assert_eq!(template["actors"].as_array().unwrap().len(), 1);
        assert_eq!(template["actors"][0]["slot"], "traverse.result");
        assert_eq!(template["messages"].as_array().unwrap().len(), 2);
        assert_eq!(
            template["messages"][0]["transforms"][0]["constructor"],
            "Assemble"
        );
        assert_eq!(template["messages"][0]["transforms"][0]["value"]["slot"], 0);
        assert_eq!(template["messages"][1]["transforms"][0]["value"]["slot"], 1);
        assert!(template["routes"].as_array().unwrap().iter().all(|route| {
            matches!(
                route["from"]["endpoint"]["constructor"].as_str(),
                Some("LocalActorRef")
            ) && matches!(
                route["to"]["endpoint"]["constructor"].as_str(),
                Some("LocalActorRef") | Some("ExistingActorRef")
            )
        }));
    }
}
