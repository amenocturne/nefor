-- Human approval remains an actor capability. ADT projection is now a
-- structural AdtUnpack junction (covered by topology_lua.rs), so this fixture
-- focuses on the gate's control-plane request/reply/cancel lifecycle.

local inventory = require("inventory")
local Registry = require("registry")
local routing = require("routing")
local observer = require("observer")
local human = require("factories.human")

local function assert_eq(actual, expected, message)
  if actual ~= expected then
    error(string.format("assertion failed: %s\n  expected: %s\n  actual: %s",
      message or "values differ", tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(condition, message)
  if not condition then error("assertion failed: " .. (message or "(no message)"), 2) end
end

local function assert_contains(value, fragment, message)
  if type(value) ~= "string" or not value:find(fragment, 1, true) then
    error(string.format("assertion failed: %s\n  expected to contain: %s\n  actual: %s",
      message or "substring missing", tostring(fragment), tostring(value)), 2)
  end
end

local function logger()
  local function noop() end
  return {info=noop,warn=noop,error=noop}
end

local function producer_factory()
  return {
    declaration={name="producer",params={},inputs={input="generic-provider.ProviderOut"},outputs={"generic-provider.TextAnswer"}},
    construct=function(id, params, emit)
      local instance={id=id}
      function instance.deliver()
        emit({kind="generic-provider.TextAnswer",from=id,text="draft"})
        return {status="ok"}
      end
      emit({kind="mag.ready",from=id})
      return instance
    end,
  }
end

local function collector_factory(decisions)
  return {
    declaration={name="collector",params={},inputs={decision="human.Decision"},outputs={}},
    construct=function(id, params, emit)
      local instance={id=id}
      function instance.deliver(activation)
        decisions[#decisions + 1] = activation.messages[1].message
        return {status="ok"}
      end
      emit({kind="mag.ready",from=id})
      return instance
    end,
  }
end

local function harness()
  local events, persisted, decisions = {}, {}, {}
  local registry = Registry.new()
  for _, factory in ipairs({
    {declaration=human.declaration,construct=human.construct},
    producer_factory(),
    collector_factory(decisions),
  }) do
    local _, err = registry:register(factory)
    assert_true(err == nil, "factory registers: " .. tostring(err))
  end

  local inv = inventory.new({log=logger(),registry=registry})
  local router = routing.new({
    inventory=inv,registry=registry,log=logger(),bus_emit=function() end,
    events=function(event) events[#events + 1] = event end,
    persist_output=function(id, output) persisted[#persisted + 1] = {id=id,output=output} end,
  })
  inv.set_on_kill(function(id) router:dispatch_kill(id); router:forget(id) end)
  inv.set_is_constructed(function(id) return router:is_constructed(id) end)
  router:set_construct(function(record)
    return registry:construct(record.factory,record.id,record.params,router:emitter(record.id),
      {writer=function() end})
  end)
  inv.set_deliver(function(to, from, content)
    content = content or {}
    router:deliver(to,from,content.kind,content)
  end)
  local obs = observer.new({inventory=inv,emit_event=function(event) events[#events + 1] = event end})
  return {inv=inv,router=router,obs=obs,events=events,persisted=persisted,decisions=decisions}
end

local function actors()
  local text_answer = {kind="named",name="nefor.contracts.TextAnswer",arguments={}}
  local approved = {kind="named",name="nefor.human.HumanWorkflowApproval",arguments={}}
  local rejected = {kind="named",name="nefor.human.HumanWorkflowRejection",arguments={}}
  local decision = {kind="adt",name="nefor.human.HumanWorkflowDecision",arguments={},constructors={
    {name="Approved",payload=approved},{name="Rejected",payload=rejected},
  }}
  return {
    {id="produce",factory="producer",type_arguments={},params={},
      input={type={kind="named",name="nefor.contracts.ProviderInput",arguments={}},wire="generic-provider.ProviderOut"},
      outputs={{type=text_answer,wire="generic-provider.TextAnswer"}},
      routes={["generic-provider.TextAnswer"]={{actor="approve",wire="generic-provider.TextAnswer"}}}},
    {id="approve",factory="human",type_arguments={decision},params={prompt="Approve the draft?"},
      input={type=text_answer,wire="generic-provider.TextAnswer"},
      outputs={{type=decision,wire="human.Decision"}},
      routes={["human.Decision"]={{actor="collect",wire="human.Decision"}}}},
    {id="collect",factory="collector",type_arguments={},params={},
      input={type=decision,wire="human.Decision"},outputs={},routes={}},
  }
end

local function seed()
  return {to="produce",content={kind="generic-provider.ProviderOut",messages={{role="user",content="write"}}}}
end

local function reply(fields)
  local content={kind="mag.ApprovalReply",content="",reason=""}
  for key, value in pairs(fields) do content[key] = value end
  return {to="approve",content=content}
end

local function of_kind(events, kind)
  local result={}
  for _, event in ipairs(events) do if event.kind == kind then result[#result + 1] = event end end
  return result
end

-- Subject emission raises one request; the reply bypasses declared actor ports
-- and the gate emits one nominal decision for structural projection downstream.
do
  local h = harness()
  local applied = h.obs:apply({actors=actors(),messages={seed()}})
  assert_true(applied.ok, "gate constellation applies: " .. tostring(applied.error))
  local requests = of_kind(h.events, "mag.approval_request")
  assert_eq(#requests, 1, "one request is raised")
  assert_eq(requests[1].from, "approve", "request names gate")
  assert_eq(requests[1].prompt, "Approve the draft?", "request carries prompt")
  assert_eq(requests[1].subject.text, "draft", "request carries subject")

  local answered = h.obs:apply({messages={reply({approved=true,content="ship it"})}})
  assert_true(answered.ok, "approval reply applies: " .. tostring(answered.error))
  assert_eq(#h.decisions, 1, "collector receives one decision")
  assert_eq(h.decisions[1].value.constructor, "Approved", "decision preserves ADT constructor")
  assert_eq(h.decisions[1].value.value.content, "ship it", "decision preserves approval content")
  local gate_outputs=0
  for _, record in ipairs(h.persisted) do
    if record.id == "approve" then
      gate_outputs = gate_outputs + 1
      assert_eq(record.output.kind, "human.Decision", "only typed decision is persisted")
    end
  end
  assert_eq(gate_outputs, 1, "control-plane request is not persisted as actor output")
end

-- A reply answers an outstanding request; before construction it rejects
-- atomically and does not falsely begin actor work.
do
  local h = harness()
  assert_true(h.obs:apply({actors=actors()}).ok, "actors register lazily")
  assert_eq(h.router:is_constructed("approve"), false, "gate is not constructed")
  local result = h.obs:apply({messages={reply({approved=true,content="premature"})}})
  assert_true(not result.ok, "premature reply rejects")
  assert_contains(result.error, "no outstanding approval request", "error names protocol")
  assert_eq(h.router:is_constructed("approve"), false, "rejection constructs nothing")
  assert_eq(#of_kind(h.events,"mag.modification_rejected"),1,"rejection is observed")
end

-- Killing or draining a live gate retracts its control-plane request.
do
  local h = harness()
  assert_true(h.obs:apply({actors=actors(),messages={seed()}}).ok, "request opens")
  assert_true(h.obs:apply({kills={"approve"}}).ok, "gate kill applies")
  local cancels=of_kind(h.events,"mag.approval_cancel")
  assert_eq(#cancels,1,"kill emits approval cancellation")
  assert_eq(cancels[1].correlation,"approve","cancellation preserves correlation")
  local late=h.obs:apply({messages={reply({approved=true,content="late"})}})
  assert_true(not late.ok,"reply after cancellation rejects")
  assert_contains(late.error,"no outstanding approval request","late reply names retracted request")

  local drained=harness()
  assert_true(drained.obs:apply({actors=actors(),messages={seed()}}).ok,"second request opens")
  assert_true(drained.router:drain("approve"),"drain handler runs")
  assert_eq(#of_kind(drained.events,"mag.approval_cancel"),1,"drain emits cancellation")
end

print("mag-kernel human_test: all assertions passed")
