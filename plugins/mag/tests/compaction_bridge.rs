#![allow(dead_code)]
#[path = "../src/bridge.rs"]
mod bridge;
#[path = "../src/error.rs"]
mod error;
mod kernel {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/kernel.rs"));
    #[test]
    fn source_config_policy_and_personal_agents_are_runtime_consumers() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let host = LuaHost::load_kernel(
            &root.join("lua/mag-kernel/init.lua"),
            Some(&root.join("../../lua")),
        )
        .unwrap();
        // Default regression exercises the consumer shape; the opt-in path reads the real source config.
        let config = std::env::var_os("NEFOR_SOURCE_CONFIG").map(std::path::PathBuf::from);
        let snapshot_source = match &config {
            Some(config) => format!("local resolver = dofile({}); return resolver.resolve({{provider='chatgpt',model='gpt-6-astra',reasoning_effort='high'}})", serde_json::to_string(&config.join("config/model-profiles.lua")).unwrap()),
            None => "local profiles={}; for _,name in ipairs({'current','fast','efficient','standard','deep'}) do profiles[name]={provider='chatgpt',model='gpt-test',compaction_policy={soft_percent=60,step_percent=10,hard_percent=90}} end; return {provider='chatgpt',model='gpt-test',profiles=profiles}".to_owned(),
        };
        let snapshot: mlua::Table = host.lua.load(&snapshot_source).eval().unwrap();
        host.lua.globals().set("source_snapshot", snapshot).unwrap();
        let encoded: String = host.lua.load("local copy,err=require('libs.model-snapshot').copy(source_snapshot); assert(copy,err); assert(copy.compaction_policy==nil); for _,name in ipairs({'current','fast','efficient','standard','deep'}) do local p=copy.profiles[name].compaction_policy; assert(p.soft_percent==60 and p.step_percent==10 and p.hard_percent==90); assert(p~=source_snapshot.profiles[name].compaction_policy) end; return nefor.json.encode(copy)").eval().unwrap();
        let snapshot = serde_json::from_str::<ExecutionModelSnapshot>(&encoded)
            .unwrap()
            .validate()
            .unwrap();
        assert!(snapshot.compaction_policy.is_none());
        if let Some(config) = &config {
            host.lua
                .globals()
                .set(
                    "profile_resolver_path",
                    config.join("config/model-profiles.lua").to_str().unwrap(),
                )
                .unwrap();
            let disabled: String = host.lua.load(r#"
                local getenv = os.getenv
                os.getenv = function(key)
                    if key == "NEFOR_DISTRIBUTION" then return "nefor-glm" end
                    return getenv(key)
                end
                local ok, resolver = pcall(dofile, profile_resolver_path)
                os.getenv = getenv
                assert(ok, resolver)
                local snapshot, err = require("libs.model-snapshot").copy(resolver.resolve({provider="chatgpt",model="gpt-6-astra"}))
                assert(snapshot, err)
                assert(snapshot.compaction_policy == nil)
                for _, profile in pairs(snapshot.profiles) do assert(profile.compaction_policy == nil) end
                return nefor.json.encode(snapshot)
            "#).eval().unwrap();
            let disabled = serde_json::from_str::<ExecutionModelSnapshot>(&disabled)
                .unwrap()
                .validate()
                .unwrap();
            assert!(disabled.compaction_policy.is_none());
            assert!(disabled
                .profiles
                .values()
                .all(|profile| profile.compaction_policy.is_none()));
        }
        let module_root = config
            .as_ref()
            .map(|c| c.join("mag/lib"))
            .unwrap_or_else(|| root.join("../../examples/nefor-agent/mag/lib"));
        let source = r#"import agents.{}
import nefor.{>>>, TextAnswer, ToolApprovalPolicy, compile_graph, sequence, source}
let workers = indexed_map(((|index, model| => agents.agent<String, TextAnswer>(str(index), agents.AgentConfig {model:model,system:"paired config",tools:[],tool_approval_policy:ToolApprovalPolicy.Default})): fn(Int, agents.Model) -> nefor.Node<String, core.types.Result<nefor.AgentError, TextAnswer>>), [agents.current,agents.fast,agents.efficient,agents.standard,agents.deep])
compile_graph(source("task", "begin") >>> sequence(workers))"#;
        let source = if config.is_some() {
            source
        } else {
            r#"import nefor.{>>>, AgentConfig, AuthoredModel, Node, TextAnswer, ToolApprovalPolicy, agent, compile_graph, model_profile, sequence, source}
let resolve: fn(String) -> AuthoredModel = |profile| => AuthoredModel.ModelProfile(model_profile(profile))
let workers = map(((|profile| => agent<String, String, TextAnswer>(profile, resolve, AgentConfig<String> {model:profile,system:"consumer regression",tools:[],tool_approval_policy:ToolApprovalPolicy.Default})): fn(String) -> Node<String, core.types.Result<nefor.AgentError, TextAnswer>>), ["current","fast","efficient","standard","deep"])
compile_graph(source("task", "begin") >>> sequence(workers))"#
        };
        let source = format!("import core.types.{{}}\n{source}");
        let artifact = nefor_mag::compile_with_inputs_and_module_roots_and_options_and_syntax(
            &source,
            &root,
            serde_json::json!({"factory_contracts":host.registry_contracts().unwrap()}),
            &[root.join("../../mag/lib"), module_root],
            nefor_mag::CompilerOptions::default(),
            nefor_mag::SyntaxMode::New,
        )
        .unwrap();
        let mut initial = artifact["program"]["initial"].clone();
        for actor in initial["actors"].as_array_mut().unwrap() {
            actor["params"] = actor["params"]["value"].clone();
        }
        for message in initial["messages"].as_array_mut().unwrap() {
            message["content"] = message["content"]["value"].clone();
        }
        // Observe parameters at the real factory constructor, retaining its implementation and async boundary.
        host.lua.load("local factory=require('factories.llm'); local original=factory.construct; captured={}; factory.construct=function(id,params,emit,deps) captured[id]={params=params,root=deps.conversation.is_root,facet=deps.compaction}; return original(id,params,emit,deps) end; package.loaded.init=nil").exec().unwrap();
        let kernel: mlua::Table = host.lua.load("return require('init')").eval().unwrap();
        let snapshot = host.lua.to_value(&snapshot).unwrap();
        let begin: mlua::Function = kernel.get("begin_run").unwrap();
        let info = host.lua.create_table().unwrap();
        info.set("run_id", "paired").unwrap();
        info.set("run_name", "paired").unwrap();
        info.set("session_id", "session").unwrap();
        info.set("conversation_id", "root").unwrap();
        info.set("model_snapshot", snapshot).unwrap();
        let begun: mlua::Table = begin.call(info).unwrap();
        assert!(begun.get::<bool>("ok").unwrap());
        let start: mlua::Function = kernel.get("start").unwrap();
        let outcome: mlua::Table = start
            .call(("paired", host.lua.to_value(&initial).unwrap()))
            .unwrap();
        assert!(
            outcome.get::<bool>("ok").unwrap(),
            "{:?}",
            outcome.get::<Option<String>>("error").unwrap()
        );
        host.lua.load("local n=0; for _,c in pairs(captured) do n=n+1; local p=c.params.compaction_policy; assert(p.soft_percent==60 and p.step_percent==10 and p.hard_percent==90); assert(type(c.facet.barrier)=='function') end; assert(n==5, 'every personal profile constructs')").exec().unwrap();
        let initial = &initial;
        let mut root_graph = initial.clone();
        for actor in root_graph["actors"].as_array_mut().unwrap() {
            if actor["factory"] == "nefor.factory.llm" {
                actor["params"]["conversation_id"] = serde_json::json!("root");
            }
        }
        host.lua
            .globals()
            .set("root_graph", host.lua.to_value(&root_graph).unwrap())
            .unwrap();
        host.lua.load("captured={}; local k=require('init'); assert(k.end_run('paired')); local snapshot=assert(require('libs.model-snapshot').copy(source_snapshot)); assert(k.begin_run({run_id='root-check',run_name='root-check',conversation_id='root',model_snapshot=snapshot}).ok); assert(k.start('root-check',root_graph).ok); local n=0; for _,c in pairs(captured) do n=n+1; assert(c.root and c.params.compaction_policy==nil) end; assert(n==5)").exec().unwrap();
    }

    pub fn fixture(path: &std::path::Path) {
        run(path)
    }
    fn run(path: &std::path::Path) {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let host = LuaHost::load_kernel(
            &root.join("lua/mag-kernel/init.lua"),
            Some(&root.join("../../lua")),
        )
        .expect("shipped kernel");
        let source = std::fs::read_to_string(path).expect("fixture");
        host.lua
            .load(source)
            .set_name("compaction_broker_test")
            .exec()
            .expect("broker contract");
    }
}
#[test]
fn asynchronous_compaction_broker_contract() {
    kernel::fixture(
        &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/lua/mag-kernel/compaction_broker_test.lua"),
    );
}

#[test]
fn canonical_manager_async_wire_contract() {
    kernel::fixture(
        &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/lua/mag-kernel/compaction_async_wire_test.lua"),
    );
}
#[test]
fn live_provider_preparation_contract() {
    kernel::fixture(
        &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/lua/mag-kernel/compaction_preparation_test.lua"),
    );
}
#[test]
fn production_lazy_injection_and_fact_ack() {
    kernel::fixture(
        &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/lua/mag-kernel/compaction_lowering_test.lua"),
    );
}

#[test]
fn context_receipts_require_the_open_matching_provider_edge() {
    use serde_json::json;
    let mut bridge = bridge::CapabilityBridge::new("tool-gate");
    let invoke = json!({"kind":"tool.invoke","class":"provider","id":"native-1","name":"p",
        "args":{"conversation_id":"c","model":"m","context_watermark":17,"notice_id":"n","epoch_id":"e"}});
    let emitted = bridge.translate_emit(invoke.as_object().unwrap().clone());
    assert_eq!(emitted[0]["context_watermark"], 17);
    assert_eq!(emitted[0]["notice_id"], "n");
    assert_eq!(emitted[0]["epoch_id"], "e");
    let receipt = json!({"request_id":"native-1","provider":"p"});
    let body = receipt.as_object().unwrap();
    let kind = "conversation.provider.context_included";
    assert_eq!(
        bridge.context_inclusion_request_id("p", kind, body),
        Some("native-1")
    );
    assert_eq!(
        bridge.context_inclusion_request_id("other", kind, body),
        None
    );
    assert_eq!(
        bridge.context_inclusion_request_id("conversation-manager", kind, body),
        None
    );
    assert_eq!(
        bridge.context_inclusion_request_id("p", "other", body),
        None
    );
    bridge.forget("native-1");
    assert_eq!(bridge.context_inclusion_request_id("p", kind, body), None);
}
