pub mod bridge {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/bridge.rs"));

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        fn obj(value: Value) -> Map<String, Value> {
            value.as_object().expect("object").clone()
        }

        fn provider_invoke(
            request_id: &str,
            provider: &str,
            messages: Value,
        ) -> Map<String, Value> {
            obj(json!({
                "kind": "tool.invoke",
                "class": "provider",
                "id": request_id,
                "name": provider,
                "args": {
                    "conversation_id": "conversation-stable",
                    "model": "opus",
                    "system": "be helpful",
                    "tools": [{"name": "read_file"}],
                    "input": {"messages": messages}
                }
            }))
        }

        fn event(
            request_id: &str,
            provider: &str,
            name: &str,
            fields: Value,
        ) -> Map<String, Value> {
            let mut body = fields.as_object().expect("fields").clone();
            body.insert("kind".into(), Value::String(PROVIDER_EVENT.into()));
            body.insert("provider".into(), Value::String(provider.into()));
            body.insert("request_id".into(), Value::String(request_id.into()));
            body.insert("event".into(), Value::String(name.into()));
            body
        }

        #[test]
        fn provider_invoke_emits_one_thin_manager_request() {
            let mut bridge = CapabilityBridge::new("tool-gate");
            let messages = json!([
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "second"},
                {"role": "user", "content": "third"}
            ]);
            let mut invoke = provider_invoke("req-1", "provider-a", messages.clone());
            let invocation = json!({"run_id": "run-1", "actor_id": "worker"});
            invoke.insert("invocation".into(), invocation.clone());
            let out = bridge.translate_emit(invoke);

            assert_eq!(out.len(), 1);
            assert_eq!(out[0]["kind"], PROVIDER_INVOKE_REQUEST);
            assert_eq!(out[0]["provider"], "provider-a");
            assert_eq!(out[0]["request_id"], "req-1");
            assert_eq!(out[0]["conversation_id"], "conversation-stable");
            assert_eq!(out[0]["invocation"], invocation);
            assert!(out[0].get("input").is_none());
            assert!(out[0].get("messages").is_none());
            assert!(out[0].get("system").is_none());
            assert!(out[0].get("tool_specs").is_none());
            assert!(out[0].get("chat_id").is_none());
            let wire = serde_json::to_string(&out).expect("serialize");
            assert!(!wire.contains("first"));
            assert!(!wire.contains(".chat."));
            assert!(!wire.contains("history"));
        }

        #[test]
        fn provider_results_correlate_out_of_order() {
            let mut bridge = CapabilityBridge::new("tool-gate");
            bridge.translate_emit(provider_invoke("req-a", "provider-a", json!([])));
            bridge.translate_emit(provider_invoke("req-b", "provider-a", json!([])));
            let b = event("req-b", "provider-a", "completed", json!({"text": "B"}));
            let a = event("req-a", "provider-a", "completed", json!({"text": "A"}));
            assert_eq!(
                bridge
                    .take_reply(b["kind"].as_str().unwrap(), &b)
                    .unwrap()
                    .request_id,
                "req-b"
            );
            assert_eq!(
                bridge
                    .take_reply(a["kind"].as_str().unwrap(), &a)
                    .unwrap()
                    .request_id,
                "req-a"
            );
        }

        #[test]
        fn cancellation_uses_request_id_and_late_output_is_ignored() {
            let mut bridge = CapabilityBridge::new("tool-gate");
            bridge.translate_emit(provider_invoke("req-1", "provider-a", json!([])));
            let cancel = bridge.translate_emit(obj(json!({"kind": "tool.cancel", "id": "req-1"})));
            assert_eq!(
                cancel,
                vec![obj(json!({
                    "kind": PROVIDER_CANCEL_REQUEST,
                    "provider": "provider-a",
                    "request_id": "req-1"
                }))]
            );
            let late = event("req-1", "provider-a", "completed", json!({"text": "late"}));
            assert!(bridge
                .take_reply(late["kind"].as_str().unwrap(), &late)
                .is_none());
        }

        #[test]
        fn canonical_events_are_self_correlated_and_only_terminal_settles() {
            let mut bridge = CapabilityBridge::new("tool-gate");
            bridge.translate_emit(provider_invoke("req-1", "provider-a", json!([])));
            for name in ["text_delta", "reasoning_delta", "retry", "usage"] {
                let body = event("req-1", "provider-a", name, json!({"text": "chunk"}));
                let kind = body["kind"].as_str().unwrap();
                assert_eq!(
                    bridge.provider_request_id(CONVERSATION_MANAGER, kind, &body),
                    Some("req-1")
                );
                assert!(bridge.take_reply(kind, &body).is_none());
            }
            let tool = event(
                "req-1",
                "provider-a",
                "tool_call",
                json!({
                    "id": "call-1", "name": "read_file", "arguments": {"path": "x"}
                }),
            );
            assert!(bridge
                .take_reply(tool["kind"].as_str().unwrap(), &tool)
                .is_none());
            let done = event("req-1", "provider-a", "completed", json!({"text": ""}));
            let reply = bridge
                .take_reply(done["kind"].as_str().unwrap(), &done)
                .unwrap();
            assert_eq!(reply.result.unwrap()["tool_calls"][0]["name"], "read_file");
        }

        #[test]
        fn provider_native_lifecycle_never_enters_terminal_tool_calls() {
            let mut bridge = CapabilityBridge::new("tool-gate");
            bridge.translate_emit(provider_invoke("req-native", "chatgpt", json!([])));
            for (name, fields) in [
                (
                    "tool_execution_started",
                    json!({
                        "tool_call_id": "web-1", "name": "web_search",
                        "arguments": {"action": "search", "query": "rust"}
                    }),
                ),
                (
                    "tool_execution_completed",
                    json!({
                        "tool_call_id": "web-1", "name": "web_search",
                        "arguments": {"action": "search", "query": "rust"},
                        "result": {"status": "completed"}
                    }),
                ),
            ] {
                let lifecycle = event("req-native", "chatgpt", name, fields);
                let kind = lifecycle["kind"].as_str().unwrap();
                assert_eq!(
                    bridge.provider_request_id(CONVERSATION_MANAGER, kind, &lifecycle),
                    Some("req-native")
                );
                assert!(bridge.take_reply(kind, &lifecycle).is_none());
            }
            let done = event(
                "req-native",
                "chatgpt",
                "completed",
                json!({"text": "answer", "finish_reason": "stop"}),
            );
            let result = bridge
                .take_reply(done["kind"].as_str().unwrap(), &done)
                .unwrap()
                .result
                .unwrap();
            assert_eq!(result["text"], "answer");
            assert!(result.get("tool_calls").is_none());
        }

        #[test]
        fn provider_context_reaches_the_kernel_reply_unchanged() {
            let mut bridge = CapabilityBridge::new("tool-gate");
            bridge.translate_emit(provider_invoke("req-1", "chatgpt", json!([])));
            let context = json!({
                "provider": "chatgpt",
                "format": "chatgpt.responses.output_items.v1",
                "model": "gpt-5.6-sol",
                "artifact": {"items": [{
                    "type": "reasoning", "encrypted_content": "sealed"
                }]}
            });
            let done = event(
                "req-1",
                "chatgpt",
                "completed",
                json!({"text": "", "provider_context": context}),
            );
            let result = bridge
                .take_reply(done["kind"].as_str().unwrap(), &done)
                .expect("terminal reply")
                .result
                .expect("successful result");
            assert_eq!(result["provider_context"], context);
        }

        #[test]
        fn error_event_settles_as_kernel_error_and_unknown_is_ignored() {
            let mut bridge = CapabilityBridge::new("tool-gate");
            bridge.translate_emit(provider_invoke("req-1", "provider-a", json!([])));
            let wrong = event("req-1", "provider-b", "completed", json!({"text": "wrong"}));
            assert!(bridge
                .take_reply(wrong["kind"].as_str().unwrap(), &wrong)
                .is_none());
            let failed = event(
                "req-1",
                "provider-a",
                "error",
                json!({"message": "overloaded"}),
            );
            let reply = bridge
                .take_reply(failed["kind"].as_str().unwrap(), &failed)
                .unwrap();
            assert_eq!(reply.error.as_deref(), Some("overloaded"));
            assert!(reply.result.is_none());
        }

        #[test]
        fn canonical_nested_result_settles_text_and_tool_calls_once() {
            let mut bridge = CapabilityBridge::new("tool-gate");
            bridge.translate_emit(provider_invoke("req-result", "chatgpt", json!([])));
            let terminal = event(
                "req-result",
                "chatgpt",
                "completed",
                json!({"result": {
                    "text": "typed answer",
                    "finish_reason": "tool_calls",
                    "tool_calls": [{
                        "id": "call-1", "name": "read_file", "arguments": {"path": "x"}
                    }]
                }}),
            );
            let result = bridge
                .take_reply(terminal["kind"].as_str().unwrap(), &terminal)
                .expect("nested output settles")
                .result
                .expect("successful result");
            assert_eq!(result["text"], "typed answer");
            assert_eq!(result["finish_reason"], "tool_calls");
            assert_eq!(result["tool_calls"][0]["name"], "read_file");
            assert!(bridge
                .take_reply(terminal["kind"].as_str().unwrap(), &terminal)
                .is_none());
        }

        #[test]
        fn canonical_nested_result_and_top_level_shapes_settle_once() {
            let mut bridge = CapabilityBridge::new("tool-gate");
            bridge.translate_emit(provider_invoke("req-result", "provider-a", json!([])));
            let result = event(
                "req-result",
                "provider-a",
                "completed",
                json!({"result": {"text": "nested result", "finish_reason": "stop"}}),
            );
            assert_eq!(
                bridge
                    .take_reply(result["kind"].as_str().unwrap(), &result)
                    .unwrap()
                    .result
                    .unwrap(),
                json!({"text": "nested result", "finish_reason": "stop"})
            );

            bridge.translate_emit(provider_invoke("req-top", "provider-b", json!([])));
            let top = event(
                "req-top",
                "provider-b",
                "completed",
                json!({"text": "top-level", "finish_reason": "stop"}),
            );
            assert_eq!(
                bridge
                    .take_reply(top["kind"].as_str().unwrap(), &top)
                    .unwrap()
                    .result
                    .unwrap(),
                json!({"text": "top-level", "finish_reason": "stop"})
            );
        }

        #[test]
        fn terminal_tool_calls_do_not_duplicate_aggregated_stream_calls() {
            let mut bridge = CapabilityBridge::new("tool-gate");
            bridge.translate_emit(provider_invoke("req-tools", "provider-a", json!([])));
            let split = event(
                "req-tools",
                "provider-a",
                "tool_call",
                json!({"id": "call-1", "name": "read_file", "arguments": {"path": "x"}}),
            );
            assert!(bridge
                .take_reply(split["kind"].as_str().unwrap(), &split)
                .is_none());
            let done = event(
                "req-tools",
                "provider-a",
                "completed",
                json!({"result": {"finish_reason": "tool_calls", "tool_calls": [{
                    "id": "call-1", "name": "read_file", "arguments": {"path": "x"}
                }]}}),
            );
            let result = bridge
                .take_reply(done["kind"].as_str().unwrap(), &done)
                .unwrap()
                .result
                .unwrap();
            assert_eq!(result["tool_calls"].as_array().unwrap().len(), 1);
        }

        #[test]
        fn tool_gate_translation_is_unchanged() {
            let mut bridge = CapabilityBridge::new("custom-gate");
            let invocation = json!({"run_id": "run-1", "actor_id": "worker"});
            let out = bridge.translate_emit(obj(json!({
                "kind": "tool.invoke", "id": "cap-1", "from": "worker",
                "name": "read_file", "invocation": invocation,
                "args": {"name": "read_file", "args": {"path": "src/lib.rs"},
                         "allowlist": ["read_file"], "da-policy": {"git": "read"}}
            })));
            assert_eq!(
                out,
                vec![obj(json!({
                    "kind": "custom-gate.tool.invoke", "id": "cap-1", "from": "worker",
                    "name": "read_file", "invocation": invocation,
                    "args": {"path": "src/lib.rs"}, "allowlist": ["read_file"],
                    "da-policy": {"git": "read"}
                }))]
            );
            let cancel = bridge.translate_emit(obj(json!({"kind": "tool.cancel", "id": "cap-1"})));
            assert_eq!(cancel[0]["kind"], "custom-gate.tool.cancel");
        }
    }
}

mod error {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/error.rs"));
}

pub mod kernel {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/kernel.rs"));
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/runtime.rs"));

#[cfg(test)]
mod tests {
    use super::*;

    fn port(id: &str, wire: &str) -> Value {
        serde_json::json!({"endpoint":{"constructor":"ActorEndpoint","value":{"id":id}},
            "type":{"kind":"primitive","name":"Unit"},"type_id":"unit","wire":wire})
    }

    #[test]
    fn versioned_program_envelope_unwraps_and_rejects_wrong_discriminators() {
        let initial = serde_json::json!({
            "types": {}, "actors": [], "junctions": [], "routes": [], "messages": [], "nodes": [], "kills": [], "result": {"from":port("answer", "out")}
        });
        let operation = serde_json::json!({
            "id":"expand", "on":{}, "captures":{},
            "expressions":[], "template":{}
        });
        let envelope = serde_json::json!({
            "format": "nefor.mag", "version": 3, "kind": "program",
            "program": {"initial": initial, "operations": [operation.clone()]}
        });
        let decoded = artifact_program(&envelope).unwrap();
        assert_eq!(decoded.initial, initial);
        assert_eq!(decoded.operations, vec![operation]);
        for invalid in [
            serde_json::json!({"format":"other","version":1,"kind":"program","program":{"initial":{},"operations":[]}}),
            serde_json::json!({"format":"nefor.mag","version":1,"kind":"program","program":{"initial":{},"operations":[]}}),
            serde_json::json!({"format":"nefor.mag","version":2,"kind":"program","program":{"initial":{},"operations":[]}}),
            serde_json::json!({"format":"nefor.mag","version":3,"kind":"delta","delta":{}}),
            serde_json::json!({"format":"nefor.mag","version":3,"kind":"program","program":{"initial":{},"operations":[1]}}),
        ] {
            assert!(
                artifact_modification(&invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn artifact_wire_boundary_rejects_mixed_and_unknown_fields() {
        let initial = serde_json::json!({
            "types": {}, "actors": [], "junctions": [], "routes": [], "messages": [], "nodes": [], "kills": [], "result": {"from":port("answer", "out")}
        });
        let delta = serde_json::json!({
            "types": {}, "actors": [], "junctions": [], "routes": [], "messages": [], "nodes": [], "kills": []
        });
        let program = serde_json::json!({
            "format":"nefor.mag", "version":3, "kind":"program",
            "program":{"initial":initial, "operations":[]}
        });
        let delta_envelope = serde_json::json!({
            "format":"nefor.mag", "version":3, "kind":"delta", "delta":delta
        });
        let mut mixed_program = program.clone();
        mixed_program["delta"] = delta_envelope["delta"].clone();
        assert!(artifact_program(&mixed_program).is_err());
        let mut mixed_delta = delta_envelope.clone();
        mixed_delta["program"] = program["program"].clone();
        assert!(artifact_delta(&mixed_delta).is_err());

        let mut program_unknown = program.clone();
        program_unknown["program"]["extra"] = Value::Bool(true);
        assert!(artifact_program(&program_unknown).is_err());
        let mut operation_unknown = program.clone();
        operation_unknown["program"]["operations"] = serde_json::json!([{
            "id":"x", "on":{}, "captures":{}, "expressions":[],
            "template":{}, "extra":true
        }]);
        assert!(artifact_program(&operation_unknown).is_err());
        let mut delta_operations = delta_envelope.clone();
        delta_operations["delta"]["operations"] = serde_json::json!([]);
        assert!(artifact_delta(&delta_operations).is_err());
        let mut delta_result = delta_envelope;
        delta_result["delta"]["result"] = serde_json::json!({});
        assert!(artifact_delta(&delta_result).is_err());
    }

    #[test]
    fn provider_schema_preflight_rejects_unsupported_types_before_activation() {
        let modification = serde_json::json!({
            "actors": [{
                "id": "answer",
                "factory": "structured-output",
                "params": {
                    "schema": {"version": 1, "root": {"kind": "json_value"}}
                }
            }]
        });
        let error = preflight_provider_schemas(&DecodedProgram {
            initial: modification,
            operations: Vec::new(),
        })
        .expect_err("JsonValue rejected");
        assert!(
            error.contains("structured-output actor \"actor:6:answer\""),
            "{error}"
        );
        assert!(error.contains("no faithful"), "{error}");
    }

    #[test]
    fn provider_schema_preflight_preserves_supported_lowering() {
        let modification = serde_json::json!({
            "actors": [{
                "id": "answer",
                "factory": "nefor.factory.structured-output",
                "params": {
                    "schema": {"version": 1, "root": {"kind": "record", "fields": [{
                        "name": "answer", "schema": {"kind": "int"}
                    }]}}
                }
            }, {
                "id": "prose",
                "factory": "llm",
                "params": {}
            }]
        });
        preflight_provider_schemas(&DecodedProgram {
            initial: modification,
            operations: Vec::new(),
        })
        .expect("supported schema lowers");
    }

    #[test]
    fn execute_principal_validates_shipped_routes_and_defaults_custom_routes_untrusted() {
        assert_eq!(
            authoritative_principal("agentic-loop", Some(&serde_json::json!("lead"))),
            Ok(RunPrincipal::Lead)
        );
        assert_eq!(
            authoritative_principal("lead-workflow", Some(&serde_json::json!("subagent"))),
            Ok(RunPrincipal::Subagent)
        );
        for (source, principal) in [
            ("agentic-loop", None),
            ("agentic-loop", Some(serde_json::json!("subagent"))),
            ("lead-workflow", Some(serde_json::json!("lead"))),
        ] {
            assert!(
                authoritative_principal(source, principal.as_ref()).is_err(),
                "shipped source {source:?} must reject {principal:?}"
            );
        }
        for (source, principal) in [
            ("engine", None),
            ("engine", Some(serde_json::json!("lead"))),
            ("direct-tool", Some(serde_json::json!("lead"))),
            ("custom-runner", Some(serde_json::json!("subagent"))),
        ] {
            assert_eq!(
                authoritative_principal(source, principal.as_ref()),
                Ok(RunPrincipal::Untrusted),
                "custom source {source:?} must execute without notice authority"
            );
        }
    }

    #[test]
    fn execution_model_snapshot_is_closed_optional_and_required_for_subagents() {
        let valid = serde_json::json!({
            "model_snapshot": {"provider": "p", "model": "m", "reasoning_effort": "high",
                "provider_options": {"service_tier": "fast"}}
        });
        let parsed = parse_model_snapshot(valid.as_object().unwrap(), RunPrincipal::Subagent)
            .expect("valid subagent snapshot")
            .expect("snapshot present");
        assert_eq!(parsed.provider, "p");
        assert_eq!(parsed.model, "m");
        assert_eq!(parsed.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            parsed.provider_options.as_ref().unwrap()["service_tier"],
            "fast"
        );
        assert!(parsed.profiles.is_empty());

        let with_profiles = serde_json::json!({
            "model_snapshot": {
                "provider": "current-provider",
                "model": "current-model",
                "profiles": {
                    "fast": {"provider": "fast-provider", "model": "fast-model"},
                    "standard": {
                        "provider": "standard-provider",
                        "model": "standard-model",
                        "reasoning_effort": "medium"
                    }
                }
            }
        });
        let parsed =
            parse_model_snapshot(with_profiles.as_object().unwrap(), RunPrincipal::Subagent)
                .unwrap()
                .unwrap();
        assert_eq!(parsed.profiles["fast"].provider, "fast-provider");
        assert_eq!(
            parsed.profiles["standard"].reasoning_effort.as_deref(),
            Some("medium")
        );

        let absent_effort = serde_json::json!({
            "model_snapshot": {"provider": "p", "model": "m"}
        });
        assert_eq!(
            parse_model_snapshot(absent_effort.as_object().unwrap(), RunPrincipal::Lead)
                .unwrap()
                .unwrap()
                .reasoning_effort,
            None
        );
        assert_eq!(
            parse_model_snapshot(&Map::new(), RunPrincipal::Lead).unwrap(),
            None,
            "lead execution preserves the no-snapshot compatibility path"
        );
        assert!(parse_model_snapshot(&Map::new(), RunPrincipal::Subagent).is_err());

        for malformed in [
            serde_json::json!(null),
            serde_json::json!({}),
            serde_json::json!({"provider": "", "model": "m"}),
            serde_json::json!({"provider": "p", "model": ""}),
            serde_json::json!({"provider": "p", "model": "m", "reasoning_effort": null}),
            serde_json::json!({"provider": "p", "model": "m", "reasoning_effort": ""}),
            serde_json::json!({"provider": "p", "model": "m", "provider_options": null}),
            serde_json::json!({"provider": "p", "model": "m", "provider_options": []}),
            serde_json::json!({"provider": "p", "model": "m", "profiles": null}),
            serde_json::json!({"provider": "p", "model": "m", "profiles": {"": {"provider": "p", "model": "m"}}}),
            serde_json::json!({"provider": "p", "model": "m", "profiles": {"fast": {"provider": "", "model": "m"}}}),
            serde_json::json!({"provider": "p", "model": "m", "profiles": {"fast": {"provider": "p", "model": ""}}}),
            serde_json::json!({"provider": "p", "model": "m", "profiles": {"fast": {"provider": "p", "model": "m", "reasoning_effort": null}}}),
            serde_json::json!({"provider": "p", "model": "m", "profiles": {"fast": {"provider": "p", "model": "m", "reasoning_effort": ""}}}),
            serde_json::json!({"provider": "p", "model": "m", "profiles": {"fast": {"provider": "p", "model": "m", "provider_options": null}}}),
            serde_json::json!({"provider": "p", "model": "m", "profiles": {"fast": {"provider": "p", "model": "m", "extra": true}}}),
            serde_json::json!({"provider": "p", "model": "m", "extra": true}),
        ] {
            let body = serde_json::json!({"model_snapshot": malformed});
            assert!(
                parse_model_snapshot(body.as_object().unwrap(), RunPrincipal::Lead).is_err(),
                "malformed snapshot must fail: {body}"
            );
        }
    }

    #[test]
    fn provider_tool_lifecycle_observation_is_closed_and_sanitized() {
        let completed = serde_json::json!({
            "event": "tool_execution_completed",
            "request_id": "request-1",
            "tool_call_id": "web-1",
            "name": "web_search",
            "arguments": {"action": "search", "query": "rust"},
            "result": {"status": "completed"},
            "provider_context": {"artifact": {"encrypted": "sealed"}},
            "extra": {"response_body": "private"}
        });
        assert_eq!(
            provider_tool_lifecycle_observation(completed.as_object().unwrap()).unwrap(),
            serde_json::json!({
                "kind": "tool_execution_completed",
                "tool_call_id": "web-1",
                "name": "web_search",
                "arguments": {"action": "search", "query": "rust"},
                "result": {"status": "completed"}
            })
        );

        let long_error = "x".repeat(PROVIDER_TOOL_ERROR_LIMIT + 20);
        let failed = serde_json::json!({
            "event": "tool_execution_failed",
            "tool_call_id": "web-2",
            "name": "web_search",
            "arguments": {},
            "error": long_error
        });
        let observed = provider_tool_lifecycle_observation(failed.as_object().unwrap()).unwrap();
        let error = observed["error"].as_str().unwrap();
        assert_eq!(error.chars().count(), PROVIDER_TOOL_ERROR_LIMIT + 1);
        assert!(error.ends_with('…'));

        for invalid in [
            serde_json::json!({
                "event": "tool_execution_started", "tool_call_id": "", "name": "web_search",
                "arguments": {}
            }),
            serde_json::json!({
                "event": "tool_execution_started", "tool_call_id": "web", "name": "web_search",
                "arguments": []
            }),
            serde_json::json!({
                "event": "tool_execution_completed", "tool_call_id": "web", "name": "web_search",
                "arguments": {}
            }),
        ] {
            assert!(provider_tool_lifecycle_observation(invalid.as_object().unwrap()).is_none());
        }
    }

    #[test]
    fn hello_body_advertises_version_and_kernel() {
        let contracts = serde_json::json!([{"identity": "nefor.factory.llm"}]);
        let b = hello_body(
            Some("mag-kernel"),
            &["sink".to_owned(), "llm".to_owned()],
            contracts,
        );
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.hello"));
        assert_eq!(
            b.get("version").and_then(Value::as_str),
            Some(PLUGIN_VERSION)
        );
        assert_eq!(b.get("kernel").and_then(Value::as_str), Some("mag-kernel"));
        let factories = b
            .get("factories")
            .and_then(Value::as_array)
            .expect("hello advertises factories");
        assert!(factories.iter().any(|f| f.as_str() == Some("sink")));
        assert_eq!(b["factory_contracts"][0]["identity"], "nefor.factory.llm");
    }

    #[test]
    fn hello_body_omits_kernel_when_absent() {
        let b = hello_body(None, &[], Value::Array(Vec::new()));
        assert!(b.get("kernel").is_none());
        // Factories always present, even when empty.
        assert_eq!(
            b.get("factories").and_then(Value::as_array).map(Vec::len),
            Some(0)
        );
    }

    #[test]
    fn pong_body_echoes_in_reply_to() {
        let b = pong_body(Some("ping-1"));
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.pong"));
        assert_eq!(b.get("in_reply_to").and_then(Value::as_str), Some("ping-1"));
    }

    #[test]
    fn goodbye_body_carries_reason() {
        let b = goodbye_body();
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.goodbye"));
        assert!(b.get("reason").and_then(Value::as_str).is_some());
    }

    #[test]
    fn loaded_body_carries_hash_and_artifact() {
        let b = loaded_body(
            Some("load-1"),
            "sha256:abc",
            serde_json::json!({"actors": []}),
            &["stub".to_owned(), "sink".to_owned()],
            serde_json::json!([{"identity": "nefor.factory.stub"}]),
        );
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.loaded"));
        assert_eq!(b.get("in_reply_to").and_then(Value::as_str), Some("load-1"));
        assert_eq!(b.get("hash").and_then(Value::as_str), Some("sha256:abc"));
        assert!(b.get("artifact").and_then(Value::as_object).is_some());
        let factories = b
            .get("factories")
            .and_then(Value::as_array)
            .expect("factories");
        assert!(factories.iter().any(|f| f.as_str() == Some("sink")));
        assert_eq!(b["factory_contracts"][0]["identity"], "nefor.factory.stub");
    }

    #[test]
    fn load_module_roots_default_to_source_dir_only() {
        let source = std::env::temp_dir().join(format!("mag-roots-default-{}", std::process::id()));
        std::fs::create_dir_all(&source).expect("source dir");
        let roots = load_module_roots(&Map::new(), &source).expect("default roots");
        assert_eq!(roots, vec![source.clone()]);
        std::fs::remove_dir_all(source).ok();
    }

    #[test]
    fn load_module_roots_accept_explicit_absolute_and_contained_roots() {
        let source =
            std::env::temp_dir().join(format!("mag-roots-explicit-{}", std::process::id()));
        let contained = source.join("lib");
        let external =
            std::env::temp_dir().join(format!("mag-roots-external-{}", std::process::id()));
        std::fs::create_dir_all(&contained).expect("contained root");
        std::fs::create_dir_all(&external).expect("external root");
        let body = serde_json::json!({"module_roots": ["lib", external.to_string_lossy()]})
            .as_object()
            .cloned()
            .expect("body");
        let roots = load_module_roots(&body, &source).expect("explicit roots");
        assert_eq!(
            roots[0],
            contained.canonicalize().expect("contained canonical")
        );
        assert_eq!(
            roots[1],
            external.canonicalize().expect("external canonical")
        );
        std::fs::remove_dir_all(source).ok();
        std::fs::remove_dir_all(external).ok();
    }

    #[test]
    fn load_module_roots_reject_relative_escape() {
        let source = std::env::temp_dir().join(format!("mag-roots-escape-{}", std::process::id()));
        std::fs::create_dir_all(&source).expect("source dir");
        let body = serde_json::json!({"module_roots": [".."]})
            .as_object()
            .cloned()
            .expect("body");
        assert!(load_module_roots(&body, &source)
            .expect_err("escape rejected")
            .contains("escapes source_dir"));
        std::fs::remove_dir_all(source).ok();
    }

    #[test]
    fn artifact_boundary_preserves_factory_identity() {
        let artifact = serde_json::json!({
            "types": {},
            "actors": [{"id": "answer", "factory": "nefor.factory.llm", "type_arguments": [], "input":port("answer","in"), "outputs":[port("answer","out")],
                "params": {"$mag": "packed-value", "value": {}}}],
            "junctions": [], "routes": [], "messages": [], "nodes": [], "kills": [], "result": {"from":port("answer", "out")}
        });
        let envelope = serde_json::json!({"format":"nefor.mag","version":3,"kind":"program",
            "program":{"initial":artifact,"operations":[]}});
        let modification = artifact_modification(&envelope).expect("valid artifact");
        assert_eq!(modification["actors"][0]["factory"], "nefor.factory.llm");
        assert_eq!(
            modification["actors"][0]["type_arguments"],
            serde_json::json!([])
        );
    }

    #[test]
    fn artifact_boundary_preserves_structural_result_metadata() {
        let artifact = serde_json::json!({
            "types": {},
            "actors": [{"id": "answer", "factory": "nefor.factory.llm", "type_arguments": [], "input":port("answer","in"), "outputs":[port("answer","out")],
                "params": {"$mag": "packed-value", "value": {}}}],
            "junctions": [], "routes": [], "messages": [], "nodes": [], "kills": [],
            "result": {"from": {
                "endpoint": {"constructor":"ActorEndpoint","value":{"id":"answer"}},
                "type": {"kind":"named","name":"audit.CodeAudit"},
                "type_id": "audit.CodeAudit",
                "wire": "generic-provider.TextAnswer"
            }}
        });
        let envelope = serde_json::json!({"format":"nefor.mag","version":3,"kind":"program",
            "program":{"initial":artifact,"operations":[]}});
        let modification = artifact_modification(&envelope).expect("valid artifact");
        assert_eq!(
            modification["result"]["from"]["endpoint"]["value"]["id"],
            "answer"
        );
        assert_eq!(
            modification["result"]["from"]["wire"],
            "generic-provider.TextAnswer"
        );
        assert_eq!(modification["actors"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn artifact_boundary_rejects_non_object() {
        let artifact = serde_json::json!([]);
        assert!(artifact_modification(&artifact)
            .expect_err("non-object must be rejected")
            .contains("artifact must be an object"));
    }

    #[test]
    fn params_overlay_patches_named_actors_only() {
        let mut modification = serde_json::json!({
            "actors": [
                { "id": "build", "factory": "llm", "params": { "prompt": "x" }, "routes": {} },
                { "id": "sink",  "factory": "sink", "params": {}, "routes": {} }
            ]
        });
        let overlay = serde_json::json!({
            "actor:5:build": { "provider": "chatgpt", "model": "gpt-5.5", "reasoning_effort": "high" }
        });
        let mut program = DecodedProgram {
            initial: modification,
            operations: Vec::new(),
        };
        apply_params_overlay(&mut program, overlay.as_object().unwrap()).unwrap();
        modification = program.initial;

        let actors = modification["actors"].as_array().unwrap();
        let build = &actors[0]["params"];
        assert_eq!(build["prompt"].as_str(), Some("x"), "existing param kept");
        assert_eq!(
            build["provider"].as_str(),
            Some("chatgpt"),
            "overlay merged"
        );
        assert_eq!(build["reasoning_effort"].as_str(), Some("high"));
        // The unnamed actor is untouched.
        assert!(actors[1]["params"].as_object().unwrap().is_empty());
    }

    #[test]
    fn params_overlay_creates_params_when_missing_or_non_object() {
        let mut modification = serde_json::json!({
            "actors": [ { "id": "a", "factory": "llm" } ]
        });
        let overlay = serde_json::json!({ "actor:1:a": { "model": "m" } });
        let mut program = DecodedProgram {
            initial: modification,
            operations: Vec::new(),
        };
        apply_params_overlay(&mut program, overlay.as_object().unwrap()).unwrap();
        modification = program.initial;
        assert_eq!(
            modification["actors"][0]["params"]["model"].as_str(),
            Some("m")
        );
    }

    #[test]
    fn params_overlay_cannot_replace_compiler_derived_params() {
        let original = serde_json::json!({"version": 1, "root": {"kind": "string"}});
        let modification = serde_json::json!({
            "actors": [
                {
                    "id": "typed",
                    "factory": "nefor.factory.structured-output",
                    "params": {
                        "schema": original,
                        "provider": "mock-provider",
                        "output_type": "output-id",
                        "error_type": "agent-error-id",
                        "provider_error_type": "provider-id",
                        "validation_error_type": "validation-id"
                    }
                },
                {
                    "id": "direct",
                    "factory": "nefor.factory.llm",
                    "params": {
                        "model_profile": {"present": true, "value": "fast"}
                    }
                }
            ]
        });
        for (actor, param, value) in [
            (
                "typed",
                "schema",
                serde_json::json!({"version": 1, "root": {"kind": "string"}}),
            ),
            ("typed", "output_type", serde_json::json!("forged")),
            ("typed", "error_type", serde_json::json!("forged")),
            ("typed", "provider_error_type", serde_json::json!("forged")),
            (
                "typed",
                "validation_error_type",
                serde_json::json!("forged"),
            ),
            (
                "typed",
                "model_profile",
                serde_json::json!({"present": true, "value": "other"}),
            ),
            (
                "direct",
                "model_profile",
                serde_json::json!({"present": true, "value": "other"}),
            ),
        ] {
            let overlay = serde_json::json!({(initial_actor_address(actor)): {(param): value}});
            let error = apply_params_overlay(
                &mut DecodedProgram {
                    initial: modification.clone(),
                    operations: Vec::new(),
                },
                overlay.as_object().unwrap(),
            )
            .unwrap_err();
            assert!(error.contains(&format!("protected compiler-derived param {param:?}")));
        }
        assert_eq!(modification["actors"][0]["params"]["schema"], original);
        assert_eq!(
            modification["actors"][0]["params"]["provider_error_type"],
            "provider-id"
        );
        assert_eq!(
            modification["actors"][0]["params"]["validation_error_type"],
            "validation-id"
        );
        assert_eq!(
            modification["actors"][0]["params"]["output_type"],
            "output-id"
        );
        assert_eq!(
            modification["actors"][0]["params"]["error_type"],
            "agent-error-id"
        );
    }

    #[test]
    fn delta_envelope_is_required_and_wrong_variants_are_rejected() {
        let delta = serde_json::json!({"types": {}, "actors": [], "junctions": [], "routes": [], "messages": [], "kills": [], "nodes": []});
        let envelope = serde_json::json!({
            "format": "nefor.mag", "version": 3, "kind": "delta", "delta": delta
        });
        assert_eq!(artifact_delta(&envelope).unwrap(), delta);
        for invalid in [
            delta,
            serde_json::json!({"format":"nefor.mag","version":3,"kind":"program","program":{"initial":{},"operations":[]}}),
            serde_json::json!({"format":"other","version":1,"kind":"delta","delta":{}}),
        ] {
            assert!(artifact_delta(&invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn packed_value_unwrap_preserves_user_records_with_the_same_field_names() {
        let authored = serde_json::json!({
            "type": "sha256:user-authored",
            "value": {"nested": true}
        });
        let mut packed = serde_json::json!({
            "$mag": "packed-value",
            "value": authored
        });

        unpack_packed(&mut packed, "test packed value").unwrap();

        assert_eq!(packed, authored);
    }

    #[test]
    fn artifact_decode_unpacks_only_known_outer_boundaries() {
        let authored = serde_json::json!({
            "type": "sha256:user-authored",
            "value": {"nested": true}
        });
        let packed = |value: Value| {
            serde_json::json!({
                "$mag": "packed-value", "value": value
            })
        };
        let artifact = serde_json::json!({
            "format": "nefor.mag", "version": 3, "kind": "program", "program": {
                "initial": {
                    "types": {},
                    "actors": [{"id": "initial", "factory": "stub", "type_arguments":[], "input":port("initial","in"), "outputs":[port("initial","out")], "params": packed(authored.clone())}],
                    "junctions": [], "routes": [],
                    "messages": [{
                        "to": {"endpoint":{"constructor":"ActorEndpoint","value":{"id":"initial"}},
                            "type":{},"type_id":"type","wire":"in"},
                        "semantic_type":{},"semantic_type_id":"type",
                        "content": packed(authored.clone())
                    }],
                    "kills": [], "nodes": [], "result": {"from":port("initial","out")}
                },
                "operations": [{
                    "id":"expand", "on":{},
                    "captures": {"saved": {"value": packed(authored.clone())}},
                    "expressions": [],
                    "template": {
                        "types": {},
                        "actors": [{"slot": "worker", "factory": "stub", "params": packed(authored.clone())}],
                        "messages": [{"to": {}, "content": {"constructor": "Static", "value": packed(authored.clone())}}],
                        "kills": [], "nodes": []
                    }
                }]
            }
        });

        let mut unwrapped = artifact.clone();
        unwrapped["program"]["initial"]["actors"][0]["params"] = authored.clone();
        let error = match artifact_program(&unwrapped) {
            Ok(_) => panic!("raw user record was mistaken for a packed value"),
            Err(error) => error,
        };
        assert!(error.contains("must be a compiler-owned packed value"));

        let decoded = artifact_program(&artifact).unwrap();
        assert_eq!(decoded.initial["actors"][0]["params"], authored);
        assert_eq!(decoded.initial["messages"][0]["content"], authored);
        assert_eq!(
            decoded.operations[0]["captures"]["saved"]["value"],
            authored
        );
        assert_eq!(
            decoded.operations[0]["template"]["actors"][0]["params"],
            authored
        );
        assert_eq!(
            decoded.operations[0]["template"]["messages"][0]["content"],
            serde_json::json!({"constructor": "Static", "value": authored})
        );
    }

    #[test]
    fn template_actor_overlays_use_collision_free_addresses_and_preserve_artifact() {
        let artifact = serde_json::json!({
            "format":"nefor.mag", "version":3, "kind":"program", "program":{
                "initial":{"types":{},"actors":[{"id":"same","factory":"llm","type_arguments":[],"input":port("same","in"),"outputs":[port("same","out")],"params":{
                    "$mag":"packed-value","value":{"model":"authored"}
                }}],"junctions":[],"routes":[],"messages":[],"nodes":[],"kills":[],"result":{"from":port("same","out")}},
                "operations":[{"id":"expand","on":{},"captures":{},"expressions":[],
                    "template":{"actors":[
                    {"slot":"same","factory":"llm","params":{
                        "$mag":"packed-value","value":{"model":"template"}
                    }}
                ]}}]
            }
        });
        let original = artifact.clone();
        let mut decoded = artifact_program(&artifact).unwrap();
        let inventory = actor_inventory(&decoded);
        assert_eq!(inventory[0].0, "actor:4:same");
        assert_eq!(inventory[1].0, "operation:6:expand:template:4:same");
        assert_ne!(inventory[0].0, inventory[1].0);
        let overlay = serde_json::json!({
            "operation:6:expand:template:4:same": {"model":"overlaid"}
        });
        apply_params_overlay(&mut decoded, overlay.as_object().unwrap()).unwrap();
        assert_eq!(
            decoded.operations[0]["template"]["actors"][0]["params"]["model"],
            "overlaid"
        );
        assert_eq!(decoded.initial["actors"][0]["params"]["model"], "authored");
        assert_eq!(
            artifact, original,
            "execution material must not mutate the immutable artifact"
        );
        assert_eq!(
            immutable_artifact_hash(&artifact),
            immutable_artifact_hash(&original)
        );
    }

    #[test]
    fn error_body_carries_message() {
        let b = error_body(Some("req-9"), "boom");
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.error"));
        assert_eq!(b.get("in_reply_to").and_then(Value::as_str), Some("req-9"));
        assert_eq!(b.get("message").and_then(Value::as_str), Some("boom"));
    }
}

#[cfg(test)]
mod project_build_tests {
    use super::*;
    use serde_json::json;

    fn host() -> LuaHost {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        LuaHost::load_kernel(
            &root.join("lua/mag-kernel/init.lua"),
            Some(&root.join("../../lua")),
        )
        .unwrap()
    }

    async fn build(host: &LuaHost, request: Value) -> Map<String, Value> {
        let (tx, mut rx) = mpsc::channel(4);
        handle_build(&tx, request.as_object().unwrap(), Some("build-id"), host)
            .await
            .unwrap();
        let body = rx.recv().await.unwrap().body;
        let Body::Event(body) = body else {
            panic!("body")
        };
        body
    }

    #[test]
    fn project_build_decodes_deep_exact_values_and_rejects_trailing_data() {
        let bytes = format!("{}0{}\n", "[".repeat(300), "]".repeat(300));
        assert!(serde_json::from_str::<Value>(&bytes).is_err());
        let value = decode_build_artifact(bytes.as_bytes()).unwrap();
        assert_eq!(serde_json::to_string(&value).unwrap(), bytes.trim());
        assert!(decode_build_artifact(b"{} {}").is_err());
    }

    #[tokio::test]
    async fn project_build_cache_equivalence_corruption_bypass_and_preflight() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("mag.toml"), "version = 1\n").unwrap();
        // An empty delta is accepted without starting any actors.
        let artifact = json!({"format":"nefor.mag", "version":3, "kind":"delta",
            "delta":{"types":{},"actors":[],"junctions":[],"routes":[],"messages":[],"nodes":[],"kills":[]}});
        // Use read-json so the artifact and deeply nested extension are ordinary observed data.
        std::fs::write(
            project.path().join("main.mag"),
            "artifact(read_json(\"artifact.json\"))",
        )
        .unwrap();
        std::fs::write(
            project.path().join("artifact.json"),
            serde_json::to_vec(&artifact).unwrap(),
        )
        .unwrap();
        let request =
            json!({"project_root":project.path(), "entry":"main.mag", "cache_dir":cache.path()});
        let host = host();
        let miss = build(&host, request.clone()).await;
        assert_eq!(miss["kind"], "mag.loaded", "{miss:?}");
        assert_eq!(miss["build"]["status"], "miss");
        let hit = build(&host, request.clone()).await;
        assert_eq!(hit["build"]["status"], "hit");
        assert_eq!(miss["artifact"], hit["artifact"]);
        assert_eq!(miss["hash"], hit["hash"]);
        let mut bypass = request.clone();
        bypass["no_cache"] = json!(true);
        let bypass = build(&host, bypass).await;
        assert_eq!(bypass["build"]["status"], "bypass");
        assert_eq!(bypass["hash"], hit["hash"]);
        fn corrupt(path: &Path) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    corrupt(&path);
                } else {
                    std::fs::write(path, b"{").unwrap();
                }
            }
        }
        corrupt(cache.path());
        assert_eq!(
            build(&host, request.clone()).await["build"]["status"],
            "miss"
        );
        std::fs::write(project.path().join("artifact.json"), b"{}").unwrap();
        for _ in 0..2 {
            let rejected = build(&host, request.clone()).await;
            assert_eq!(rejected["kind"], "mag.error");
        }
        let mut invalid = request;
        invalid["cache_dir"] = json!("relative");
        assert!(parse_build_request(invalid.as_object().unwrap()).is_err());
    }
}

#[tokio::test]
async fn project_build_deep_program_hit_rechecks_current_kernel() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("mag.toml"), "version = 1\n").unwrap();
    let bindings = (1..=140)
        .map(|index| format!("let n{index} = [n{}]\n", index - 1))
        .collect::<String>();
    let deep_type = (0..140).fold("Int".to_owned(), |ty, _| format!("List<{ty}>"));
    let source = format!(
        r#"type ActorId {{id: String}}
type Endpoint = ActorEndpoint(ActorId)
type Port {{endpoint: Endpoint, `type`: TypeDescriptor, type_id: SemanticTypeId, wire: String}}
type Boundary {{from: Port}}
type Types {{deep: {deep_type}}}
type Initial {{types: Types, actors: List<String>, junctions: List<String>, routes: List<String>, messages: List<String>, nodes: List<String>, kills: List<String>, result: Boundary}}
type Program {{initial: Initial, operations: List<String>}}
type Envelope {{format: String, version: Int, kind: String, program: Program}}
let n0 = 0
{bindings}artifact(Envelope {{
  format: "nefor.mag",
  version: 3,
  kind: "program",
  program: Program {{
    initial: Initial {{
      types: Types {{deep: n140}},
      actors: ([]: List<String>),
      junctions: ([]: List<String>),
      routes: ([]: List<String>),
      messages: ([]: List<String>),
      nodes: ([]: List<String>),
      kills: ([]: List<String>),
      result: Boundary {{from: Port {{endpoint: named(Endpoint, ActorEndpoint, ActorId {{id: "terminal"}}), `type`: type_evidence(type_tag<Unit>()), type_id: type_id(type_evidence(type_tag<Unit>())), wire: "out"}}}},
    }},
    operations: ([]: List<String>),
  }},
}})"#
    );
    std::fs::write(project.path().join("main.mag"), source).unwrap();
    let kernel = project.path().join("kernel.lua");
    std::fs::write(
        &kernel,
        "return {preflight_program = function() return {ok=true} end}",
    )
    .unwrap();
    let accepted = LuaHost::load_kernel(&kernel, None).unwrap();
    let request = serde_json::json!({"project_root":project.path(), "entry":"main.mag", "cache_dir":cache.path()});
    let mut hash = None;
    for status in ["miss", "hit"] {
        let (tx, mut rx) = mpsc::channel(4);
        handle_build(&tx, request.as_object().unwrap(), Some("deep"), &accepted)
            .await
            .unwrap();
        let Body::Event(reply) = rx.recv().await.unwrap().body else {
            panic!("event")
        };
        assert_eq!(reply["kind"], "mag.loaded", "{reply:?}");
        assert_eq!(reply["build"]["status"], status);
        if let Some(hash) = &hash {
            assert_eq!(&reply["hash"], hash);
        }
        hash = Some(reply["hash"].clone());
        let bytes = serde_json::to_vec(&reply["artifact"]).unwrap();
        assert!(serde_json::from_slice::<Value>(&bytes).is_err());
    }
    std::fs::write(&kernel, "return {preflight_program = function() return {ok=false,error='current kernel rejection'} end}").unwrap();
    let rejected = LuaHost::load_kernel(&kernel, None).unwrap();
    let (tx, mut rx) = mpsc::channel(4);
    handle_build(&tx, request.as_object().unwrap(), Some("deep"), &rejected)
        .await
        .unwrap();
    let Body::Event(reply) = rx.recv().await.unwrap().body else {
        panic!("event")
    };
    assert_eq!(reply["kind"], "mag.error");
    assert_eq!(reply["message"], "current kernel rejection");
}
