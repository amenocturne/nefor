-- tests/lua/mag-kernel/routing_test.lua — unit tests for the mag-kernel
-- routing layer (plugins/mag/lua/mag-kernel/routing.lua, firing.lua, correlation.lua).
-- Driven from engine/tests/starter_mag_kernel_test.rs.
--
-- Covers: two stub actors exchange by id; a delivery to a registered-but-
-- unconstructed id constructs the instance lazily (ready first, then the
-- activation) and reuses it afterward; a dead-target send drops with a log
-- entry; a capability round-trip correlates through a stubbed bus; a
-- product-input actor fires once per complete sender-bound set with per-slot
-- FIFO ((Unit + Unit) needs one from each of two distinct upstreams); a
-- dependency-only edge delivers kernel-emitted mag.Unit on the upstream's
-- completion.

local inventory = require("inventory")
local Registry = require("registry")
local routing = require("routing")
local firing = require("firing")
local typed_value = require("typed-value")

-- ------------------------------------------------------------------
-- assert helpers
-- ------------------------------------------------------------------

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ", tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

local function new_logger()
  local rec = { info = {}, warn = {}, error = {} }
  local function sink(bucket)
    return function(m) bucket[#bucket + 1] = m end
  end
  return { info = sink(rec.info), warn = sink(rec.warn), error = sink(rec.error) }, rec
end

-- Stand up an inventory + registry + router wired the way init.lua does, with
-- a capturing logger and a capturing bus. `factories` is a map of name ->
-- declaration; a matching construct is not needed here because the tests bind
-- instances directly (the factory-construction layer is a sibling's task).
local function harness(factories)
  local log, rec = new_logger()
  local bus = {}
  local events = {}
  local inv = inventory.new({ log = log })
  local reg = Registry.new()
  for _, factory in pairs(factories) do
    local entry = factory.declaration and factory or {
      declaration = factory,
      construct = function(id) return { id = id } end,
    }
    local _, err = reg:register(entry)
    assert_true(err == nil, "factory registers: " .. tostring(err))
  end
  local seq = 0
  local router = routing.new({
    inventory = inv,
    registry = reg,
    log = log,
    bus_emit = function(env) bus[#bus + 1] = env end,
    events = function(e) events[#events + 1] = e end,
    gen_id = function() seq = seq + 1; return "req-" .. seq end,
  })
  -- Mirror init.lua: hand the dying instance its final kill message
  -- (dispatch_kill) BEFORE dropping routing state (forget) — emit-before-forget.
  inv.set_on_kill(function(id)
    router:dispatch_kill(id)
    router:forget(id)
  end)
  return { inv = inv, reg = reg, router = router, log = rec, bus = bus, events = events }
end

-- Build a test actor instance implementing the kernel<->instance contract:
-- an id-signed emitter (from the router) and a deliver(activation) whose body
-- is supplied per test. Emitting mag.ready confirms readiness.
local function spawn_actor(h, id, factory, routes, deliver_fn)
  local res = h.inv.apply({
    actors = { { id = id, factory = factory, params = {}, routes = routes or {} } },
  })
  assert_true(res.ok, "spawn " .. id .. ": " .. tostring(res.error))
  local inst = { id = id, emit = h.router:emitter(id), received = {} }
  inst.deliver = function(activation) return deliver_fn(inst, activation) end
  h.router:bind(id, inst)
  return inst
end

local function ready(h, inst)
  inst.emit({ kind = "mag.ready", from = inst.id })
end

local function count(t) local n = 0; for _ in pairs(t) do n = n + 1 end; return n end

-- ==================================================================
-- firing machine (pure) — single/union per-message, product per set
-- ==================================================================

do
  local single = firing.build("A.T")
  assert_eq(#single:offer("s", "A.T", { n = 1 }), 1, "single fires per matching message")
  assert_eq(#single:offer("s", "A.T", { n = 2 }), 1, "single fires again on the next message")
  assert_eq(#single:offer("s", "Other", {}), 0, "single ignores a non-matching tag")

  local union = firing.build({ "A.T", "B.T" })
  assert_eq(#union:offer("s", "A.T", {}), 1, "union fires on either variant (a)")
  assert_eq(#union:offer("s", "B.T", {}), 1, "union fires on either variant (b)")

  -- product with two sender-bound slots carrying the SAME type from two senders
  local prod = firing.build({ product = { "mag.Unit", "mag.Unit" } }, {
    { sender = "u1", type = "mag.Unit" },
    { sender = "u2", type = "mag.Unit" },
  })
  assert_eq(#prod:offer("u1", "mag.Unit", {}), 0, "product waits — only one slot filled")
  assert_eq(#prod:offer("u1", "mag.Unit", {}), 0, "second arrival from the same sender queues (per-slot FIFO), no set yet")
  local fired = prod:offer("u2", "mag.Unit", {})
  assert_eq(#fired, 1, "product fires once the other slot is filled — one from each")
  assert_eq(fired[1].shape, "product", "assembled activation is a product")
  -- one u1 message remains queued; a second u2 completes the leftover set
  assert_eq(#prod:offer("u2", "mag.Unit", {}), 1, "leftover u1 + a second u2 assembles a second set")
end

-- ==================================================================
-- compiled ordered product positions + whole-value isolation
-- ==================================================================

do
  local descriptor = { kind = "product", items = {
    { kind = "named", name = "test.A", arguments = {} },
    { kind = "named", name = "test.B", arguments = {} },
  } }
  local machine = firing.build({ product = { "in.Pair", "in.Pair" } }, {
    { edge_id = "edge-a", product_position = 0 },
    { edge_id = "edge-b", product_position = 1 },
  }, { input_type_id = "pair-id" })

  local function arrival(id, edge, type_id, position, value)
    local source = typed_value.factory({
      arrival_id = id,
      from = "producer",
      edge_id = "factory",
      type_id = type_id,
      type = descriptor,
      constructor_id = type_id,
      protocol_wire = "out.Value",
      product_position = -1,
      payload = { value = value },
    })
    return typed_value.routed(source, {
      actor = "pair",
      wire = "in.Pair",
      edge_id = edge,
      product_position = position,
    })
  end

  assert_eq(#machine:offer("producer", "in.Pair",
    arrival("a1", "edge-a", "a-id", 0, "A1")), 0,
    "first component queues")
  local whole = machine:offer("producer", "in.Pair",
    arrival("whole", "whole-edge", "pair-id", -1, "whole"))
  assert_eq(#whole, 1, "whole product fires immediately")
  assert_true(whole[1].whole, "whole activation is identified")
  local assembled = machine:offer("producer", "in.Pair",
    arrival("b3", "edge-b", "b-id", 1, "B3"))
  assert_eq(#assembled, 1, "queued components assemble independently of whole arrivals")
  assert_eq(assembled[1].messages[1].message.value, "A1", "position zero is stable")
  assert_eq(assembled[1].messages[2].message.value, "B3", "position one is stable")

  local first = arrival("equal-1", "edge-a", "a-id", 0, "same")
  local second = arrival("equal-2", "edge-a", "a-id", 0, "same")
  assert_true(first.arrival_id ~= second.arrival_id,
    "equal payloads on one edge remain distinct arrivals")
  assert_eq(#machine:offer("producer", "in.Pair", first), 0,
    "one edge cannot fill another product position")
  assert_eq(#machine:offer("producer", "in.Pair", second), 0,
    "a second emission on the same edge queues in the same occurrence")
end

-- ==================================================================
-- two stub actors exchange a message by id
-- ==================================================================

do
  local h = harness({
    producer = { name = "producer", inputs = { input = "start.Kick" }, outputs = { "hop.Ping" } },
    consumer = { name = "consumer", inputs = { input = "hop.Ping" }, outputs = {} },
  })

  local got = {}
  spawn_actor(h, "b", "consumer", {}, function(_, activation)
    got[#got + 1] = activation.messages[1]
    return "ok"
  end)
  local a = spawn_actor(h, "a", "producer", { ["hop.Ping"] = { { actor = "b", wire = "hop.Ping" } } }, function(self, activation)
    -- On its own activation, emit an id-signed output that routes to b.
    self.emit({ kind = "hop.Ping", from = self.id, payload = "hello-from-a" })
    return "ok"
  end)

  a.emit({ kind = "mag.ready", from = "a" })
  h.router:on_ready("b")

  -- Activate a with a graph input; a emits hop.Ping; router routes to b by id.
  h.router:fire("a", "seed", "start.Kick", { task = "go" })

  assert_eq(#got, 1, "b received exactly one routed message")
  assert_eq(got[1].from, "a", "message is signed with the sender id")
  assert_eq(got[1].tag, "hop.Ping", "message carries the routed output type")
  assert_eq(got[1].message.payload, "hello-from-a", "payload delivered intact")
end

-- ==================================================================
-- lazy construction: a delivery to a registered-but-unconstructed id
-- constructs the instance (ready first), fires it immediately, and reuses
-- the instance on later deliveries
-- ==================================================================

do
  local h = harness({
    sink = { name = "sink", inputs = { input = "q.Item" }, outputs = {} },
  })

  local order = {}
  -- Register the spec through the fold, but bind nothing: the router's
  -- construct hook (init.lua's seam) builds the instance on demand.
  local res = h.inv.apply({ actors = { { id = "b", factory = "sink", params = {}, routes = {},
    evidence={version=2,identity="nefor.factory.sink",arguments={{kind="named",name="test.Answer",arguments={}}},input={kind="named",name="test.Answer",arguments={}},output={kind="primitive",name="Unit"}},
    input={type={kind="named",name="test.Answer",arguments={}},wire="generic-provider.FinalAnswer"},outputs={{type={kind="primitive",name="Unit"},wire="mag.Unit"}} } } })
  assert_true(res.ok, "spawn b: " .. tostring(res.error))
  h.router:set_construct(function(record)
    order[#order + 1] = "construct:" .. record.id
    local inst = { id = record.id, emit = h.router:emitter(record.id) }
    inst.deliver = function(activation)
      order[#order + 1] = "deliver:" .. tostring(activation.messages[1].message.n)
      return "ok"
    end
    inst.emit({ kind = "mag.ready", from = record.id })
    return inst
  end)

  assert_eq(#order, 0, "registration alone constructs nothing")
  h.router:deliver("b", "a", "q.Item", { n = 1 })
  assert_eq(order[1], "construct:b", "the first delivery constructs the instance")
  assert_eq(order[2], "deliver:1", "the first activation lands right after construct")

  h.router:deliver("b", "a", "q.Item", { n = 2 })
  assert_eq(#order, 3, "a later delivery reuses the instance")
  assert_eq(order[3], "deliver:2", "no re-construct on the second activation")

  local readied = 0
  for _, e in ipairs(h.events) do
    if e.kind == "mag.actor_ready" and e.id == "b" then readied = readied + 1 end
  end
  assert_eq(readied, 1, "construction surfaced exactly one mag.actor_ready")
end

-- ==================================================================
-- a dead-target send drops with a log entry (routing, not the fold)
-- ==================================================================

do
  local h = harness({
    producer = { name = "producer", inputs = { input = "start.Kick" }, outputs = { "hop.Ping" } },
    consumer = { name = "consumer", inputs = { input = "hop.Ping" }, outputs = {} },
  })

  spawn_actor(h, "b", "consumer", {}, function() return "ok" end)
  local a = spawn_actor(h, "a", "producer", { ["hop.Ping"] = { { actor = "b", wire = "hop.Ping" } } }, function(self)
    self.emit({ kind = "hop.Ping", from = self.id, payload = "x" })
    return "ok"
  end)
  a.emit({ kind = "mag.ready", from = "a" })
  h.router:on_ready("b")

  h.inv.apply({ kills = { "b" } }) -- b dies; router:forget runs via on_kill
  h.router:fire("a", "seed", "start.Kick", {})

  local dropped = false
  for _, m in ipairs(h.log.info) do
    if m:find("send dropped", 1, true) and m:find("'b'", 1, true) then dropped = true end
  end
  assert_true(dropped, "routing a message to a dead target logs a drop")
end

-- ==================================================================
-- a capability round-trip correlates through a stubbed bus
-- ==================================================================

do
  local h = harness({
    worker = { name = "worker", inputs = { input = "job.Start" }, outputs = { "job.Done" } },
    sink   = { name = "sink",   inputs = { input = "job.Done" }, outputs = {} },
  })

  local final = {}
  spawn_actor(h, "out", "sink", {}, function(_, activation)
    final[#final + 1] = activation.messages[1].message
    return "ok"
  end)

  local w = spawn_actor(h, "w", "worker", { ["job.Done"] = { { actor = "out", wire = "job.Done" } } }, function(self, activation)
    if activation.kind == "reply" then
      -- Capability answered: emit the final output, then complete.
      self.emit({ kind = "job.Done", from = self.id, answer = activation.result.value })
      return "ok"
    end
    -- Initial activation: ask the capability plugin and defer completion.
    self.emit({ kind = "capability.invoke", from = self.id, capability = "compute", request = { x = 41 }, ref = "r1" })
    return nil -- pending; completion arrives with the reply
  end)
  w.emit({ kind = "mag.ready", from = "w" })
  h.router:on_ready("out")

  h.router:fire("w", "seed", "job.Start", { x = 41 })

  assert_eq(#h.bus, 1, "one tool.invoke put on the bus")
  local env = h.bus[1]
  assert_eq(env.kind, "tool.invoke", "bus envelope is tool.invoke-shaped")
  assert_eq(env.name, "compute", "envelope names the capability")
  assert_eq(env.args.x, 41, "envelope carries the request args")
  assert_true(type(env.id) == "string", "kernel minted a tracked request id")
  assert_eq(env.from, "w", "envelope is stamped with the emitting actor's address")
  assert_eq(#final, 0, "no final output before the reply arrives")

  -- Host delivers the correlated response; router routes it back to w.
  h.router:bus_response({ id = env.id, result = { value = 42 } })

  assert_eq(#final, 1, "final output produced after the reply routes back")
  assert_eq(final[1].answer, 42, "worker's final output carries the capability result")
  assert_eq(count(h.router.correlation.pending), 0, "correlation entry cleared after the reply")

  -- A stray reply for an unknown id is ignored (not ours).
  h.router:bus_response({ id = "no-such", result = {} })
  assert_eq(#final, 1, "an unknown-id reply changes nothing")
end

-- ==================================================================
-- product-input actor: one activation per complete sender-bound set,
-- per-slot FIFO — (Unit + Unit) from two distinct upstreams needs one
-- from EACH; a second arrival from the same sender queues.
-- ==================================================================

do
  local h = harness({
    upstream = { name = "upstream", inputs = { input = "go.Kick" }, outputs = { "mag.Unit" } },
    joiner   = { name = "joiner",   inputs = { joined = { product = { "mag.Unit", "mag.Unit" } } }, outputs = {} },
  })

  local fires = {}
  spawn_actor(h, "j", "joiner", {}, function(_, activation)
    fires[#fires + 1] = activation
    return "ok"
  end)
  -- Two distinct upstreams, each routing mag.Unit into the same product input.
  local u1 = spawn_actor(h, "u1", "upstream", { ["mag.Unit"] = { { actor = "j", wire = "mag.Unit" } } }, function() return "ok" end)
  local u2 = spawn_actor(h, "u2", "upstream", { ["mag.Unit"] = { { actor = "j", wire = "mag.Unit" } } }, function() return "ok" end)
  h.router:on_ready("j")
  u1.emit({ kind = "mag.ready", from = "u1" })
  u2.emit({ kind = "mag.ready", from = "u2" })

  -- Two completions from u1 alone: per-slot FIFO — the second queues, no set.
  h.router:deliver("j", "u1", "mag.Unit", { seq = "u1-a" })
  h.router:deliver("j", "u1", "mag.Unit", { seq = "u1-b" })
  assert_eq(#fires, 0, "(Unit + Unit) does not fire from one sender twice — needs one from each")

  -- First completion from u2 completes one set (u1-a + u2-a).
  h.router:deliver("j", "u2", "mag.Unit", { seq = "u2-a" })
  assert_eq(#fires, 1, "one complete sender-bound set fires exactly once")
  assert_eq(fires[1].shape, "product", "the assembled activation is a product")
  assert_eq(#fires[1].messages, 2, "the set carries one message per slot")

  -- A second u2 completes the leftover set (u1-b + u2-b).
  h.router:deliver("j", "u2", "mag.Unit", { seq = "u2-b" })
  assert_eq(#fires, 2, "the leftover u1 message assembles a second set with a new u2")
end

-- ==================================================================
-- a dependency-only edge delivers kernel-emitted mag.Unit on completion
-- (the factory never returns mag.Unit — the kernel synthesizes it)
-- ==================================================================

do
  local h = harness({
    task = { name = "task", inputs = { input = "do.Work" }, outputs = { "work.Result" } },
    dep  = { name = "dep",  inputs = { input = "mag.Unit" }, outputs = {} },
  })

  local dep_fired = {}
  spawn_actor(h, "d", "dep", {}, function(_, activation)
    dep_fired[#dep_fired + 1] = activation.messages[1]
    return "ok"
  end)
  -- The dependency edge is a mag.Unit route on the upstream — a route key that
  -- is NOT a declared factory output; the kernel emits it on completion.
  local t = spawn_actor(h, "t", "task", { ["mag.Unit"] = { { actor = "d", wire = "mag.Unit" } } }, function(self)
    -- The factory does its work and returns a normal completion. It never
    -- emits mag.Unit and does not know a dependency edge exists.
    return "ok"
  end)
  t.emit({ kind = "mag.ready", from = "t" })
  h.router:on_ready("d")

  h.router:fire("t", "seed", "do.Work", {})

  assert_eq(#dep_fired, 1, "the dependency actor fired on the upstream's completion")
  assert_eq(dep_fired[1].tag, "mag.Unit", "it received kernel-emitted mag.Unit")
  assert_eq(dep_fired[1].from, "t", "the mag.Unit is signed with the completing actor's id")
end

-- ==================================================================
-- kill consumes and cancels only the dying requester's correlations before
-- local actor removal; late replies cannot settle or re-fire it.
-- ==================================================================

do
  local h = harness({
    worker = { name = "worker", inputs = { input = "job.Start" }, outputs = { "job.Done" } },
  })
  local replies = 0
  local w = spawn_actor(h, "w", "worker", {}, function(self, activation)
    if activation.kind == "reply" then replies = replies + 1; return "ok" end
    self.emit({ kind = "capability.invoke", capability = "provider", request = {}, ref = "actor@r1" })
    self.emit({ kind = "capability.invoke", capability = "tool", request = {}, ref = "actor@r2" })
    return nil
  end)
  local other = spawn_actor(h, "other", "worker", {}, function() return nil end)
  w.emit({ kind = "mag.ready" })
  other.emit({ kind = "mag.ready" })
  h.router:fire("w", "seed", "job.Start", {})
  other.emit({ kind = "capability.invoke", capability = "unrelated", request = {}, ref = "other@r1" })

  assert_eq(h.router.correlation:request_id("w", "actor@r1"), "req-1",
    "requester plus opaque ref resolves the generated id")
  assert_eq(h.router.correlation:request_id("other", "actor@r1"), nil,
    "opaque refs are requester-scoped")

  local observed = {}
  function w.handle_kill()
    observed.bound = h.router.instances.w ~= nil
    observed.owned = h.router.correlation:request_id("w", "actor@r1")
  end
  h.inv.apply({ kills = { "w" } })

  assert_eq(#h.bus, 5, "three invokes plus exactly two cancels reached the bus")
  assert_eq(h.bus[4].kind, "tool.cancel", "first owned request uses generic cancellation")
  assert_eq(h.bus[4].id, "req-1", "first generated request id is cancelled")
  assert_eq(h.bus[5].kind, "tool.cancel", "second owned request uses generic cancellation")
  assert_eq(h.bus[5].id, "req-2", "second generated request id is cancelled")
  assert_true(observed.bound, "cancellation precedes local actor removal")
  assert_eq(observed.owned, nil, "ownership is consumed before local kill cleanup")
  assert_eq(count(h.router.correlation.pending), 1, "unrelated requester correlation remains open")
  assert_eq(h.router.instances.w, nil, "the dying instance is removed after cancellation")
  assert_eq(h.router.machines.w, nil, "kill drops firing slots")
  assert_eq(h.router.ready.w, nil, "kill drops readiness")

  assert_eq(h.router:bus_response({ id = "req-1", result = { value = "late" } }), false,
    "a late response cannot claim consumed ownership")
  assert_eq(replies, 0, "a late response cannot re-fire the dead requester")
  h.inv.apply({ kills = { "w" } })
  assert_eq(#h.bus, 5, "duplicate kill emits no duplicate cancellation")
end

-- ==================================================================
-- drain(id): calls handle_drain (graceful path); kill never auto-drains
-- ==================================================================

do
  local h = harness({ worker = { name = "worker", inputs = { input = "job.Start" }, outputs = {} } })

  local drained = { count = 0 }
  local inst = { id = "d", emit = h.router:emitter("d") }
  inst.deliver = function() return "ok" end
  function inst.handle_drain()
    drained.count = drained.count + 1
    inst.emit({ kind = "mag.complete", from = "d" })
  end
  h.inv.apply({ actors = { { id = "d", factory = "worker", params = {}, routes = {} } } })
  h.router:bind("d", inst)
  inst.emit({ kind = "mag.ready", from = "d" })

  assert_eq(h.router:drain("d"), true, "drain runs the handler where declared")
  assert_eq(drained.count, 1, "handle_drain was called exactly once")

  -- drain on an id with no handler (or unknown) is a no-op returning false.
  assert_eq(h.router:drain("nonexistent"), false, "drain on an unknown id returns false")

  -- kill does NOT auto-invoke handle_drain (drain is a distinct op).
  local flags = { drained = false }
  local i2 = { id = "z", emit = h.router:emitter("z") }
  i2.deliver = function() return "ok" end
  function i2.handle_drain() flags.drained = true end
  h.inv.apply({ actors = { { id = "z", factory = "worker", params = {}, routes = {} } } })
  h.router:bind("z", i2)
  i2.emit({ kind = "mag.ready", from = "z" })
  h.inv.apply({ kills = { "z" } })
  assert_true(not flags.drained, "kill does not auto-invoke handle_drain")
end

-- ==================================================================
-- failed completions: routed failure routes; unrouted failure escalates to a
-- run-failed event (mag.run_failed) instead of vanishing
-- ==================================================================

do
  local h = harness({
    worker = { name = "worker", inputs = { input = "start.Kick" }, outputs = {} },
    catcher = { name = "catcher", inputs = { input = "mag.Failed" }, outputs = {} },
  })

  -- No route for the failure tag → the delivery layer escalates: the run would
  -- otherwise strand (nothing downstream ever fires, the sink never completes).
  local w = spawn_actor(h, "w", "worker", {}, function()
    return { status = "failed", failure = "mag.Failed", value = { error = "provider blew up" } }
  end)
  ready(h, w)
  h.router:deliver("w", "test", "start.Kick", { kind = "start.Kick" })

  local run_failed
  for _, e in ipairs(h.events) do
    if e.kind == "mag.run_failed" then run_failed = e end
  end
  assert_true(run_failed ~= nil, "an unrouted failure escalates to mag.run_failed")
  assert_eq(run_failed.from, "w", "the event names the failing actor")
  assert_eq(run_failed.failure, "mag.Failed", "the event carries the failure tag")
  assert_eq(run_failed.error, "provider blew up", "the event surfaces the failure detail")

  -- A failure without an error string still produces a readable detail.
  local w0 = spawn_actor(h, "w0", "worker", {}, function()
    return { status = "failed", failure = "mag.Failed" }
  end)
  ready(h, w0)
  h.router:deliver("w0", "test", "start.Kick", { kind = "start.Kick" })
  local last
  for _, e in ipairs(h.events) do
    if e.kind == "mag.run_failed" and e.from == "w0" then last = e end
  end
  assert_true(last ~= nil and type(last.error) == "string" and #last.error > 0,
    "a detail-less failure still carries a readable error")

  -- A ROUTED failure is composed failure handling: it routes, no escalation.
  local caught = {}
  local c = spawn_actor(h, "c", "catcher", {}, function(_, activation)
    caught[#caught + 1] = activation.messages[1]
    return "ok"
  end)
  ready(h, c)
  local w2 = spawn_actor(h, "w2", "worker", { ["mag.Failed"] = { { actor = "c", wire = "mag.Failed" } } }, function()
    return { status = "failed", failure = "mag.Failed", value = { error = "handled" } }
  end)
  ready(h, w2)
  h.router:deliver("w2", "test", "start.Kick", { kind = "start.Kick" })
  assert_eq(#caught, 1, "a routed failure delivers the failure-typed output")
  assert_eq(caught[1].message.value.error, "handled", "the routed failure carries its value")
  for _, e in ipairs(h.events) do
    assert_true(not (e.kind == "mag.run_failed" and e.from == "w2"),
      "a routed failure does not escalate to run-failed")
  end
end

-- ==================================================================
-- Semantic routing keeps provider-native turn messages while dropping
-- unrelated transport fields from the canonical payload.
-- ==================================================================

do
  local prior_semantic_type = nefor.semantic_type
  local provider_input = {
    kind = "named", name = "nefor.contracts.ProviderInput", arguments = {},
  }
  nefor.semantic_type = {
    accepts = function() return true end,
    id = function(value) return value.name or value.kind end,
    validate_value = function(_, value)
      return type(value) == "table" and value.content ~= nil
        and { ok = true }
        or { ok = false, violations = { { path = "$.content", message = "required" } } }
    end,
  }

  local producer_decl = {
    name = "provider-input-producer",
    inputs = { input = "test.Start" },
    outputs = { "generic-provider.ProviderOut" },
  }
  local consumer_decl = {
    name = "provider-input-consumer",
    inputs = { input = "generic-provider.ProviderOut" },
    outputs = {},
  }
  local h = harness({ producer = producer_decl, consumer = consumer_decl })
  local received = {}

  local consumer_result = h.inv.apply({ actors = { {
    id = "provider-consumer",
    factory = consumer_decl.name,
    params = {},
    semantic_strict = true,
    input = {
      wire = "generic-provider.ProviderOut",
      type_id = "nefor.contracts.ProviderInput",
      type = provider_input,
    },
    outputs = {},
    routes = {},
  } } })
  assert_true(consumer_result.ok, "typed provider consumer registers")
  local consumer = { id = "provider-consumer", emit = h.router:emitter("provider-consumer") }
  consumer.deliver = function(activation)
    received[#received + 1] = activation.messages[1].message
    return "ok"
  end
  h.router:bind("provider-consumer", consumer)
  ready(h, consumer)

  local producer_result = h.inv.apply({ actors = { {
    id = "provider-producer",
    factory = producer_decl.name,
    params = {},
    semantic_strict = true,
    input = { wire = "test.Start", type_id = "test.Start", type = provider_input },
    outputs = { {
      wire = "generic-provider.ProviderOut",
      type_id = "nefor.contracts.ProviderInput",
      type = provider_input,
    } },
    routes = {
      ["generic-provider.ProviderOut"] = { {
        actor = "provider-consumer",
        wire = "generic-provider.ProviderOut",
      } },
    },
  } } })
  assert_true(producer_result.ok, "typed provider producer registers")
  local producer = { id = "provider-producer", emit = h.router:emitter("provider-producer") }
  producer.deliver = function() return "ok" end
  h.router:bind("provider-producer", producer)
  ready(h, producer)

  local turn_messages = { {
    role = "user",
    content = { value = { prompt = "inspect the repository" } },
  } }
  local tool_calls = { {
    id = "call-1", name = "read_file", args = { path = "README.md" },
  } }
  producer.emit({
    kind = "generic-provider.ProviderOut",
    value = { content = { prompt = "inspect the repository" } },
    messages = turn_messages,
    calls = tool_calls,
    transcript_delta = { { role = "assistant", content = "private transcript" } },
    result = { raw = "private provider response" },
  })

  assert_eq(#received, 1, "provider input routes once")
  assert_true(rawequal(received[1].messages, turn_messages),
    "provider-native turn messages survive semantic routing")
  assert_true(rawequal(received[1].calls, tool_calls),
    "tool-call correlation handles survive semantic routing")
  assert_eq(received[1].transcript_delta, nil,
    "transcript metadata stays off the routed provider input")
  assert_eq(received[1].result, nil,
    "raw provider responses stay off the routed provider input")

  nefor.semantic_type = prior_semantic_type
end

-- ==================================================================
-- RetryGate routes canonical branch records while preserving the original
-- transport payload, then latches closed after the exhausted value.
-- ==================================================================

do
  local retry_gate = require("factories.retry-gate")
  local prior_semantic_type = nefor.semantic_type
  local answer = { kind = "named", name = "test.Answer", arguments = {} }
  local function branch(name)
    return { kind = "named", name = name, arguments = { answer } }
  end
  local continue_type = branch("nefor.contracts.Continue")
  local exhausted_type = branch("nefor.contracts.Exhausted")
  local output_type = { kind = "union", items = { continue_type, exhausted_type } }

  nefor.semantic_type = {
    accepts = function() return true end,
    id = function(value) return value.name or value.kind end,
    validate_value = function(expected, value)
      local valid = type(value) == "table" and value.value ~= nil
        and (expected.name == "nefor.contracts.Continue"
          or expected.name == "nefor.contracts.Exhausted")
      return valid and { ok = true } or {
        ok = false,
        violations = { { path = "$", message = "expected branch record {value:T}" } },
      }
    end,
  }

  local function exercise(maximum)
    local diagnostics = {}
    local sink_decl = {
      name = "retry-sink-" .. maximum,
      inputs = { input = { "nefor.retry.Continue", "nefor.retry.Exhausted" } },
      outputs = {},
    }
    local h = harness({
      retry = retry_gate,
      sink = sink_decl,
    })
    local received = {}
    local sink_id = "sink-" .. maximum
    local gate_id = "gate-" .. maximum
    local sink_result = h.inv.apply({ actors = { {
      id = sink_id,
      factory = sink_decl.name,
      params = {},
      semantic_strict = true,
      input = { wire = "retry.Branch", type_id = "test.Branch", type = output_type },
      outputs = {},
      routes = {},
    } } })
    assert_true(sink_result.ok, "typed retry sink registers: " .. tostring(sink_result.error))
    local sink = { id = sink_id, emit = h.router:emitter(sink_id) }
    sink.deliver = function(activation)
      received[#received + 1] = activation.messages[1]
      return "ok"
    end
    h.router:bind(sink_id, sink)
    ready(h, sink)

    local result = h.inv.apply({ actors = { {
      id = gate_id,
      factory = "retry-gate",
      params = { max_retries = maximum },
      semantic_strict = true,
      evidence = {
        version = 2,
        identity = "nefor.factory.retry-gate",
        arguments = { answer },
        input = answer,
        output = output_type,
      },
      input = { wire = "nefor.retry.Input", type_id = "test.Answer", type = answer },
      outputs = {
        { wire = "nefor.retry.Continue", type_id = "test.Continue", type = continue_type },
        { wire = "nefor.retry.Exhausted", type_id = "test.Exhausted", type = exhausted_type },
      },
      routes = {
        ["nefor.retry.Continue"] = { { actor = sink_id, wire = "nefor.retry.Continue" } },
        ["nefor.retry.Exhausted"] = { { actor = sink_id, wire = "nefor.retry.Exhausted" } },
      },
    } } })
    assert_true(result.ok, "typed retry gate registers: " .. tostring(result.error))
    h.router:set_construct(function(record)
      return h.reg:construct(record.factory, record.id, record.params,
        h.router:emitter(record.id), {
          diagnostic = function(value) diagnostics[#diagnostics + 1] = value end,
        })
    end)
    return h, gate_id, received, diagnostics
  end

  for _, maximum in ipairs({ 0, 1, 3 }) do
    local h, gate_id, received, diagnostics = exercise(maximum)
    local payloads = {}
    for index = 1, maximum + 1 do
      payloads[index] = { maximum = maximum, index = index }
      h.router:deliver(gate_id, "source", "nefor.retry.Input", { value = payloads[index] })
    end
    assert_eq(#received, maximum + 1, "max " .. maximum .. " emits one branch per accepted value")
    for index = 1, maximum do
      assert_eq(received[index].tag, "nefor.retry.Continue", "retry-budget values continue")
      assert_true(rawequal(received[index].message.value, payloads[index]),
        "continue transport preserves exact payload identity")
      assert_true(rawequal(received[index].message.semantic_value.value, payloads[index]),
        "continue semantic record contains the original value")
    end
    local exhausted = received[maximum + 1]
    assert_eq(exhausted.tag, "nefor.retry.Exhausted", "the value after the retry budget exhausts")
    assert_true(rawequal(exhausted.message.value, payloads[maximum + 1]),
      "exhausted transport preserves exact payload identity")
    assert_true(rawequal(exhausted.message.semantic_value.value, payloads[maximum + 1]),
      "exhausted semantic record contains the original value")
    assert_eq(#h.log.error, 0, "canonical branch records pass semantic-strict routing")

    local late = { maximum = maximum, late = true }
    h.router:deliver(gate_id, "source", "nefor.retry.Input", { value = late })
    assert_eq(#received, maximum + 1, "a latched gate emits no branch for late input")
    assert_eq(#diagnostics, 1, "a latched gate diagnoses late input")
    assert_eq(diagnostics[1].kind, "late_input_after_exhaustion", "late diagnostic is specific")
  end

  nefor.semantic_type = prior_semantic_type
end

print("mag-kernel routing_test: all cases passed")
