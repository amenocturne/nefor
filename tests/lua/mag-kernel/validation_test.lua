-- Apply-time graph wiring belongs to topology.lua. This fixture protects the
-- complete prospective-topology validation used by initial programs and deltas.

local Topology = require("topology")

local function assert_true(condition, message)
  if not condition then error("assertion failed: " .. (message or "(no message)"), 2) end
end

local function assert_contains(value, fragment, message)
  if type(value) ~= "string" or not value:find(fragment, 1, true) then
    error(string.format("assertion failed: %s\n  expected to contain: %s\n  actual: %s",
      message or "substring missing", tostring(fragment), tostring(value)), 2)
  end
end

local string_type = {kind="primitive",name="String"}
local int_type = {kind="primitive",name="Int"}

local function endpoint(id)
  return {constructor="ActorEndpoint",value={id=id}}
end

local function port(id, wire, descriptor)
  return {
    endpoint=endpoint(id),
    type=descriptor,
    type_id=nefor.semantic_type.id(descriptor),
    wire=wire,
  }
end

local function actor(id, input_type, output_type)
  return {
    id=id,
    factory="stub",
    type_arguments={},
    params={},
    input=port(id, "in", input_type),
    outputs={port(id, "out", output_type)},
  }
end

local function topology()
  return Topology.new({
    inventory={pairs=function() return pairs({}) end},
    semantic=nefor.semantic_type,
    dispatch=function() end,
    settle_result=function() end,
  })
end

local function modification(actors, routes)
  return {actors=actors,routes=routes,messages={},nodes={},kills={}}
end

-- A compatible route is accepted as one whole typed edge.
do
  local source = actor("source", string_type, string_type)
  local destination = actor("destination", string_type, string_type)
  local state, err = topology():preflight(modification({source,destination}, {{
    id="source-destination",from=source.outputs[1],to=destination.input,transforms={},
  }}))
  assert_true(state ~= nil and err == nil, "compatible typed route passes")
end

-- A source port must be the exact port declared by its endpoint owner.
do
  local source = actor("source", string_type, string_type)
  local destination = actor("destination", string_type, string_type)
  local spoofed = port("source", "not-an-output", string_type)
  local state, err = topology():preflight(modification({source,destination}, {{
    id="spoofed-source",from=spoofed,to=destination.input,transforms={},
  }}))
  assert_true(state == nil, "undeclared source rejects")
  assert_contains(err, "source is not a declared actor output", "error names the source contract")
end

-- Destination acceptance is semantic rather than legacy tag/registry matching.
do
  local source = actor("source", string_type, string_type)
  local destination = actor("destination", int_type, int_type)
  local state, err = topology():preflight(modification({source,destination}, {{
    id="incompatible",from=source.outputs[1],to=destination.input,transforms={},
  }}))
  assert_true(state == nil, "incompatible semantic types reject")
  assert_contains(err, "transformed type is incompatible", "error names semantic incompatibility")
end

-- Routes are checked against the post-apply graph: existing live actors are
-- resolvable, unknown actors reject, and conflicting redefinitions stay atomic.
do
  local live = actor("live", string_type, string_type)
  local inventory = {
    pairs=function()
      return pairs({live={id=live.id,state="alive",factory=live.factory,
        type_arguments=live.type_arguments,params=live.params,input=live.input,outputs=live.outputs}})
    end,
  }
  local graph = Topology.new({inventory=inventory,semantic=nefor.semantic_type,
    dispatch=function() end,settle_result=function() end})
  local source = actor("source", string_type, string_type)
  local state, err = graph:preflight(modification({source}, {{
    id="to-live",from=source.outputs[1],to=live.input,transforms={},
  }}))
  assert_true(state ~= nil and err == nil, "route may target an existing live actor")

  local ghost = port("ghost", "in", string_type)
  state, err = graph:preflight(modification({source}, {{
    id="to-ghost",from=source.outputs[1],to=ghost,transforms={},
  }}))
  assert_true(state == nil, "unknown endpoint rejects")
  assert_contains(err, "references unknown actor ghost", "error names unknown endpoint")
  assert_true(next(graph.assemblies) == nil and #graph.routes == 0,
    "failed preflight installs no partial topology")
end

print("mag-kernel validation_test: all assertions passed")
