-- tests/lua/mag-kernel/lazy_construct_test.lua — lazy actor construction
-- (starter/mag-kernel/inventory.lua + routing.lua + observer.lua wired the way
-- init.lua does). Driven from engine/tests/starter_mag_kernel_test.rs.
--
-- The contract under test (actor-model.md, Lifecycle): apply REGISTERS the
-- spec (id, routes, input contract) and emits mag.actor_spawned; the instance
-- constructs at the actor's FIRST satisfied input contract — mag.actor_ready,
-- then the first deliver — and never before. Covers:
--   (a) a product-input actor stays unconstructed while only some slots are
--       filled, constructs + fires when the last component arrives;
--   (b) a single-input (and union-input) actor constructs on its first
--       message, with spawned-before-ready wire order;
--   (c) kill before construction drops the spec: mag.actor_killed fires, no
--       instance ever exists (no courtesy kill delivery), later sends drop,
--       respawn stays a monotone no-op;
--   (d) a completed run where a routed-but-never-activated actor NEVER
--       constructs (the exhaust-on-an-early-finishing-loop shape).

local inventory = require("inventory")
local Registry = require("registry")
local routing = require("routing")
local observer = require("observer")

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

-- Stand up inventory + registry + routing + observer wired the way init.lua
-- does: the observer emits actor_spawned at registration, the router's
-- construct hook builds instances lazily via the registry, and every
-- construction is recorded in h.constructed (in order).
--
-- `factories` is a map name -> { declaration, make } where `make(id, emit)`
-- returns the instance body: { deliver = fn, handle_kill = fn? }. The harness
-- wraps it to log construction and emit the ready confirm, exactly like the
-- shipped factories do.
local function harness(factories)
  local log, rec = new_logger()
  local events = {}
  local constructed = {}
  local inv = inventory.new({ log = log })
  local reg = Registry.new()
  for _, f in pairs(factories) do
    local decl = f.declaration
    local make = f.make
    local _, err = reg:register({
      declaration = decl,
      construct = function(id, params, emit, deps)
        constructed[#constructed + 1] = id
        local inst = make(id, emit)
        inst.id = id
        emit({ kind = "mag.ready", from = id })
        return inst
      end,
    })
    assert_true(err == nil, "factory registers: " .. tostring(err))
  end
  local router = routing.new({
    inventory = inv,
    registry = reg,
    log = log,
    bus_emit = function() end,
    events = function(e) events[#events + 1] = e end,
  })
  inv.set_on_kill(function(id)
    router:dispatch_kill(id)
    router:forget(id)
  end)
  router:set_construct(function(record)
    return reg:construct(record.factory, record.id, record.params, router:emitter(record.id), {})
  end)
  inv.set_deliver(function(to, from, content)
    content = content or {}
    router:deliver(to, from, content.kind, content)
  end)
  local obs = observer.new({ inventory = inv, emit_event = function(e) events[#events + 1] = e end })
  return { inv = inv, reg = reg, router = router, obs = obs, log = rec, events = events, constructed = constructed }
end

local function has_constructed(h, id)
  for _, c in ipairs(h.constructed) do
    if c == id then return true end
  end
  return false
end

local function event_kinds_for(h, id)
  local out = {}
  for _, e in ipairs(h.events) do
    if e.id == id then out[#out + 1] = e.kind end
  end
  return out
end

-- ==================================================================
-- (a) product input: partial slots buffer without constructing; the last
-- component constructs + fires one assembled activation
-- ==================================================================

do
  local fired = {}
  local h = harness({
    upstream = {
      declaration = { name = "upstream", params = {}, inputs = { input = "go.Kick" }, outputs = { "l.Out", "r.Out" } },
      make = function() return { deliver = function() return "ok" end } end,
    },
    joiner = {
      declaration = { name = "joiner", params = {}, inputs = { joined = { product = { "l.Out", "r.Out" } } }, outputs = {} },
      make = function()
        return { deliver = function(activation)
          fired[#fired + 1] = activation
          return "ok"
        end }
      end,
    },
  })

  local res = h.obs:apply({
    actors = {
      { id = "u1", factory = "upstream", params = {}, routes = { ["l.Out"] = { "j" } } },
      { id = "u2", factory = "upstream", params = {}, routes = { ["r.Out"] = { "j" } } },
      { id = "j", factory = "joiner", params = {}, routes = {} },
    },
  })
  assert_true(res.ok, "constellation registers: " .. tostring(res.error))
  assert_eq(#h.constructed, 0, "registration constructs nothing")

  -- One component: the slot buffers, the joiner stays unconstructed.
  h.router:deliver("j", "u1", "l.Out", { part = "left" })
  assert_true(not has_constructed(h, "j"), "a partial product does not construct")
  assert_eq(#fired, 0, "a partial product does not fire")

  -- The last component: construct, ready, one assembled product activation.
  h.router:deliver("j", "u2", "r.Out", { part = "right" })
  assert_true(has_constructed(h, "j"), "the completing component constructs the joiner")
  assert_eq(#fired, 1, "one complete sender-bound set fires exactly once")
  assert_eq(fired[1].shape, "product", "the assembled activation is a product")
  assert_eq(#fired[1].messages, 2, "the set carries one message per slot")
  -- Slot order derives from a hash-ordered routes scan (derive_slots), so the
  -- assembled set's order is nondeterministic: assert membership, not position.
  local parts = {}
  for _, m in ipairs(fired[1].messages) do
    parts[m.message.part] = true
  end
  assert_true(parts.left, "the buffered first component is in the set")
  assert_true(parts.right, "the completing component is in the set")
end

-- ==================================================================
-- (b) single input constructs on the first message; union likewise; the wire
-- order is spawned (registration) strictly before ready (first activation)
-- ==================================================================

do
  local got = {}
  local h = harness({
    solo = {
      declaration = { name = "solo", params = {}, inputs = { input = "seed.In" }, outputs = {} },
      make = function(id)
        return { deliver = function(activation)
          got[#got + 1] = { id = id, n = activation.messages[1].message.n }
          return "ok"
        end }
      end,
    },
    either = {
      declaration = { name = "either", params = {}, inputs = { boundary = { "a.In", "b.In" } }, outputs = {} },
      make = function(id)
        return { deliver = function(activation)
          got[#got + 1] = { id = id, tag = activation.messages[1].tag }
          return "ok"
        end }
      end,
    },
  })

  local res = h.obs:apply({
    actors = {
      { id = "s", factory = "solo", params = {}, routes = {} },
      { id = "u", factory = "either", params = {}, routes = {} },
    },
    messages = {
      { to = "s", content = { kind = "seed.In", n = 1 } },
      { to = "u", content = { kind = "b.In" } },
    },
  })
  assert_true(res.ok, "apply: " .. tostring(res.error))

  assert_true(has_constructed(h, "s"), "single constructs on its first message")
  assert_true(has_constructed(h, "u"), "union constructs on whichever variant arrives first")
  assert_eq(#got, 2, "both actors fired once")
  assert_eq(got[1].n, 1, "the seed drove the single's first activation")
  assert_eq(got[2].tag, "b.In", "the union activation carries the arriving variant's tag")

  -- Wire order per id: spawned (registration) strictly before ready (first
  -- activation), ready exactly once.
  for _, id in ipairs({ "s", "u" }) do
    local kinds = event_kinds_for(h, id)
    assert_eq(kinds[1], "mag.actor_spawned", id .. ": spawned fires at registration, first")
    assert_eq(kinds[2], "mag.actor_ready", id .. ": ready fires at first activation, after spawned")
  end

  -- A second message reuses the instance: no re-construct.
  h.obs:apply({ messages = { { to = "s", content = { kind = "seed.In", n = 2 } } } })
  local constructs = 0
  for _, c in ipairs(h.constructed) do
    if c == "s" then constructs = constructs + 1 end
  end
  assert_eq(constructs, 1, "a later activation does not re-construct")
  assert_eq(got[3].n, 2, "the second message fired the same instance")
end

-- ==================================================================
-- (c) kill before construction: the spec drops, mag.actor_killed fires, no
-- instance ever exists (no courtesy kill delivery), sends to the dead id
-- drop, respawn stays a monotone no-op
-- ==================================================================

do
  local killed_handler_ran = false
  local delivered = 0
  local h = harness({
    idle = {
      declaration = { name = "idle", params = {}, inputs = { input = "seed.In" }, outputs = {}, signals = { "kill" } },
      make = function()
        return {
          deliver = function() delivered = delivered + 1 return "ok" end,
          handle_kill = function() killed_handler_ran = true end,
        }
      end,
    },
  })

  -- Register without ever messaging: alive externally, no instance inside.
  local res = h.obs:apply({ actors = { { id = "x", factory = "idle", params = {}, routes = {} } } })
  assert_true(res.ok, "spawn x: " .. tostring(res.error))
  assert_eq(h.inv.state_of("x"), "alive", "a registered-but-unconstructed actor counts as alive")
  assert_eq(#h.constructed, 0, "no construction without an activation")

  -- Kill before construction.
  local k = h.obs:apply({ kills = { "x" } })
  assert_true(k.ok, "kill applies")
  assert_eq(h.inv.state_of("x"), "dead", "the spec dropped to a tombstone")
  assert_true(not killed_handler_ran, "no courtesy kill delivery — no instance exists")
  assert_true(not has_constructed(h, "x"), "kill did not construct")

  local kinds = event_kinds_for(h, "x")
  assert_eq(kinds[1], "mag.actor_spawned", "spawned fired at registration")
  assert_eq(kinds[2], "mag.actor_killed", "actor_killed still fires for observability")
  assert_eq(#kinds, 2, "no ready ever fired for the unconstructed actor")

  -- A send to the dead id drops as a logged no-op; nothing constructs.
  local s = h.obs:apply({ messages = { { to = "x", content = { kind = "seed.In" } } } })
  assert_true(s.ok, "a dead-target send is a no-op, not a rejection")
  assert_eq(delivered, 0, "nothing was delivered to the dead id")
  assert_eq(#h.constructed, 0, "a dead-target send does not construct")

  -- Respawn after kill: monotone no-op (docs/ir.md), still dead.
  local r = h.obs:apply({ actors = { { id = "x", factory = "idle", params = {}, routes = {} } } })
  assert_true(r.ok, "respawn-after-kill is a no-op, not a rejection")
  assert_eq(h.inv.state_of("x"), "dead", "spawn cannot revive a dead id")
  assert_eq(#h.constructed, 0, "the no-op respawn constructs nothing")
end

-- ==================================================================
-- (d) a completed run: the routed-but-never-activated actor (exhaust on a
-- loop that finished early) NEVER constructs
-- ==================================================================

do
  local h = harness({
    entry = {
      -- Two declared exits: the happy path (hop.Ping → sink) and the never-
      -- taken one (alt.Out → exhaust). deliver emits only the happy path.
      declaration = { name = "entry", params = {}, inputs = { input = "seed.In" }, outputs = { "hop.Ping", "alt.Out" } },
      make = function(id, emit)
        return { deliver = function()
          emit({ kind = "hop.Ping", from = id, payload = "done" })
          return "ok"
        end }
      end,
    },
    terminal = {
      declaration = { name = "terminal", params = {}, inputs = { final = "hop.Ping" }, outputs = {} },
      make = function(id, emit)
        return { deliver = function()
          emit({ kind = "mag.RunComplete", from = id, result = { text = "done" }, persisted = false })
          return "ok"
        end }
      end,
    },
    summarizer = {
      declaration = { name = "summarizer", params = {}, inputs = { input = "alt.Out" }, outputs = {} },
      make = function()
        return { deliver = function() return "ok" end }
      end,
    },
  })

  local res = h.obs:apply({
    actors = {
      { id = "entry", factory = "entry", params = {}, routes = { ["hop.Ping"] = { "sink" }, ["alt.Out"] = { "exhaust" } } },
      { id = "sink", factory = "terminal", params = {}, routes = {} },
      { id = "exhaust", factory = "summarizer", params = {}, routes = {} },
    },
    messages = { { to = "entry", content = { kind = "seed.In" } } },
  })
  assert_true(res.ok, "apply: " .. tostring(res.error))

  -- The run completed through entry → sink…
  local completed = false
  for _, e in ipairs(h.events) do
    if e.kind == "mag.run_complete" then completed = true end
  end
  assert_true(completed, "the run completed")
  assert_true(has_constructed(h, "entry"), "entry constructed (it fired)")
  assert_true(has_constructed(h, "sink"), "sink constructed (it fired)")

  -- …and the never-activated exhaust was spawned but never constructed.
  assert_true(not has_constructed(h, "exhaust"), "the never-activated actor never constructs")
  local kinds = event_kinds_for(h, "exhaust")
  assert_eq(kinds[1], "mag.actor_spawned", "exhaust was spawned (registered)")
  for _, k in ipairs(kinds) do
    assert_true(k ~= "mag.actor_ready", "exhaust never readied — it never began work")
  end
  assert_eq(h.inv.state_of("exhaust"), "alive", "the unconstructed actor still counts as alive")
end

print("mag-kernel lazy_construct_test: all cases passed")
