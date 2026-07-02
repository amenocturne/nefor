-- tests/lua/mag-kernel/routing_test.lua — unit tests for the mag-kernel
-- routing layer (starter/mag-kernel/routing.lua, firing.lua, correlation.lua).
-- Driven from engine/tests/starter_mag_kernel_test.rs.
--
-- Covers the task's done-when list: two stub actors exchange by id; a send
-- between spawn-request and ready is delivered after ready in order; a
-- dead-target send drops with a log entry; a capability round-trip correlates
-- through a stubbed bus; a product-input actor fires once per complete
-- sender-bound set with per-slot FIFO ((Unit + Unit) needs one from each of
-- two distinct upstreams); a dependency-only edge delivers kernel-emitted
-- mag.Unit on the upstream's completion.

local inventory = require("inventory")
local Registry = require("registry")
local routing = require("routing")

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
  local inv = inventory.new({ log = log })
  local reg = Registry.new()
  for _, decl in pairs(factories) do
    local _, err = reg:register({ declaration = decl, construct = function(id) return { id = id } end })
    assert_true(err == nil, "factory registers: " .. tostring(err))
  end
  local seq = 0
  local router = routing.new({
    inventory = inv,
    registry = reg,
    log = log,
    bus_emit = function(env) bus[#bus + 1] = env end,
    gen_id = function() seq = seq + 1; return "req-" .. seq end,
  })
  inv.set_on_kill(function(id) router:forget(id) end)
  return { inv = inv, reg = reg, router = router, log = rec, bus = bus }
end

-- Build a test actor instance implementing the kernel<->instance contract:
-- an id-signed emitter (from the router) and a deliver(activation) whose body
-- is supplied per test. Emitting mag.ready confirms the ready barrier.
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
  local firing = require("firing")

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
  local a = spawn_actor(h, "a", "producer", { ["hop.Ping"] = { "b" } }, function(self, activation)
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
-- a send between spawn-request and ready is delivered after ready, in order
-- ==================================================================

do
  local h = harness({
    sink = { name = "sink", inputs = { input = "q.Item" }, outputs = {} },
  })

  local order = {}
  local b = spawn_actor(h, "b", "sink", {}, function(_, activation)
    order[#order + 1] = activation.messages[1].message.n
    return "ok"
  end)
  -- b is registered but NOT ready yet: two deliveries must queue in arrival
  -- order, not drop and not fire.
  h.router:deliver("b", "a", "q.Item", { n = 1 })
  h.router:deliver("b", "a", "q.Item", { n = 2 })
  assert_eq(#order, 0, "nothing fires before ready")

  ready(h, b)
  assert_eq(#order, 2, "both queued messages drain on ready")
  assert_eq(order[1], 1, "drained in arrival order (first)")
  assert_eq(order[2], 2, "drained in arrival order (second)")
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
  local a = spawn_actor(h, "a", "producer", { ["hop.Ping"] = { "b" } }, function(self)
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

  local w = spawn_actor(h, "w", "worker", { ["job.Done"] = { "out" } }, function(self, activation)
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
  local u1 = spawn_actor(h, "u1", "upstream", { ["mag.Unit"] = { "j" } }, function() return "ok" end)
  local u2 = spawn_actor(h, "u2", "upstream", { ["mag.Unit"] = { "j" } }, function() return "ok" end)
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
  local t = spawn_actor(h, "t", "task", { ["mag.Unit"] = { "d" } }, function(self)
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
-- kill drops firing slots + correlations (forget wired via on_kill)
-- ==================================================================

do
  local h = harness({
    worker = { name = "worker", inputs = { input = "job.Start" }, outputs = { "job.Done" } },
  })
  local w = spawn_actor(h, "w", "worker", {}, function(self)
    self.emit({ kind = "capability.invoke", from = self.id, capability = "compute", request = {}, ref = "r" })
    return nil
  end)
  w.emit({ kind = "mag.ready", from = "w" })
  h.router:fire("w", "seed", "job.Start", {})
  assert_eq(count(h.router.correlation.pending), 1, "one correlation outstanding before kill")

  h.inv.apply({ kills = { "w" } })
  assert_eq(count(h.router.correlation.pending), 0, "kill dropped the outstanding correlation")
  assert_eq(h.router.machines["w"], nil, "kill dropped the firing machine (slots)")
  assert_eq(h.router.ready["w"], nil, "kill dropped the ready flag")
end

print("mag-kernel routing_test: all cases passed")
