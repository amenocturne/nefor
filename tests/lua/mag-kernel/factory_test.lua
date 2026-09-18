-- Factory registry contracts remain actor-local. Structural route validation is
-- owned by topology.lua and is exercised here through typed endpoint ports.

local shape = require("shape")
local Registry = require("registry")
local Topology = require("topology")
local stub = require("factories.stub")

local function assert_eq(actual, expected, message)
  if actual ~= expected then
    error(string.format("assertion failed: %s\n  expected: %s\n  actual:   %s",
      message or "values differ", tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(condition, message)
  if not condition then error("assertion failed: " .. (message or "(no message)"), 2) end
end

local function capture()
  local out = {}
  return out, function(message) out[#out + 1] = message end
end

local function find_kind(messages, kind)
  for _, message in ipairs(messages) do
    if message.kind == kind then return message end
  end
end

-- Shape remains the legacy actor activation contract used inside factories.
assert_eq(shape.classify("stub.In"), "single", "string is a single shape")
assert_eq(shape.classify({"A", "B"}), "union", "list is a union shape")
assert_eq(shape.classify({product={"A", "B"}}), "product", "product table is a product shape")
assert_eq(shape.firing("stub.In"), "per-message", "single fires per message")
assert_eq(shape.firing({"A", "B"}), "any", "union fires on any")
assert_eq(shape.firing({product={"A", "B"}}), "all", "product fires on all")
local malformed, shape_error = shape.classify({})
assert_true(malformed == nil and type(shape_error) == "string", "empty shape rejects")

-- Registry owns declaration uniqueness, lookup, and actor construction only.
do
  local registry = Registry.new()
  local declaration, err = registry:register({declaration=stub.declaration, construct=stub.construct})
  assert_true(declaration ~= nil and err == nil, "stub registers")
  assert_true(registry:lookup("stub") ~= nil, "stub lookup succeeds")
  assert_eq(registry:declaration("stub").outputs[1], "stub.Out", "declaration is exposed")
  assert_eq(registry:declared_input("stub", "input"), "stub.In", "input is exposed")

  local messages, emit = capture()
  local instance, construct_error = registry:construct("stub", "docs.stub", {greeting="hi"}, emit)
  assert_true(instance ~= nil and construct_error == nil, "stub constructs")
  assert_eq(find_kind(messages, "mag.ready").from, "docs.stub", "ready is actor-signed")
  local completion = instance.deliver({shape="single", messages={{from="upstream",tag="stub.In",message="payload"}}})
  assert_eq(completion.status, "ok", "stub completes synchronously")
  local output = find_kind(messages, "stub.Out")
  assert_eq(output.from, "docs.stub", "output is actor-signed")
  assert_eq(output.payload, "payload", "output carries activation")
  assert_eq(output.greeting, "hi", "output carries params")

  local unknown, unknown_error = registry:construct("missing", "x", {}, function() end)
  assert_true(unknown == nil and unknown_error:match("unknown factory"), "unknown factory rejects")
  local duplicate, duplicate_error = registry:register({declaration=stub.declaration,construct=stub.construct})
  assert_true(duplicate == nil and duplicate_error:match("already registered"), "duplicate factory rejects")
end

-- Topology, not Registry, validates that a route names a declared output port.
do
  local string_type = {kind="primitive",name="String"}
  local string_id = nefor.semantic_type.id(string_type)
  local function endpoint(id) return {constructor="ActorEndpoint",value={id=id}} end
  local function port(id, wire, descriptor)
    descriptor = descriptor or string_type
    return {endpoint=endpoint(id),type=descriptor,type_id=nefor.semantic_type.id(descriptor),wire=wire}
  end
  local function actor(id)
    return {id=id,factory="stub",type_arguments={},params={},
      input=port(id,"stub.In"),outputs={port(id,"stub.Out")}}
  end
  local topology = Topology.new({
    inventory={pairs=function() return pairs({}) end},
    semantic=nefor.semantic_type,
    dispatch=function() end,
    observe=function() return true end,
  })
  local source, destination = actor("source"), actor("destination")
  local spoofed = port("source", "stub.Nope")
  local state, err = topology:preflight({actors={source,destination},junctions={},messages={},nodes={},kills={},routes={{
    id="bad-route",from=spoofed,to=destination.input,product_position=-1,
  }}})
  assert_true(state == nil and err:match("source port is not declared"),
    "topology rejects a route from an undeclared output")

  state, err = topology:preflight({actors={source,destination},junctions={},messages={},nodes={},kills={},routes={{
    id="good-route",from=source.outputs[1],to=destination.input,product_position=-1,
  }}})
  assert_true(state ~= nil and err == nil, "topology accepts declared compatible ports")
  assert_eq(source.outputs[1].type_id, string_id, "fixture carries semantic identity")
end

print("mag-kernel factory_test: all assertions passed")
