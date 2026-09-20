mod topology_lua;

pub mod bridge {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/bridge.rs"));
}

mod error {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/error.rs"));
}

pub mod kernel {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/kernel.rs"));

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Write;

        fn write_kernel(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
            let path = dir.join("kernel.lua");
            let mut f = std::fs::File::create(&path).expect("create kernel");
            f.write_all(body.as_bytes()).expect("write kernel");
            path
        }

        fn shipped_host() -> LuaHost {
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            LuaHost::load_kernel(
                &manifest.join("lua/mag-kernel/init.lua"),
                Some(&manifest.join("../../lua")),
            )
            .expect("load shipped kernel")
        }

        fn compile_mag_source(host: &LuaHost, name: &str, source: &str) -> JsonValue {
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let source_dir = std::env::temp_dir()
                .join(format!("mag-kernel-shell-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&source_dir);
            std::fs::create_dir_all(&source_dir).expect("create shell test workspace");
            std::fs::write(source_dir.join("main.mag"), source).expect("write shell test program");
            let contracts = host.registry_contracts().expect("runtime contracts");
            let artifact =
                nefor_mag::compile_file_with_inputs_and_module_roots_and_options_and_syntax(
                    &source_dir,
                    "main.mag",
                    serde_json::json!({"factory_contracts": contracts}),
                    &[
                        manifest.join("../../mag/lib"),
                        manifest.join("../../examples/nefor-agent/mag/lib"),
                    ],
                    nefor_mag::CompilerOptions::default(),
                    nefor_mag::SyntaxMode::New,
                )
                .expect("compile MAG test program");
            let modification =
                crate::artifact_modification(&artifact).expect("normalize shell artifact");
            let _ = std::fs::remove_dir_all(source_dir);
            modification
        }

        fn compile_mag_source_error(host: &LuaHost, source: &str) -> String {
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            nefor_mag::compile_with_inputs_and_module_roots_and_options_and_syntax(
                source,
                &manifest,
                serde_json::json!({"factory_contracts": host.registry_contracts().expect("runtime contracts")}),
                &[
                    manifest.join("../../mag/lib"),
                    manifest.join("../../examples/nefor-agent/mag/lib"),
                ],
                nefor_mag::CompilerOptions::default(),
                nefor_mag::SyntaxMode::New,
            )
            .expect_err("MAG source must fail compilation")
            .to_string()
        }

        fn actor_params<'a>(artifact: &'a JsonValue, actor: &str) -> &'a JsonValue {
            artifact["actors"]
                .as_array()
                .expect("initial actors")
                .iter()
                .find(|candidate| candidate["id"] == actor)
                .unwrap_or_else(|| panic!("actor {actor}"))
                .get("params")
                .expect("actor params")
        }

        fn compile_mag_eval_expression(host: &LuaHost, name: &str, expression: &str) -> JsonValue {
            let source = format!(
                r#"import nefor.artifact.{{}}
    import nefor.contracts.{{}}
    import nefor.graph.{{}}
    import nefor.process.{{}}
    import nefor.result.{{}}
    import nefor.shell.{{}}
    let start = nefor.graph.source("start", nil)
    let operation = {expression}
    let result = nefor.graph.output_for("result", operation)
    nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [
      nefor.graph.edge(start, operation),
      nefor.graph.edge(operation, result),
    ])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
    "#
            );
            compile_mag_source(host, name, &source)
        }

        #[test]
        fn bounded_unicode_projection_crosses_the_semantic_validation_host_boundary() {
            let host = shipped_host();
            let valid: bool = host
                .lua
                .load(
                    r#"
                    local model_context = require("model-context")
                    local malformed_ok, malformed_error = pcall(
                      nefor.semantic_type.validate_value,
                      { kind = "primitive", name = "String" },
                      string.char(0xc3))
                    assert(not malformed_ok)
                    assert(tostring(malformed_error):find("invalid type: byte array", 1, true))

                    local projected = model_context._utf8_head("éx", 2)
                    assert(projected == "é")
                    local ok, validation = pcall(
                      nefor.semantic_type.validate_value,
                      { kind = "primitive", name = "String" },
                      projected)
                    return ok and validation.ok == true
                    "#,
                )
                .eval()
                .expect("evaluate projected semantic value");
            assert!(
                valid,
                "bounded Unicode must remain JSON-valid across mlua serde"
            );
        }

        #[test]
        fn nefor_mag_in_five_minutes_satisfies_v4_runtime_contracts() {
            let host = shipped_host();
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let repository = manifest.join("../..");
            let markdown = std::fs::read_to_string(
                repository.join("mag/book/02. nefor/00. Nefor MAG in Five Minutes.md"),
            )
            .expect("read Nefor guide");
            let source = markdown
                .split("```mag\n")
                .nth(2)
                .and_then(|rest| rest.split_once("\n```").map(|(source, _)| source))
                .expect("Nefor guide contains one complete MAG program");
            let workspace = repository
                .join("tmp")
                .join(format!("mag-book-host-contracts-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&workspace);
            std::fs::create_dir_all(&workspace).expect("create guide host workspace");
            std::fs::write(workspace.join("main.mag"), source).expect("write guide program");

            let artifact = nefor_mag::compile_file_with_inputs_and_module_roots_and_options_and_syntax(
                &workspace,
                "main.mag",
                serde_json::json!({"factory_contracts": host.registry_contracts().expect("runtime contracts")}),
                &[
                    repository.join("mag/lib"),
                    repository.join("examples/nefor-agent/mag/lib"),
                ],
                nefor_mag::CompilerOptions::default(),
                nefor_mag::SyntaxMode::New,
            )
            .expect("compile Nefor guide with runtime contracts");
            let decoded = crate::artifact_program(&artifact).expect("decode guide program");
            let actors = decoded.initial["actors"].as_array().expect("guide actors");
            for factory in [
                "nefor.factory.shell-script",
                "nefor.factory.dynamic-each",
                "nefor.factory.dynamic-output",
                "nefor.factory.dynamic-all",
            ] {
                assert!(
                    actors.iter().any(|actor| actor["factory"] == factory),
                    "guide must exercise {factory}"
                );
            }
            for retired in [
                "nefor.factory.discard",
                "nefor.factory.output",
                "nefor.factory.product-first",
                "nefor.factory.product-join",
                "nefor.factory.product-split",
                "nefor.factory.collector",
                "nefor.factory.sequence-empty",
                "nefor.factory.adt-pack",
                "nefor.factory.adt-unpack",
            ] {
                assert!(
                    !actors.iter().any(|actor| actor["factory"] == retired),
                    "guide must not restore retired scaffolding actor {retired}"
                );
            }
            assert!(decoded.initial.get("junctions").is_none());
            assert!(
                decoded.initial["routes"].as_array().is_some_and(|routes| {
                    routes.iter().any(|route| {
                        route["transforms"]
                            .as_array()
                            .is_some_and(|steps| !steps.is_empty())
                    })
                }),
                "guide fixed composition lowers to route transformations"
            );
            assert!(
                decoded.operations.iter().any(|operation| {
                    operation["template"]["actors"]
                        .as_array()
                        .is_some_and(|actors| !actors.is_empty())
                }),
                "guide declares a runtime-sized worker operation"
            );
            assert_eq!(
                actor_params(&decoded.initial, "verification.build")["script"],
                "cargo build"
            );
            assert_eq!(
                actor_params(&decoded.initial, "verification.test")["script"],
                "cargo test"
            );

            assert!(
                host.begin_run("mag-book-contracts", "mag-book-contracts", None)
                    .expect("begin guide run")
                    .ok
            );
            let started = host
                .start_program("mag-book-contracts", &decoded.initial, &decoded.operations)
                .expect("start guide modification");
            assert!(
                started.ok,
                "guide host validation failed: {:?}",
                started.error
            );
            host.end_run("mag-book-contracts", TeardownReason::RunComplete)
                .expect("end guide host run");
            std::fs::remove_dir_all(workspace).expect("remove guide host workspace");
        }

        #[test]
        fn zero_actor_workers_execute_as_isolated_v4_traversal_templates() {
            let host = shipped_host();
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let source_dir = std::env::temp_dir().join(format!(
                "mag-kernel-v4-traverse-workers-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&source_dir);
            std::fs::create_dir_all(&source_dir).expect("create traversal test workspace");

            let cases = [
                (
                    "identity-empty",
                    "nefor.graph.identity<String>(\"worker\")",
                    "String",
                    "String",
                    serde_json::json!([]),
                    serde_json::json!([]),
                ),
                (
                    "identity",
                    "nefor.graph.identity<String>(\"worker\")",
                    "String",
                    "String",
                    serde_json::json!(["alpha", "beta"]),
                    serde_json::json!(["alpha", "beta"]),
                ),
                (
                    "fanout",
                    "nefor.node.fanout(\"worker\", nefor.graph.identity<String>(\"left\"), nefor.graph.identity<String>(\"right\"))",
                    "String",
                    "(String, String)",
                    serde_json::json!(["alpha", "beta"]),
                    serde_json::json!([["alpha", "alpha"], ["beta", "beta"]]),
                ),
                (
                    "choose",
                    "nefor.node.choose(\"worker\", nefor.graph.identity<String>(\"left\"), nefor.graph.identity<Int>(\"right\"))",
                    "core.types.Either<String, Int>",
                    "core.types.Either<String, Int>",
                    serde_json::json!([{"constructor":"Left","value":"alpha"},{"constructor":"Right","value":9}]),
                    serde_json::json!([{"constructor":"Left","value":"alpha"},{"constructor":"Right","value":9}]),
                ),
                (
                    "result-bind",
                    "nefor.result.and_then(\"worker\", nefor.graph.identity<core.types.Result<String, Int>>(\"input\"), nefor.result.lift<Int, String, Int>(\"lift\", nefor.graph.identity<Int>(\"value\")))",
                    "core.types.Result<String, Int>",
                    "core.types.Result<String, Int>",
                    serde_json::json!([{"constructor":"Error","value":"failed"},{"constructor":"Ok","value":9}]),
                    serde_json::json!([{"constructor":"Error","value":"failed"},{"constructor":"Ok","value":9}]),
                ),
                (
                    "sequence",
                    "nefor.node.sequence([nefor.graph.identity<String>(\"first\"), nefor.graph.identity<String>(\"second\")])",
                    "String",
                    "List<String>",
                    serde_json::json!(["alpha", "beta"]),
                    serde_json::json!([["alpha", "alpha"], ["beta", "beta"]]),
                ),
            ];

            for (name, worker, input_type, output_type, provider_values, expected) in cases {
                let source = format!(
                    r#"
import core.types.{{}}
import nefor.actors.{{}}
import nefor.artifact.{{}}
import nefor.contracts.{{}}
import nefor.dynamic.{{}}
import nefor.graph.{{}}
import nefor.node.{{}}
import nefor.result.{{}}
type InvestigationInput {{prompt: String}}
let exact_model: fn(nefor.actors.ResolvedModel) -> nefor.actors.AuthoredModel = |model| => named(nefor.actors.AuthoredModel, ResolvedModel, model)
let configured_model = nefor.actors.ResolvedModel {{provider: "test-provider", model: "test-model", reasoning_effort: nefor.actors.no_reasoning_effort}}
let start = nefor.graph.source("task", InvestigationInput {{prompt: "produce values"}})
let planner = nefor.actors.agent<nefor.actors.ResolvedModel, InvestigationInput, nefor.dynamic.DynamicList<{input_type}>>("planner", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {{model: configured_model, system: "Return values.", tools: [], tool_approval_policy: named(nefor.contracts.ToolApprovalPolicy, Default, nil), max_corrections: 0}})
let planned = nefor.node.`>>>`(start, planner)
let traversal = nefor.dynamic.traverse("traversal", {worker})
let lifted = nefor.result.lift<nefor.dynamic.DynamicList<{input_type}>, nefor.contracts.AgentError, nefor.dynamic.DynamicList<{output_type}>>("lifted", traversal)
let completed = nefor.result.`>=>`(planned, lifted)
let contextual = nefor.result.map(completed, nefor.dynamic.context<{output_type}>("completed-context"))
nefor.artifact.compile_graph(contextual)
"#
                );
                std::fs::write(source_dir.join("main.mag"), source)
                    .expect("write traversal test program");
                let artifact = nefor_mag::compile_file_with_inputs_and_module_roots_and_options_and_syntax(
                    &source_dir,
                    "main.mag",
                    serde_json::json!({"factory_contracts": host.registry_contracts().expect("runtime contracts")}),
                    &[
                        manifest.join("../../mag/lib"),
                        manifest.join("../../examples/nefor-agent/mag/lib"),
                    ],
                    nefor_mag::CompilerOptions::default(),
                    nefor_mag::SyntaxMode::New,
                )
                .unwrap_or_else(|error| panic!("compile {name} traversal worker: {error}"));
                let decoded = crate::artifact_program(&artifact)
                    .unwrap_or_else(|error| panic!("decode {name} traversal program: {error}"));
                let operation = decoded
                    .operations
                    .iter()
                    .find(|operation| operation["id"] == "traversal.expand")
                    .unwrap_or_else(|| panic!("{name} traversal operation"));
                let template_actors = operation["template"]["actors"]
                    .as_array()
                    .expect("traversal template actors");
                assert_eq!(
                    template_actors.len(),
                    1,
                    "{name}: only the dynamic index actor remains"
                );
                assert_eq!(template_actors[0]["factory"], "nefor.factory.dynamic-index");
                assert!(operation["template"].get("junctions").is_none());
                assert!(operation["template"]["routes"].as_array().is_some());
                assert!(
                    operation["template"]["messages"]
                        .as_array()
                        .is_some_and(|messages| {
                            messages
                                .iter()
                                .all(|message| message["transforms"].is_array())
                        }),
                    "{name}: occurrence messages retain anonymous transforms"
                );

                assert!(host.begin_run(name, name, None).unwrap().ok);
                host.drain_emits().unwrap();
                let started = host
                    .start_program(name, &decoded.initial, &decoded.operations)
                    .unwrap();
                assert!(started.ok, "{name} start: {:?}", started.error);
                let emitted = host.drain_emits().unwrap();
                let request = tool_invoke(&emitted, "test-provider");
                host.bus_response(
                    request["id"].as_str().unwrap(),
                    Some(&serde_json::json!({"text": serde_json::json!({"value":provider_values}).to_string()})),
                    None,
                    Some("async"),
                ).unwrap();
                let failure = host.take_run_failed(name).unwrap();
                let completion = host.take_run_complete(name).unwrap();
                assert!(completion.is_some(), "{name} did not complete: {failure:?}");
                let result = completion.unwrap().result.unwrap();
                assert_eq!(
                    result["value"]["value"]["content"]["value"], expected,
                    "{name}: {result}"
                );
                host.end_run(name, TeardownReason::RunComplete).unwrap();
                host.drain_emits().unwrap();
            }

            std::fs::remove_dir_all(source_dir).expect("remove traversal test workspace");
        }

        #[test]
        fn fixed_operator_transformations_execute_without_synthetic_entities() {
            let host = shipped_host();
            let cases = [
                (
                    "operator-compose",
                    r#"let operation = nefor.node.`>>>`(nefor.graph.source("source", 7), nefor.graph.identity<Int>("right"))"#,
                    serde_json::json!(7),
                ),
                (
                    "operator-parallel",
                    r#"let operation = nefor.node.`>>>`(nefor.graph.source("source", (7, "x")), nefor.node.`***`(nefor.graph.identity<Int>("left"), nefor.graph.identity<String>("right")))"#,
                    serde_json::json!([7, "x"]),
                ),
                (
                    "operator-then",
                    r#"let operation = nefor.node.`*>`(nefor.graph.source("source", 7), nefor.graph.identity<Unit>("right"))"#,
                    serde_json::Value::Null,
                ),
                (
                    "operator-before",
                    r#"let operation = nefor.node.`<*`(nefor.graph.source("source", 7), nefor.graph.identity<Unit>("right"))"#,
                    serde_json::json!(7),
                ),
                (
                    "operator-choice",
                    r#"let operation = nefor.node.`>>>`(nefor.graph.source("source", named(core.types.Either<Int, String>, Left, 7)), nefor.node.`+++`(nefor.graph.identity<Int>("left"), nefor.graph.identity<String>("right")))"#,
                    serde_json::json!({"constructor":"Left","value":7}),
                ),
                (
                    "operator-sequence",
                    r#"let operation = nefor.node.`>>>`(nefor.graph.source("source", 7), nefor.node.sequence([nefor.graph.identity<Int>("first"), nefor.graph.identity<Int>("second")]))"#,
                    serde_json::json!([7, 7]),
                ),
            ];

            for (name, definition, expected) in cases {
                let source = format!(
                    "import core.types.{{}}\nimport nefor.artifact.{{}}\nimport nefor.graph.{{}}\nimport nefor.node.{{}}\n{definition}\nnefor.artifact.compile_graph(operation)"
                );
                let modification = compile_mag_source(&host, name, &source);
                assert!(modification.get("junctions").is_none(), "{name}");
                assert_eq!(
                    modification["actors"].as_array().unwrap().len(),
                    1,
                    "{name}"
                );
                assert_eq!(modification["actors"][0]["id"], "source", "{name}");
                assert_eq!(modification["nodes"].as_array().unwrap().len(), 1, "{name}");
                assert_eq!(
                    modification["nodes"][0]["path"],
                    serde_json::json!(["source"]),
                    "{name}"
                );

                assert!(host.begin_run(name, name, None).expect("begin run").ok);
                host.drain_emits().expect("drain begin events");
                let started = host
                    .start_program(name, &modification, &[])
                    .unwrap_or_else(|error| panic!("start {name}: {error:?}"));
                assert!(started.ok, "{name} start: {:?}", started.error);
                let failure = host.take_run_failed(name).expect("read failure");
                let completion = host.take_run_complete(name).expect("read completion");
                assert!(completion.is_some(), "{name} did not complete: {failure:?}");
                assert_eq!(
                    completion.unwrap().result.unwrap()["value"],
                    expected,
                    "{name}"
                );
                host.end_run(name, TeardownReason::RunComplete)
                    .expect("end run");
                host.drain_emits().expect("drain end events");
            }
        }

        #[test]
        fn shared_source_union_executes_once_and_fans_out_to_distinct_consumers() {
            let host = shipped_host();
            let source = r#"
    import nefor.artifact.{}
    import nefor.graph.{}
    import nefor.node.{}
    let shared = nefor.graph.source("shared", 7)
    let left = nefor.node.compose("left", shared, nefor.graph.identity<Int>("left-value"))
    let right = nefor.node.compose("right", shared, nefor.graph.identity<Int>("right-value"))
    nefor.artifact.compile_graph(nefor.node.fanout("branched", left, right))
    "#;
            let modification = compile_mag_source(&host, "shared-source-union", source);
            assert_eq!(
                modification["actors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|actor| actor["id"] == "shared")
                    .count(),
                1
            );
            assert!(
                host.begin_run("shared-source-union", "shared-source-union", None)
                    .expect("begin shared source run")
                    .ok
            );
            host.drain_emits().expect("drain begin events");
            let started = host
                .start_program("shared-source-union", &modification, &[])
                .expect("start shared source program");
            assert!(started.ok, "shared source start: {:?}", started.error);
            let completion = host
                .take_run_complete("shared-source-union")
                .expect("read shared source completion")
                .expect("shared source completed");
            assert_eq!(
                completion.result.unwrap()["value"],
                serde_json::json!([7, 7])
            );
            host.end_run("shared-source-union", TeardownReason::RunComplete)
                .expect("end shared source run");
        }

        #[test]
        fn run_model_snapshot_overrides_llm_factories_without_mutating_specs() {
            let host = shipped_host();
            let direct = r#"
    import core.types.{}
    import nefor.actors.{}
    import nefor.artifact.{}
    import nefor.contracts.{}
    import nefor.graph.{}
    type InvestigationInput {prompt: String}
    let exact_model: fn(nefor.actors.ResolvedModel) -> nefor.actors.AuthoredModel = |model| => named(nefor.actors.AuthoredModel, ResolvedModel, model)
    let resolved = nefor.actors.ResolvedModel {provider: "authored-provider", model: "authored-model", reasoning_effort: nefor.actors.reasoning_effort("authored-effort")}
    let start = nefor.graph.source("task", InvestigationInput {prompt: "answer"})
    let worker = nefor.actors.agent<nefor.actors.ResolvedModel, InvestigationInput, nefor.contracts.TextAnswer>("worker", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: resolved, system: "", tools: [], tool_approval_policy: named(nefor.contracts.ToolApprovalPolicy, Default, nil), max_corrections: 2})
    let result = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("result")
    nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, worker), nefor.graph.edge(worker, result)])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
    "#;
            let structured = direct
                .replace(
                    "import nefor.actors.{}",
                    "import nefor.actors.{}\n    type Answer {answer: String}",
                )
                .replace(
                    "nefor.actors.agent<nefor.actors.ResolvedModel, InvestigationInput, nefor.contracts.TextAnswer>",
                    "nefor.actors.agent<nefor.actors.ResolvedModel, InvestigationInput, Answer>",
                )
                .replace(
                    "nefor.actors.agent<Model, InvestigationInput, nefor.contracts.TextAnswer>",
                    "nefor.actors.agent<Model, InvestigationInput, Answer>",
                )
                .replace(
                    "nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>",
                    "nefor.graph.output<core.types.Result<nefor.contracts.AgentError, Answer>>",
                );
            for (run_id, source, factory) in [
                ("snapshot-direct", direct.to_owned(), "nefor.factory.llm"),
                (
                    "snapshot-structured",
                    structured,
                    "nefor.factory.structured-output",
                ),
            ] {
                let modification = compile_mag_source(&host, run_id, &source);
                let mut snapshot = ExecutionModelSnapshot {
                    provider: "snapshot-provider".to_owned(),
                    model: "snapshot-model".to_owned(),
                    reasoning_effort: None,
                    provider_options: Some(serde_json::Map::from_iter([(
                        "service_tier".to_owned(),
                        serde_json::Value::String("fast".to_owned()),
                    )])),
                    profiles: Default::default(),
                };
                let begun = host
                    .begin_run_with_principal(
                        run_id,
                        run_id,
                        Some("session"),
                        Some("subagent"),
                        Some("conversation"),
                        Some(&snapshot),
                    )
                    .expect("begin snapshotted run");
                assert!(begun.ok, "begin failed: {:?}", begun.error);
                snapshot
                    .provider_options
                    .as_mut()
                    .unwrap()
                    .insert("service_tier".to_owned(), serde_json::json!("mutated"));
                host.drain_emits().expect("drain begin");
                let outcome = host.start(run_id, &modification).expect("start run");
                assert!(outcome.ok, "start failed: {:?}", outcome.error);
                let emits = host.drain_emits().expect("drain start");
                let spawned = emits
                    .iter()
                    .find(|event| {
                        event["kind"] == "mag.actor_spawned" && event["id"] == "worker.llm"
                    })
                    .expect("llm spawn event");
                assert_eq!(spawned["factory"], factory);
                assert_eq!(spawned["spec"]["params"]["provider"], "authored-provider");
                assert_eq!(spawned["spec"]["params"]["model"], "authored-model");
                assert_eq!(
                    spawned["spec"]["params"]["reasoning_effort"]["present"], true,
                    "inventory retains authored optional effort"
                );
                let invoke = tool_invoke(&emits, "snapshot-provider");
                assert_eq!(invoke["args"]["model"], "snapshot-model");
                assert_eq!(invoke["args"]["provider_options"]["service_tier"], "fast");
                assert!(
                    invoke["args"].get("reasoning_effort").is_none(),
                    "snapshot omission clears authored effort: {invoke:?}"
                );

                if run_id == "snapshot-direct" {
                    let later_snapshot = ExecutionModelSnapshot {
                        provider: "later-provider".to_owned(),
                        model: "later-model".to_owned(),
                        reasoning_effort: Some("low".to_owned()),
                        provider_options: None,
                        profiles: Default::default(),
                    };
                    let begun = host
                        .begin_run_with_principal(
                            "snapshot-later",
                            "snapshot-later",
                            Some("session"),
                            Some("subagent"),
                            Some("conversation-later"),
                            Some(&later_snapshot),
                        )
                        .expect("begin later run");
                    assert!(begun.ok);
                    host.drain_emits().expect("drain later begin");
                    let outcome = host
                        .start("snapshot-later", &modification)
                        .expect("start later run");
                    assert!(outcome.ok, "later start failed: {:?}", outcome.error);
                    let later_emits = host.drain_emits().expect("drain later invoke");
                    let later_invoke = tool_invoke(&later_emits, "later-provider");
                    assert_eq!(later_invoke["args"]["model"], "later-model");

                    let patch_source = direct
                        .replace("\"task\"", "\"patch-task\"")
                        .replace("\"worker\"", "\"patch\"")
                        .replace("\"result\"", "\"patch-result\"");
                    let mut patch = compile_mag_source(&host, "snapshot-patch", &patch_source);
                    patch
                        .as_object_mut()
                        .expect("patch object")
                        .remove("result");
                    let outcome = host.apply(run_id, &patch).expect("apply snapshotted patch");
                    assert!(outcome.ok, "patch failed: {:?}", outcome.error);
                    let patch_emits = host.drain_emits().expect("drain patch invoke");
                    let patch_invoke = tool_invoke(&patch_emits, "snapshot-provider");
                    assert_eq!(patch_invoke["args"]["model"], "snapshot-model");
                    assert!(patch_invoke["args"].get("reasoning_effort").is_none());
                    assert_eq!(
                        patch_invoke["args"]["provider_options"]["service_tier"],
                        "fast"
                    );
                    let patch_spawn = patch_emits
                        .iter()
                        .find(|event| {
                            event["kind"] == "mag.actor_spawned" && event["id"] == "patch.llm"
                        })
                        .expect("patch llm spawn");
                    assert_eq!(
                        patch_spawn["spec"]["params"]["provider"],
                        "authored-provider"
                    );

                    host.end_run("snapshot-later", TeardownReason::Killed)
                        .expect("end later run");
                    host.drain_emits().expect("drain later end");
                }

                host.end_run(run_id, TeardownReason::Killed)
                    .expect("end snapshot run");
                host.drain_emits().expect("drain end");
            }

            let modification = compile_mag_source(&host, "snapshot-effort", direct);
            let snapshot = ExecutionModelSnapshot {
                provider: "snapshot-provider".to_owned(),
                model: "snapshot-model".to_owned(),
                reasoning_effort: Some("high".to_owned()),
                provider_options: None,
                profiles: Default::default(),
            };
            let begun = host
                .begin_run_with_principal(
                    "snapshot-effort",
                    "snapshot-effort",
                    Some("session"),
                    Some("subagent"),
                    Some("conversation"),
                    Some(&snapshot),
                )
                .expect("begin effort run");
            assert!(begun.ok);
            host.drain_emits().expect("drain begin");
            let outcome = host
                .start("snapshot-effort", &modification)
                .expect("start effort run");
            assert!(outcome.ok, "start failed: {:?}", outcome.error);
            let emits = host.drain_emits().expect("drain effort invoke");
            let invoke = tool_invoke(&emits, "snapshot-provider");
            assert_eq!(invoke["args"]["reasoning_effort"], "high");
            host.end_run("snapshot-effort", TeardownReason::Killed)
                .expect("end effort run");
            host.drain_emits().expect("drain effort end");

            let begun = host
                .begin_run("snapshot-fallback", "snapshot-fallback", Some("session"))
                .expect("begin fallback run");
            assert!(begun.ok);
            host.drain_emits().expect("drain fallback begin");
            let outcome = host
                .start("snapshot-fallback", &modification)
                .expect("start fallback run");
            assert!(outcome.ok, "fallback start failed: {:?}", outcome.error);
            let emits = host.drain_emits().expect("drain fallback invoke");
            let invoke = tool_invoke(&emits, "authored-provider");
            assert_eq!(invoke["args"]["model"], "authored-model");
            assert_eq!(invoke["args"]["reasoning_effort"], "authored-effort");
            host.end_run("snapshot-fallback", TeardownReason::Killed)
                .expect("end fallback run");
            host.drain_emits().expect("drain fallback end");

            let mut malformed = modification;
            let llm = malformed["actors"]
                .as_array_mut()
                .expect("actors")
                .iter_mut()
                .find(|actor| actor["id"] == "worker.llm")
                .expect("worker llm");
            llm["params"]["reasoning_effort"] = serde_json::json!({"present": true, "value": ""});
            let begun = host
                .begin_run("malformed-effort", "malformed-effort", Some("session"))
                .expect("begin malformed run");
            assert!(begun.ok);
            host.drain_emits().expect("drain malformed begin");
            let outcome = host
                .start("malformed-effort", &malformed)
                .expect("start malformed run");
            assert!(outcome.ok, "initial modification still applies");
            let failure = host
                .take_run_failed("malformed-effort")
                .expect("take malformed failure")
                .expect("malformed effort fails construction");
            assert!(failure.contains("present=true requires a non-empty value"));
            assert!(host
                .drain_emits()
                .expect("drain malformed failure")
                .iter()
                .all(|event| event["kind"] != "tool.invoke"));
        }

        #[test]
        fn authored_model_profiles_resolve_from_the_owned_run_snapshot() {
            let host = shipped_host();
            let direct = r#"
    import core.types.{}
    import nefor.actors.{}
    import nefor.artifact.{}
    import nefor.contracts.{}
    import nefor.graph.{}
    type InvestigationInput {prompt: String}
    type Model = Current(Unit) | Fast(Unit)
    let fast = Model.Fast(nil)
    let resolve_model: fn(Model) -> nefor.actors.AuthoredModel = |model| => match model {
      case Current(value) => named(nefor.actors.AuthoredModel, ResolvedModel, nefor.actors.ResolvedModel {provider: "authored-provider", model: "authored-model", reasoning_effort: nefor.actors.no_reasoning_effort}),
      case Fast(value) => named(nefor.actors.AuthoredModel, ModelProfile, nefor.actors.model_profile("fast")),
    }
    let start = nefor.graph.source("task", InvestigationInput {prompt: "answer"})
    let worker = nefor.actors.agent<Model, InvestigationInput, nefor.contracts.TextAnswer>("worker", resolve_model, nefor.actors.AgentConfig<Model> {model: fast, system: "", tools: [], tool_approval_policy: named(nefor.contracts.ToolApprovalPolicy, Default, nil), max_corrections: 2})
    let result = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("result")
    nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, worker), nefor.graph.edge(worker, result)])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
    "#;
            let structured = direct
                .replace(
                    "import nefor.actors.{}",
                    "import nefor.actors.{}\n    type Answer {answer: String}",
                )
                .replace(
                    "nefor.actors.agent<nefor.actors.ResolvedModel, InvestigationInput, nefor.contracts.TextAnswer>",
                    "nefor.actors.agent<nefor.actors.ResolvedModel, InvestigationInput, Answer>",
                )
                .replace(
                    "nefor.actors.agent<Model, InvestigationInput, nefor.contracts.TextAnswer>",
                    "nefor.actors.agent<Model, InvestigationInput, Answer>",
                )
                .replace(
                    "nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>",
                    "nefor.graph.output<core.types.Result<nefor.contracts.AgentError, Answer>>",
                );
            for (run_id, source, factory) in [
                ("profile-direct", direct.to_owned(), "nefor.factory.llm"),
                (
                    "profile-structured",
                    structured,
                    "nefor.factory.structured-output",
                ),
            ] {
                let modification = compile_mag_source(&host, run_id, &source);
                assert_eq!(
                    actor_params(&modification, "worker.llm")["model_profile"],
                    serde_json::json!({"present": true, "value": "fast"}),
                    "the compiled actor carries the typed authored selector"
                );
                let mut profiles = BTreeMap::new();
                profiles.insert(
                    "fast".to_owned(),
                    ExecutionResolvedModel {
                        provider: "fast-provider".to_owned(),
                        model: "fast-model".to_owned(),
                        reasoning_effort: Some("low".to_owned()),
                        provider_options: Some(serde_json::Map::from_iter([(
                            "service_tier".to_owned(),
                            serde_json::Value::String("fast".to_owned()),
                        )])),
                    },
                );
                let snapshot = ExecutionModelSnapshot {
                    provider: "current-provider".to_owned(),
                    model: "current-model".to_owned(),
                    reasoning_effort: Some("high".to_owned()),
                    provider_options: None,
                    profiles,
                };
                let begun = host
                    .begin_run_with_principal(
                        run_id,
                        run_id,
                        Some("session"),
                        Some("subagent"),
                        Some("conversation"),
                        Some(&snapshot),
                    )
                    .expect("begin profiled run");
                assert!(begun.ok, "begin failed: {:?}", begun.error);
                host.drain_emits().expect("drain begin");
                let outcome = host
                    .start(run_id, &modification)
                    .expect("start profiled run");
                assert!(outcome.ok, "start failed: {:?}", outcome.error);
                let emits = host.drain_emits().expect("drain profiled invoke");
                let spawned = emits
                    .iter()
                    .find(|event| {
                        event["kind"] == "mag.actor_spawned" && event["id"] == "worker.llm"
                    })
                    .expect("profiled llm spawn");
                assert_eq!(spawned["factory"], factory);
                let invoke = tool_invoke(&emits, "fast-provider");
                assert_eq!(invoke["args"]["model"], "fast-model");
                assert_eq!(invoke["args"]["reasoning_effort"], "low");
                assert_eq!(invoke["args"]["provider_options"]["service_tier"], "fast");
                assert!(invoke["args"].get("model_profile").is_none());

                if run_id == "profile-direct" {
                    let patch_source = direct
                        .replace("\"task\"", "\"patch-task\"")
                        .replace("\"worker\"", "\"patch\"")
                        .replace("\"result\"", "\"patch-result\"");
                    let mut patch = compile_mag_source(&host, "profile-patch", &patch_source);
                    patch
                        .as_object_mut()
                        .expect("patch object")
                        .remove("result");
                    let outcome = host.apply(run_id, &patch).expect("apply profiled patch");
                    assert!(outcome.ok, "patch failed: {:?}", outcome.error);
                    let patch_emits = host.drain_emits().expect("drain profiled patch");
                    let patch_invoke = tool_invoke(&patch_emits, "fast-provider");
                    assert_eq!(patch_invoke["args"]["model"], "fast-model");
                }

                host.end_run(run_id, TeardownReason::Killed)
                    .expect("end profiled run");
                host.drain_emits().expect("drain profiled end");
            }

            let standard_source =
                direct.replace("model_profile(\"fast\")", "model_profile(\"standard\")");
            let modification =
                compile_mag_source(&host, "profile-clears-options", &standard_source);
            let mut profiles = BTreeMap::new();
            profiles.insert(
                "standard".to_owned(),
                ExecutionResolvedModel {
                    provider: "standard-provider".to_owned(),
                    model: "standard-model".to_owned(),
                    reasoning_effort: Some("medium".to_owned()),
                    provider_options: None,
                },
            );
            let snapshot = ExecutionModelSnapshot {
                provider: "current-provider".to_owned(),
                model: "current-model".to_owned(),
                reasoning_effort: Some("high".to_owned()),
                provider_options: Some(serde_json::Map::from_iter([(
                    "service_tier".to_owned(),
                    serde_json::Value::String("fast".to_owned()),
                )])),
                profiles,
            };
            let begun = host
                .begin_run_with_principal(
                    "profile-clears-options",
                    "profile-clears-options",
                    Some("session"),
                    Some("subagent"),
                    Some("conversation"),
                    Some(&snapshot),
                )
                .expect("begin standard-profile run");
            assert!(begun.ok, "begin failed: {:?}", begun.error);
            host.drain_emits().expect("drain begin");
            let outcome = host
                .start("profile-clears-options", &modification)
                .expect("start standard-profile run");
            assert!(outcome.ok, "start failed: {:?}", outcome.error);
            let emits = host.drain_emits().expect("drain standard-profile invoke");
            let invoke = tool_invoke(&emits, "standard-provider");
            assert_eq!(invoke["args"]["model"], "standard-model");
            assert!(
                invoke["args"].get("provider_options").is_none(),
                "profile omission clears root provider options: {invoke:?}"
            );
            host.end_run("profile-clears-options", TeardownReason::Killed)
                .expect("end standard-profile run");
            host.drain_emits().expect("drain standard-profile end");

            let modification = compile_mag_source(&host, "missing-profile", direct);
            let snapshot = ExecutionModelSnapshot {
                provider: "current-provider".to_owned(),
                model: "current-model".to_owned(),
                reasoning_effort: None,
                provider_options: None,
                profiles: Default::default(),
            };
            let begun = host
                .begin_run_with_principal(
                    "missing-profile",
                    "missing-profile",
                    Some("session"),
                    Some("subagent"),
                    Some("conversation"),
                    Some(&snapshot),
                )
                .expect("begin missing-profile run");
            assert!(begun.ok);
            host.drain_emits().expect("drain missing-profile begin");
            let outcome = host
                .start("missing-profile", &modification)
                .expect("start missing-profile run");
            assert!(outcome.ok, "initial modification still applies");
            let failure = host
                .take_run_failed("missing-profile")
                .expect("take missing-profile failure")
                .expect("missing profile fails construction");
            assert!(failure.contains("model profile \"fast\" is absent"));
            assert!(host
                .drain_emits()
                .expect("drain missing-profile failure")
                .iter()
                .all(|event| event["kind"] != "tool.invoke"));
        }

        #[test]
        fn task_source_preserves_task_type_and_value_at_runtime() {
            let host = shipped_host();
            let source = r#"
    import core.types.{}
    import nefor.actors.{}
    import nefor.artifact.{}
    import nefor.contracts.{}
    import nefor.graph.{}
    type InvestigationInput {prompt: String}
    let start = nefor.graph.source("task-input", InvestigationInput {prompt: "runtime prompt"})
    let result = nefor.graph.output<InvestigationInput>("result")
    nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, result)])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
    "#;
            let modification = compile_mag_source(&host, "task-source-runtime", source);
            let begun = host
                .begin_run("task-source-runtime", "task-source-runtime", None)
                .expect("begin Task source run");
            assert!(begun.ok, "begin failed: {:?}", begun.error);
            host.drain_emits().expect("drain begin event");
            let outcome = host
                .start("task-source-runtime", &modification)
                .expect("start Task source run");
            assert!(outcome.ok, "start failed: {:?}", outcome.error);
            let completion = host
                .take_run_complete("task-source-runtime")
                .expect("take Task source completion")
                .expect("Task source run completed");
            assert_eq!(
                completion
                    .result
                    .as_ref()
                    .and_then(|result| result.get("value")),
                Some(&serde_json::json!({"prompt": "runtime prompt"}))
            );
            assert_eq!(
                completion
                    .result
                    .as_ref()
                    .and_then(|result| result.pointer("/semantic_type/name"))
                    .and_then(JsonValue::as_str),
                Some("main.InvestigationInput")
            );
        }

        #[test]
        fn shared_input_sequence_and_left_sequencing_retain_runtime_values() {
            for (run_id, source, expected) in [
                (
                    "shared-input-sequence",
                    r#"
import nefor.artifact.{}
import nefor.graph.{}
import nefor.node.{}
let start = nefor.graph.source("shared", "shared value")
let left = nefor.graph.identity<String>("left")
let right = nefor.graph.identity<String>("right")
let operation = nefor.node.`>>>`(start, nefor.node.sequence([left, right]))
nefor.artifact.compile_graph(operation)
"#,
                    serde_json::json!(["shared value", "shared value"]),
                ),
                (
                    "retain-left-sequencing",
                    r#"
import nefor.artifact.{}
import nefor.graph.{}
import nefor.node.{}
let left = nefor.graph.source("left", "retained")
let right = nefor.graph.source("right", "discarded")
let operation = nefor.node.`<*`(left, right)
nefor.artifact.compile_graph(operation)
"#,
                    serde_json::json!("retained"),
                ),
            ] {
                let host = shipped_host();
                let modification = compile_mag_source(&host, run_id, source);
                let begun = host.begin_run(run_id, run_id, None).expect("begin run");
                assert!(begun.ok, "begin failed: {:?}", begun.error);
                host.drain_emits().expect("drain begin event");
                let outcome = host.start(run_id, &modification).expect("start run");
                assert!(outcome.ok, "start failed: {:?}", outcome.error);
                let completion = host
                    .take_run_complete(run_id)
                    .expect("read completion")
                    .expect("composition completed");
                assert_eq!(
                    completion.result.as_ref().map(|result| &result["value"]),
                    Some(&expected)
                );
            }
        }

        #[test]
        fn result_mapping_preserves_the_unmapped_runtime_branch() {
            for (run_id, operation, expected) in [
                (
                    "result-map-error-branch",
                    "let start = nefor.graph.source(\"start\", named(core.types.Result<String, Int>, Error, \"failed\"))\nlet mapper = nefor.graph.identity<Int>(\"mapper\")\nlet operation = nefor.result.map(start, mapper)",
                    serde_json::json!({"constructor": "Error", "value": "failed"}),
                ),
                (
                    "result-map-error-ok-branch",
                    "let start = nefor.graph.source(\"start\", named(core.types.Result<String, Int>, Ok, 7))\nlet mapper = nefor.graph.identity<String>(\"mapper\")\nlet operation = nefor.result.map_error(start, mapper)",
                    serde_json::json!({"constructor": "Ok", "value": 7}),
                ),
            ] {
                let host = shipped_host();
                let source = format!(
                    "import core.types.{{}}\nimport nefor.artifact.{{}}\nimport nefor.graph.{{}}\nimport nefor.result.{{}}\n{operation}\nnefor.artifact.compile_graph(operation)"
                );
                let modification = compile_mag_source(&host, run_id, &source);
                let begun = host.begin_run(run_id, run_id, None).expect("begin run");
                assert!(begun.ok, "begin failed: {:?}", begun.error);
                host.drain_emits().expect("drain begin event");
                let outcome = host.start(run_id, &modification).expect("start run");
                assert!(outcome.ok, "start failed: {:?}", outcome.error);
                let completion = host
                    .take_run_complete(run_id)
                    .expect("read completion")
                    .expect("result composition completed");
                assert_eq!(completion.result.as_ref().map(|result| &result["value"]), Some(&expected));
            }
        }

        fn unit_root_program(definitions: &str) -> String {
            format!(
                r#"import nefor.artifact.{{}}
import nefor.graph.{{}}
import nefor.node.{{}}
import nefor.shell.{{}}
import nefor.contracts.{{}}
type MessageContent {{content: String}}
let params = nefor.shell.ShellScriptParams {{script: "printf root-ok", cwd: ".", timeout: named(nefor.contracts.Timeout, Unlimited, nil)}}
{definitions}
let result = nefor.graph.output_for("result", operation)
nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(operation, result)])): fn(nefor.graph.Graph) -> nefor.graph.Graph)"#
            )
        }

        // Execute only the harmless command requested by the shipped shell factory;
        // the capability response then re-enters the real kernel routing path.
        fn execute_unit_root_command(host: &LuaHost, invoke: &Map<String, JsonValue>) {
            let args = &invoke["args"]["args"];
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(args["script"].as_str().unwrap())
                .current_dir(args["cwd"].as_str().unwrap())
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert!(output.status.success());
            host.bus_response(
                invoke["id"].as_str().unwrap(),
                Some(&serde_json::json!({
                    "stdout": String::from_utf8(output.stdout).unwrap(),
                    "stderr": String::from_utf8(output.stderr).unwrap(),
                    "termination": {"kind": "code", "code": output.status.code().unwrap()}
                })),
                None,
                Some("async"),
            )
            .unwrap();
        }

        #[test]
        fn nefor_mag_single_command_guide_executes_without_agent() {
            let host = shipped_host();
            let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
            let markdown = std::fs::read_to_string(
                repository.join("mag/book/02. nefor/00. Nefor MAG in Five Minutes.md"),
            )
            .expect("read Nefor guide");
            let source = markdown
                .split_once("```mag\n")
                .and_then(|(_, rest)| rest.split_once("\n```").map(|(source, _)| source))
                .expect("Nefor guide starts with a complete single-command program");
            let run_id = "guide-single-command";
            let modification = compile_mag_source(&host, run_id, source);
            assert!(host.begin_run(run_id, run_id, None).unwrap().ok);
            host.drain_emits().unwrap();
            let started = host.start(run_id, &modification).unwrap();
            assert!(started.ok, "{:?}", started.error);
            let emits = host.drain_emits().unwrap();
            assert_eq!(
                emits
                    .iter()
                    .filter(|event| event["kind"] == "tool.invoke")
                    .count(),
                1,
                "the documented graph dispatches one shell command"
            );
            assert!(host.take_run_complete(run_id).unwrap().is_none());
            execute_unit_root_command(&host, tool_invoke(&emits, "shell.script"));
            let completion = host
                .take_run_complete(run_id)
                .unwrap()
                .expect("shell output completes the documented graph");
            let result = completion.result.unwrap();
            assert_eq!(result["value"]["stdout"], "hello\n");
            assert_eq!(result["value"]["stderr"], "");
            assert!(
                !host
                    .drain_emits()
                    .unwrap()
                    .iter()
                    .any(|event| event["kind"] == "tool.invoke"),
                "command completion needs no agent or further tool invocation"
            );
        }

        #[test]
        fn unit_accepting_roots_execute_once_and_dependencies_do_not_start_early() {
            for (name, definitions, dependent) in [
                (
                    "unit-runtime-run",
                    "let operation = nefor.shell.script(\"command\", params)",
                    false,
                ),
                (
                    "unit-runtime-script",
                    "let operation = nefor.shell.script(\"command\", params)",
                    false,
                ),
                (
                    "unit-runtime-dependent",
                    r#"
let operation = nefor.node.then("ordered", nefor.shell.script("dependency", params), nefor.shell.script("command", params))"#,
                    true,
                ),
            ] {
                let host = shipped_host();
                let modification = compile_mag_source(&host, name, &unit_root_program(definitions));
                assert!(host.begin_run(name, name, None).unwrap().ok);
                host.drain_emits().unwrap();
                let started = host.start(name, &modification).unwrap();
                assert!(started.ok, "{:?}", started.error);
                let emits = host.drain_emits().unwrap();
                let invocations: Vec<_> = emits
                    .iter()
                    .filter(|event| event["kind"] == "tool.invoke")
                    .collect();
                assert_eq!(invocations.len(), 1, "only the root starts");
                let invoke = tool_invoke(&emits, "shell.script");
                assert_eq!(
                    invoke["from"],
                    if dependent { "dependency" } else { "command" }
                );
                assert!(host.take_run_complete(name).unwrap().is_none());
                execute_unit_root_command(&host, invoke);
                let emits = host.drain_emits().unwrap();
                if dependent {
                    assert!(host.take_run_complete(name).unwrap().is_none());
                    assert_eq!(
                        emits
                            .iter()
                            .filter(|event| event["kind"] == "tool.invoke")
                            .count(),
                        1
                    );
                    let invoke = tool_invoke(&emits, "shell.script");
                    assert_eq!(invoke["from"], "command");
                    execute_unit_root_command(&host, invoke);
                } else {
                    assert!(!emits.iter().any(|event| event["kind"] == "tool.invoke"));
                }
                let completion = host
                    .take_run_complete(name)
                    .unwrap()
                    .expect("completed command");
                assert_eq!(completion.result.unwrap()["value"]["stdout"], "root-ok");
                assert!(
                    !host
                        .drain_emits()
                        .unwrap()
                        .iter()
                        .any(|event| event["kind"] == "tool.invoke"),
                    "no duplicate execution after completion"
                );
            }
        }

        fn start_shell_expression(
            host: &LuaHost,
            run_id: &str,
            expression: &str,
        ) -> Vec<Map<String, JsonValue>> {
            let modification = compile_mag_eval_expression(host, run_id, expression);
            let begun = host
                .begin_run(run_id, run_id, None)
                .expect("begin shell run");
            assert!(begun.ok, "begin failed: {:?}", begun.error);
            host.drain_emits().expect("drain begin event");
            let outcome = host.start(run_id, &modification).expect("start shell run");
            assert!(outcome.ok, "start failed: {:?}", outcome.error);
            host.drain_emits().expect("drain shell start")
        }

        fn tool_invoke<'a>(
            emits: &'a [Map<String, JsonValue>],
            command: &str,
        ) -> &'a Map<String, JsonValue> {
            emits
                .iter()
                .find(|event| {
                    event.get("kind").and_then(JsonValue::as_str) == Some("tool.invoke")
                        && event.get("name").and_then(JsonValue::as_str) == Some(command)
                })
                .unwrap_or_else(|| panic!("missing tool.invoke for {command}: {emits:#?}"))
        }

        #[test]
        fn loads_a_table_returning_kernel() {
            let dir = std::env::temp_dir().join(format!("mag-kernel-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let path = write_kernel(&dir, "nefor.log(\"hi\")\nreturn { name = \"k\" }");
            let host = LuaHost::load_kernel(&path, None).expect("load");
            assert_eq!(host.kernel_name().as_deref(), Some("k"));
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn rejects_non_table_kernel() {
            let dir = std::env::temp_dir().join(format!("mag-kernel-nt-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let path = write_kernel(&dir, "return 42");
            let err = match LuaHost::load_kernel(&path, None) {
                Ok(_) => panic!("expected KernelNotTable error"),
                Err(e) => e,
            };
            assert!(matches!(err, MagError::KernelNotTable { .. }));
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn surfaces_missing_kernel_file() {
            let err = match LuaHost::load_kernel(
                std::path::Path::new("/nonexistent/mag/kernel.lua"),
                None,
            ) {
                Ok(_) => panic!("expected KernelRead error"),
                Err(e) => e,
            };
            assert!(matches!(err, MagError::KernelRead { .. }));
        }

        #[test]
        fn json_and_now_and_emit_bindings_are_installed() {
            let dir = std::env::temp_dir().join(format!("mag-kernel-bind-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            // A kernel that exercises the new native surface and returns a table.
            let path = write_kernel(
                &dir,
                r#"
                assert(type(nefor.json) == "table", "json missing")
                assert(nefor.json.decode(nefor.json.encode({a=1})).a == 1, "json roundtrip")
                assert(type(nefor.now_ms) == "function" and nefor.now_ms() > 0, "now_ms")
                assert(type(nefor.fs.data_root) == "function", "fs.data_root")
                nefor.emit({ kind = "test.event", n = 7 })
                return { name = "bindings" }
                "#,
            );
            let host = LuaHost::load_kernel(&path, None).expect("load");
            let drained = host.drain_emits().expect("drain");
            assert_eq!(drained.len(), 1, "one queued emit");
            assert_eq!(
                drained[0].get("kind").and_then(JsonValue::as_str),
                Some("test.event")
            );
            // Draining again yields an empty queue.
            assert!(host.drain_emits().expect("drain2").is_empty());
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn reads_registry_contract_snapshot_as_json() {
            let dir =
                std::env::temp_dir().join(format!("mag-kernel-contract-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let path = write_kernel(
                &dir,
                r#"
                return {
                  registry_contracts = function()
                    return {{
                      identity = "nefor.factory.example",
                      implementation = "example",
                      params = { count = "int" },
                      type_scheme = {
                        variables = { "T" },
                        inputs = { value = "T" },
                        outputs = { "T" },
                      },
                      signals = {},
                    }}
                  end,
                }
                "#,
            );
            let host = LuaHost::load_kernel(&path, None).expect("load");
            let contracts = host.registry_contracts().expect("contracts");
            assert_eq!(contracts[0]["identity"], "nefor.factory.example");
            assert_eq!(contracts[0]["type_scheme"]["variables"][0], "T");
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn process_capability_invocation_carries_authoritative_run_provenance() {
            let host = shipped_host();
            let run_id = "provenance-run";
            let expression = r#"nefor.process.exec("command", nefor.process.ProcessExecParams {argv: ["printf", "provenance"], cwd: nefor.process.cwd, timeout: named(nefor.contracts.Timeout, Unlimited, nil)})"#;
            let modification = compile_mag_eval_expression(&host, run_id, expression);
            let begun = host
                .begin_run_with_principal(
                    run_id,
                    "scout",
                    Some("session-1"),
                    Some("subagent"),
                    Some("conversation-1"),
                    None,
                )
                .expect("begin provenance run");
            assert!(begun.ok, "begin failed: {:?}", begun.error);
            host.drain_emits().expect("drain begin event");
            let outcome = host.start(run_id, &modification).expect("start run");
            assert!(outcome.ok, "start failed: {:?}", outcome.error);
            let emits = host.drain_emits().expect("drain start");
            let invoke = tool_invoke(&emits, "process.exec");
            assert_eq!(
                invoke["args"]["args"]["argv"],
                serde_json::json!(["printf", "provenance"])
            );
            let provenance = invoke["invocation"].as_object().expect("provenance");
            assert_eq!(provenance["session_id"], "session-1");
            assert_eq!(provenance["run_id"], run_id);
            assert_eq!(provenance["principal"], "subagent");
            assert_eq!(provenance["actor_id"], invoke["from"]);
            assert_eq!(provenance["capability_id"], invoke["id"]);
            assert_eq!(provenance["root_conversation_id"], "conversation-1");
        }

        #[test]
        fn process_and_script_keep_structured_results_and_explicit_timeouts() {
            let host = shipped_host();
            let expression = r#"nefor.shell.script("script", nefor.shell.ShellScriptParams {script: "printf output", cwd: nefor.process.cwd, timeout: named(nefor.contracts.Timeout, Milliseconds, 30000)})"#;
            let emits = start_shell_expression(&host, "shell-script", expression);
            let invoke = tool_invoke(&emits, "shell.script");
            assert_eq!(invoke["args"]["args"]["cwd"], ".");
            assert_eq!(
                invoke["args"]["args"]["timeout"],
                serde_json::json!({"present": true, "milliseconds": 30000})
            );
            let id = invoke["id"].as_str().expect("correlation id");
            assert_eq!(
                host.bus_response(
                    id,
                    Some(&serde_json::json!({
                        "stdout": "output", "stderr": "warning",
                        "termination": {"kind": "code", "code": 9}
                    })),
                    None,
                    Some("async")
                )
                .expect("structured response"),
                Some("shell-script".into())
            );
            let completion = host
                .take_run_complete("shell-script")
                .expect("completion")
                .expect("nonzero process result completes normally");
            let value = completion.result.expect("typed result");
            assert_eq!(value["value"]["stdout"], "output");
            assert_eq!(value["value"]["stderr"], "warning");
            assert_eq!(
                value["value"]["termination"]["constructor"],
                "ProcessExited"
            );
            assert_eq!(value["value"]["termination"]["value"]["code"], 9);

            let expression = r#"nefor.process.exec("signaled", nefor.process.ProcessExecParams {argv: ["sleep", "5"], cwd: nefor.process.cwd, timeout: named(nefor.contracts.Timeout, Unlimited, nil)})"#;
            let emits = start_shell_expression(&host, "process-signaled", expression);
            let invoke = tool_invoke(&emits, "process.exec");
            assert_eq!(
                invoke["args"]["args"]["argv"],
                serde_json::json!(["sleep", "5"])
            );
            assert!(invoke["args"]["args"].get("exited_type").is_none());
            assert!(invoke["args"]["args"].get("signaled_type").is_none());
            let id = invoke["id"].as_str().expect("correlation id");
            assert_eq!(
                host.bus_response(
                    id,
                    Some(&serde_json::json!({
                        "stdout": "partial", "stderr": "terminated",
                        "termination": {"kind": "signal", "signal": 15}
                    })),
                    None,
                    Some("async")
                )
                .expect("structured response"),
                Some("process-signaled".into())
            );
            let completion = host
                .take_run_complete("process-signaled")
                .expect("completion")
                .expect("signal termination completes normally");
            let value = completion.result.expect("typed result");
            let signaled = &value["value"]["termination"];
            assert_eq!(signaled["constructor"], "ProcessSignaled");
            assert_eq!(signaled["value"]["signal"], 15);
        }

        #[test]
        fn malformed_and_nonpositive_process_params_fail_before_invocation() {
            let host = shipped_host();
            for milliseconds in [0, -1] {
                let source = format!(
                    r#"import nefor.artifact.{{}}
import nefor.contracts.{{}}
import nefor.process.{{}}
let operation = nefor.process.exec("invalid", nefor.process.ProcessExecParams {{argv: ["true"], cwd: ".", timeout: named(nefor.contracts.Timeout, Milliseconds, {milliseconds})}})
nefor.artifact.compile_graph(operation)"#
                );
                let error = compile_mag_source_error(&host, &source);
                assert!(
                    error.contains("Timeout must be strictly positive"),
                    "{error}"
                );
            }

            let run_id = "process-empty";
            let expression = r#"nefor.process.exec("invalid", nefor.process.ProcessExecParams {argv: ([]: List<String>), cwd: ".", timeout: named(nefor.contracts.Timeout, Unlimited, nil)})"#;
            let emits = start_shell_expression(&host, run_id, expression);
            assert!(emits.iter().all(|event| {
                event.get("kind").and_then(JsonValue::as_str) != Some("tool.invoke")
            }));
            assert!(host
                .take_run_failed(run_id)
                .expect("invalid run failure")
                .is_some());
        }

        #[test]
        fn ordinary_source_node_output_graph_executes_to_its_typed_result() {
            let host = shipped_host();
            let modification = compile_mag_source(
                &host,
                "ordinary-source-node-output",
                r#"
    import core.map.{}
    import nefor.artifact.{}
    import nefor.contracts.{}
    import nefor.graph.{}
    type MessageContent {content: String}

    let start = nefor.graph.source("start", MessageContent {content: "ordinary"})
    let input = nefor.graph.port("echo", type_tag<MessageContent>(), "stub.In")
    let output = nefor.graph.port("echo", type_tag<MessageContent>(), "stub.Out")
    let actor = nefor.graph.actor("echo", "nefor.factory.stub", [], core.map.empty<String, String>(), nefor.graph.store_port(input), [nefor.graph.store_port(output)])
    let echo = nefor.graph.node("echo", "ordinary", [actor], ([]: List<nefor.graph.StoredRoute>), ([]: List<nefor.graph.Message>), input, output)
    let result = nefor.graph.output_for("result", echo)
    nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, echo), nefor.graph.edge(echo, result)])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
                "#,
            );

            let begun = host
                .begin_run(
                    "ordinary-source-node-output",
                    "ordinary-source-node-output",
                    None,
                )
                .expect("begin ordinary run");
            assert!(begun.ok, "begin failed: {:?}", begun.error);
            host.drain_emits().expect("drain begin event");
            let outcome = host
                .start("ordinary-source-node-output", &modification)
                .expect("start ordinary run");
            assert!(outcome.ok, "start failed: {:?}", outcome.error);
            let completion = host
                .take_run_complete("ordinary-source-node-output")
                .expect("read completion")
                .expect("ordinary graph completes");
            let result = completion.result.expect("typed output result");
            assert_eq!(result["kind"], "out");
            assert_eq!(result["value"]["content"], "ordinary");
            assert!(host
                .take_run_failed("ordinary-source-node-output")
                .expect("read failure")
                .is_none());
        }

        #[test]
        fn factory_output_with_correct_type_id_but_malformed_value_fails_the_run() {
            let host = shipped_host();
            let modification = compile_mag_source(
                &host,
                "malformed-typed-output",
                r#"
    import core.map.{}
    import nefor.artifact.{}
    import nefor.contracts.{}
    import nefor.graph.{}
    type MessageContent {content: String}

    let start = nefor.graph.source("start", MessageContent {content: "valid"})
    let input = nefor.graph.port("broken", type_tag<MessageContent>(), "stub.In")
    let output = nefor.graph.port("broken", type_tag<MessageContent>(), "stub.Out")
    let actor = nefor.graph.actor("broken", "nefor.factory.stub", [], core.map.insert(core.map.empty<String, String>(), "value", "not-a-Text-record"), nefor.graph.store_port(input), [nefor.graph.store_port(output)])
    let broken = nefor.graph.node("broken", "ordinary", [actor], ([]: List<nefor.graph.StoredRoute>), ([]: List<nefor.graph.Message>), input, output)
    let result = nefor.graph.output_for("result", broken)
    nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, broken), nefor.graph.edge(broken, result)])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
                "#,
            );
            let run_id = "malformed-typed-output";
            assert!(host.begin_run(run_id, run_id, None).expect("begin").ok);
            host.drain_emits().expect("drain begin event");
            let outcome = host.start(run_id, &modification).expect("start");
            assert!(outcome.ok, "modification remains structurally valid");
            let failure = host
                .take_run_failed(run_id)
                .expect("read failure")
                .expect("malformed factory value fails");
            assert!(failure.contains("malformed semantic value"), "{failure}");
            assert!(
                failure.contains("expected main.MessageContent"),
                "{failure}"
            );
            assert!(host
                .take_run_complete(run_id)
                .expect("read completion")
                .is_none());
        }

        #[test]
        fn whole_product_reaches_output_through_ordinary_firing() {
            let host = shipped_host();
            let modification = compile_mag_source(
                &host,
                "whole-product-output",
                r#"
    import nefor.artifact.{}
    import nefor.contracts.{}
    import nefor.graph.{}
    type MessageContent {content: String}

    let start = nefor.graph.source("start", (MessageContent {content: "left"}, MessageContent {content: "right"}))
    let result = nefor.graph.output<(MessageContent, MessageContent)>("result")
    nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, result)])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
                "#,
            );

            let begun = host
                .begin_run("whole-product-output", "whole-product-output", None)
                .expect("begin product run");
            assert!(begun.ok, "begin failed: {:?}", begun.error);
            host.drain_emits().expect("drain begin event");
            let outcome = host
                .start("whole-product-output", &modification)
                .expect("start product run");
            assert!(outcome.ok, "start failed: {:?}", outcome.error);
            let completion = host
                .take_run_complete("whole-product-output")
                .expect("read completion")
                .expect("whole product graph completes");
            let result = completion.result.expect("typed product result");
            assert_eq!(result["kind"], "out");
            assert_eq!(result["value"][0]["content"], "left");
            assert_eq!(result["value"][1]["content"], "right");
            assert!(result["semantic_type_id"].as_str().is_some());
            assert_eq!(result["constructor_id"], result["semantic_type_id"]);
        }

        #[test]
        fn adt_arrival_routes_only_to_its_explicit_branch_and_keeps_constructor_id() {
            let host = shipped_host();
            let modification = compile_mag_source(
                &host,
                "direct-sum-routing",
                r#"
    import core.map.{}
    import core.types.{}
    import nefor.artifact.{}
    import nefor.graph.{}
    import nefor.node.{}

    type Left {value: String}
    type Right {value: Int}

    let branch<T>: fn(String, TypeTag<T>) -> nefor.graph.Node<T, T> = |id, `type`| => {
      let input = nefor.graph.port(id, `type`, "stub.In")
      let output = nefor.graph.port(id, `type`, "stub.Out")
      let actor = nefor.graph.actor(id, "nefor.factory.stub", [], core.map.empty<String, String>(), nefor.graph.store_port(input), [nefor.graph.store_port(output)])
      nefor.graph.node(id, "ordinary", [actor], ([]: List<nefor.graph.StoredRoute>), ([]: List<nefor.graph.Message>), input, output)
    }

    let start = nefor.graph.source("start", named(core.types.Either<Left, Right>, Left, Left {value: "chosen"}))
    let left = branch("left", type_tag<Left>())
    let right = branch("right", type_tag<Right>())
    let selected = nefor.node.choose("selected", left, right)
    let result = nefor.graph.output<core.types.Either<Left, Right>>("result")
    nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, selected), nefor.graph.edge(selected, result)])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
                "#,
            );
            let begun = host
                .begin_run("direct-sum-routing", "direct-sum-routing", None)
                .expect("begin direct sum run");
            assert!(begun.ok, "begin failed: {:?}", begun.error);
            host.drain_emits().expect("drain begin event");
            let outcome = host
                .start("direct-sum-routing", &modification)
                .expect("start direct sum run");
            assert!(outcome.ok, "start failed: {:?}", outcome.error);
            let emits = host.drain_emits().expect("drain direct sum events");
            assert!(
                emits.iter().any(|event| {
                    event.get("kind").and_then(JsonValue::as_str) == Some("mag.actor_ready")
                        && event.get("id").and_then(JsonValue::as_str) == Some("left")
                }),
                "{emits:?}"
            );
            assert!(!emits.iter().any(|event| {
                event.get("kind").and_then(JsonValue::as_str) == Some("mag.actor_ready")
                    && event.get("id").and_then(JsonValue::as_str) == Some("right")
            }));
            let completion = host
                .take_run_complete("direct-sum-routing")
                .expect("read completion");
            assert!(
                completion.is_some(),
                "sum graph did not complete: {emits:?}"
            );
            let completion = completion.expect("checked above");
            let result = completion.result.expect("typed ADT result");
            assert_eq!(result["value"]["constructor"], "Left");
            assert_eq!(result["value"]["value"]["value"], "chosen");
            let owner = nefor_mag::json::concrete_type_from_json(&result["semantic_type"]).unwrap();
            assert_eq!(
                result["constructor_id"],
                owner.constructor_id("Left").unwrap().as_str()
            );
            assert!(result["constructor_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("sha256:")));
        }

        #[test]
        fn result_bind_routes_ok_and_reconstructs_error_without_erasure() {
            let host = shipped_host();
            for (run_id, constructor, payload, expected) in [
                (
                    "result-bind-ok",
                    "Ok",
                    r#""accepted""#,
                    serde_json::json!({
                        "constructor":"Ok", "value":"accepted"
                    }),
                ),
                (
                    "result-bind-error",
                    "Error",
                    r#"Failure {message: "rejected"}"#,
                    serde_json::json!({"constructor":"Error","value":{"message":"rejected"}}),
                ),
            ] {
                let source = format!(
                    r#"
import core.types.{{}}
import nefor.artifact.{{}}
import nefor.graph.{{}}
import nefor.node.{{}}
import nefor.result.{{}}
type Failure {{message: String}}
let start = nefor.graph.source("start", named(core.types.Result<Failure, String>, {constructor}, {payload}))
let continuation = nefor.result.lift<String, Failure, String>("continued", nefor.graph.identity<String>("right"))
let bound = nefor.result.`>=>`(start, continuation)
let result = nefor.graph.output_for("result", bound)
nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(bound, result)])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
"#
                );
                let modification = compile_mag_source(&host, run_id, &source);
                assert!(host.begin_run(run_id, run_id, None).expect("begin").ok);
                host.drain_emits().expect("drain begin");
                let outcome = host.start(run_id, &modification).expect("start");
                assert!(outcome.ok, "{run_id}: {:?}", outcome.error);
                host.drain_emits().expect("drain");
                let completion = host
                    .take_run_complete(run_id)
                    .expect("take")
                    .expect("complete");
                assert_eq!(completion.result.expect("result")["value"], expected);
                host.end_run(run_id, TeardownReason::RunComplete)
                    .expect("end");
                host.drain_emits().expect("drain end");
            }
        }

        #[test]
        fn result_map_error_transforms_error_and_preserves_ok() {
            let host = shipped_host();
            for (run_id, constructor, payload, expected) in [
                (
                    "result-map-error",
                    "Error",
                    r#"Failure {message: "rejected"}"#,
                    serde_json::json!({"constructor":"Error","value":"mapped"}),
                ),
                (
                    "result-map-ok",
                    "Ok",
                    r#""accepted""#,
                    serde_json::json!({"constructor":"Ok","value":"accepted"}),
                ),
            ] {
                let source = format!(
                    r#"
import core.map.{{}}
import core.types.{{}}
import nefor.artifact.{{}}
import nefor.graph.{{}}
import nefor.result.{{}}
type Failure {{message: String}}
let start = nefor.graph.source("start", named(core.types.Result<Failure, String>, {constructor}, {payload}))
let mapper_input = nefor.graph.port("map-error", type_tag<Failure>(), "stub.In")
let mapper_output = nefor.graph.port("map-error", type_tag<String>(), "stub.Out")
let mapper_actor = nefor.graph.actor("map-error", "nefor.factory.stub", [], core.map.insert(core.map.empty<String, String>(), "value", "mapped"), nefor.graph.store_port(mapper_input), [nefor.graph.store_port(mapper_output)])
let mapper = nefor.graph.node("map-error", "ordinary", [mapper_actor], ([]: List<nefor.graph.StoredRoute>), ([]: List<nefor.graph.Message>), mapper_input, mapper_output)
let mapped = nefor.result.map_error(start, mapper)
let result = nefor.graph.output_for("result", mapped)
nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(mapped, result)])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
"#
                );
                let modification = compile_mag_source(&host, run_id, &source);
                assert!(host.begin_run(run_id, run_id, None).expect("begin").ok);
                host.drain_emits().expect("drain begin");
                let outcome = host.start(run_id, &modification).expect("start");
                assert!(outcome.ok, "{run_id}: {:?}", outcome.error);
                let emits = host.drain_emits().expect("drain");
                let completion = host
                    .take_run_complete(run_id)
                    .expect("take")
                    .unwrap_or_else(|| panic!("{run_id} did not complete: {emits:?}"));
                assert_eq!(completion.result.expect("result")["value"], expected);
                let mapper_ran = emits
                    .iter()
                    .any(|event| event["kind"] == "mag.actor_ready" && event["id"] == "map-error");
                assert_eq!(mapper_ran, constructor == "Error");
                host.end_run(run_id, TeardownReason::RunComplete)
                    .expect("end");
                host.drain_emits().expect("drain end");
            }
        }

        #[test]
        fn nonpositive_timeout_is_rejected_during_compilation() {
            let host = shipped_host();
            let source = r#"import nefor.artifact.{}
import nefor.contracts.{}
import nefor.process.{}
let operation = nefor.process.exec("broken", nefor.process.ProcessExecParams {argv: ["true"], cwd: nefor.process.cwd, timeout: named(nefor.contracts.Timeout, Milliseconds, 0)})
nefor.artifact.compile_graph(operation)"#;
            let error = compile_mag_source_error(&host, source);
            assert!(
                error.contains("Timeout must be strictly positive"),
                "{error}"
            );
        }
    }
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/runtime.rs"));

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn malformed_agent_error_output_rejects_during_load_before_run_registration() {
        let root =
            std::env::temp_dir().join(format!("mag-malformed-agent-output-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("workspace");
        fs::write(
            root.join("main.mag"),
            r#"
import core.types.{}
import nefor.actors.{}
import nefor.artifact.{}
import nefor.contracts.{}
import nefor.graph.{}
type InvestigationInput {prompt: String}
let exact_model: fn(nefor.actors.ResolvedModel) -> nefor.actors.AuthoredModel = |selected| => named(nefor.actors.AuthoredModel, ResolvedModel, selected)
let configured_model = nefor.actors.ResolvedModel {provider: "mock-provider", model: "mock-model", reasoning_effort: nefor.actors.reasoning_effort("medium")}
let start = nefor.graph.source("task", InvestigationInput {prompt: "test"})
let worker = nefor.actors.agent<nefor.actors.ResolvedModel, InvestigationInput, core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("worker", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: configured_model, system: "Answer.", tools: ([]: List<String>), tool_approval_policy: named(nefor.contracts.ToolApprovalPolicy, Default, nil), max_corrections: 0})
let result = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>> >("result")
nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, worker), nefor.graph.edge(worker, result)])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
"#,
        )
        .expect("program");
        let module_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mag/lib");
        let config_module_root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/nefor-agent/mag/lib");
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let host = LuaHost::load_kernel(
            &manifest.join("lua/mag-kernel/init.lua"),
            Some(&manifest.join("../../lua")),
        )
        .expect("kernel");
        let body = serde_json::json!({
            "id": "load-malformed",
            "source_dir": root,
            "module_roots": [module_root, config_module_root],
            "entry": "main.mag"
        });
        let (out_tx, mut out_rx) = mpsc::channel(CHANNEL_CAP);
        handle_load(
            &out_tx,
            body.as_object().expect("load body"),
            Some("load-malformed"),
            &host,
        )
        .await
        .expect("load rejection is a protocol response");

        let outgoing = out_rx.try_recv().expect("load rejection");
        let Body::Event(body) = outgoing.body else {
            panic!("expected event response")
        };
        assert_eq!(body["kind"], ERROR_KIND);
        let message = body["message"].as_str().expect("actionable error");
        assert!(message.contains("worker.llm"), "{message}");
        assert!(message.contains("nefor.contracts.AgentError"), "{message}");
        assert!(message.contains("last_output"), "{message}");
        assert!(
            message.contains("pass only the success output type"),
            "{message}"
        );
        assert!(
            host.drain_emits().expect("kernel emits").is_empty(),
            "load rejection cannot emit mag.run_started"
        );
        fs::remove_dir_all(root).ok();
    }
}
