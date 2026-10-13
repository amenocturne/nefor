-- Exercise the actual lazy constructor without executing any provider capability.
local factory = require("factories.llm")
local original = factory.construct
local captured
factory.construct = function(id, params, emit, deps)
  captured = { params = params, deps = deps }
  return { id = id, deliver = function() return nil end }
end
package.loaded["init"] = nil
local kernel = require("init")
factory.construct = original
local function program()
  local function array(values) return nefor.json.mark_array(values) end
  local function named(name) return {kind="named",name=name,arguments=array({})} end
  local provider_input = named("nefor.contracts.ProviderInput")
  local text_answer = named("nefor.contracts.TextAnswer")
  local agent_error = named("nefor.contracts.AgentError")
  local tool_calls = named("nefor.wire.ToolCalls")
  local result_type = {kind="adt",name="core.types.Result",
    arguments=array({agent_error,text_answer}),constructors=array({
      {name="Error",payload=agent_error},{name="Ok",payload=text_answer},
    })}
  local function endpoint(id)
    return {constructor="ActorEndpoint",value={id=id}}
  end
  local function port(wire, descriptor)
    return {endpoint=endpoint("agent"),type=descriptor,
      type_id=nefor.semantic_type.id(descriptor),wire=wire}
  end
  local input = port("generic-provider.ProviderOut", provider_input)
  local result = port("nefor.agent.Result", result_type)
  return {
    actors = {
      {
        id = "agent",
        factory = "llm",
        type_arguments = {result_type},
        params = { tool_approval_policy = { constructor = "Default" },  model = "m", provider = "prov", system = "answer",
          output_type = "text-answer-id", error_type = "agent-error-id",
          provider_error_type = "provider-error-id" },
        evidence={version=2,identity="nefor.factory.llm",arguments={result_type},input=provider_input,output=result_type},
        input=input,
        outputs={port("generic-tool.ToolCalls",tool_calls),result},
      },
    },
    routes = {},
    messages = {
      {to=input,transforms={},semantic_type=provider_input,semantic_type_id=nefor.semantic_type.id(provider_input),content={
        kind="generic-provider.ProviderOut",
        value={messages={{role="user",content="hi"}}},
        semantic_value={messages={{role="user",content="hi"}}},
        messages={{role="user",content="hi"}},
      }},
    },
    kills = {},
    nodes = { { path = { "agent" }, members = { "agent" } } },
    result = {from={type=result_type,type_id=nefor.semantic_type.id(result_type),
      leaves={{port=result,steps={}}},through={}}},
  }
end


local thresholds = {soft_percent=60,step_percent=10,hard_percent=90}
local snapshot = { provider="p", model="m", profiles={
  current={provider="p",model="m",compaction_policy=thresholds},
  disabled={provider="p",model="m"},
} }
local function check(id, params, top_policy, expected, root)
  captured = nil
  snapshot.compaction_policy = top_policy
  assert(kernel.begin_run({run_id=id,run_name=id,conversation_id="root",model_snapshot=snapshot}).ok)
  local graph = program()
  for k,v in pairs(params) do graph.actors[1].params[k] = v end
  assert(kernel.start(id,graph).ok)
  assert(captured, "factory constructed")
  local facet = captured.deps.compaction
  for _, op in ipairs({"barrier","read","prepare","append_notice","bind_notice","acknowledge_notice","request_compaction","cancel_compaction","detach"}) do
    assert(type(facet[op]) == "function", op)
  end
  local cid = captured.deps.conversation.id
  local eid = id .. "/test-fact"
  captured.deps.conversation.emit({event_id=eid,conversation_id=cid})
  local settled = false
  facet.barrier({conversation_id=cid},function(outcome)
    assert(outcome.kind=="Ok" and outcome.value.watermark==12)
    settled=true
  end)
  kernel.pump_commands(); assert(not settled)
  kernel.canonical_event("wrong-source",{kind="conversation.fact.recorded",event={event_id=eid,conversation_id=cid,sequence=12}})
  kernel.pump_commands(); assert(not settled)
  kernel.canonical_event("conversation-manager",{kind="conversation.fact.recorded",event={event_id=eid,conversation_id=cid,sequence=12}})
  assert(not settled)
  kernel.pump_commands(); assert(settled)
  assert(captured.deps.conversation.is_root == root)
  local actual = captured.params.compaction_policy
  if expected == nil then assert(actual == nil) else
    assert(actual.soft_percent == expected.soft_percent and actual.hard_percent == expected.hard_percent)
  end
  assert(graph.actors[1].params.compaction_policy == params.compaction_policy, "authored spec unchanged")
  local resumed_after_kill = false
  local pending = facet.read({conversation_id=cid,min_watermark=12},function()
    resumed_after_kill=true
  end)
  kernel.end_run(id,"killed")
  kernel.canonical_event("conversation-manager",{kind="conversation.context.snapshot",request_id=pending.id,
    conversation_id=cid,found=true,context={watermark=12}})
  kernel.pump_commands(); assert(not resumed_after_kill)
end
local disabled={constructor="Disabled",value=nefor.json.decode("null")}
check("profile",{model_profile={present=true,value="current"},compaction_policy=disabled},nil,thresholds,false)
check("direct",{compaction_policy={constructor="Thresholds",value=thresholds}},nil,thresholds,false)
check("root",{conversation_id="root",model_profile={present=true,value="current"},compaction_policy=disabled},nil,nil,true)
check("root-enabled",{conversation_id="root",model_profile={present=true,value="disabled"},compaction_policy=disabled},thresholds,thresholds,true)
check("dynamic-relocated-worker",{model_profile={present=true,value="current"},compaction_policy=disabled},nil,thresholds,false)
print("compaction lowering assertions passed")
