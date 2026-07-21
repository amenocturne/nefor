-- tests/lua/mag-kernel/activity_test.lua — per-actor busy/idle activity
-- events (plugins/mag/lua/mag-kernel/routing.lua wired the way init.lua does).
-- Driven from engine/tests/starter_mag_kernel_test.rs.
--
-- The contract under test (actor-model.md, Activity): `mag.actor_busy` fires
-- at activation delivery, `mag.actor_idle` when that activation's completion
-- settles — sync return, async mag.complete emit, or a capability reply
-- resolving a pending completion — failed settles included, with `busy_ms`
-- spanning the window. Alternation is strict per actor. Covers:
--   (a) sync actors: busy/idle per activation, ready-before-first-busy wire
--       order, busy_ms from the injected clock, idle(upstream) before
--       busy(dependent) along a mag.Unit edge;
--   (b) async capability actors: busy at activation, NO idle while pending,
--       idle at the bus_response settle spanning the wait;
--   (c) async settle through a mag.complete emit;
--   (d) failed settles (routed and unrouted) still emit idle.

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

local function silent_log()
  local function noop() end
  return { info = noop, warn = noop, error = noop }
end

-- Stand up inventory + registry + routing + observer the way init.lua wires
-- them, plus a fake clock (h.clock.advance) injected as routing's now_ms and
-- a capturing bus sink (h.bus). Factories: name -> { declaration, make }.
local function harness(factories)
  local log = silent_log()
  local events = {}
  local bus = {}
  local now = { ms = 0 }
  local inv = inventory.new({ log = log })
  local reg = Registry.new()
  for _, f in pairs(factories) do
    local make = f.make
    local _, err = reg:register({
      declaration = f.declaration,
      construct = function(id, params, emit, deps)
        local inst = make(id, emit)
        inst.id = id
        emit({ kind = "mag.ready", from = id })
        return inst
      end,
    })
    assert_true(err == nil, "factory registers: " .. tostring(err))
  end
  local cap_seq = 0
  local router = routing.new({
    inventory = inv,
    registry = reg,
    log = log,
    bus_emit = function(e) bus[#bus + 1] = e end,
    events = function(e) events[#events + 1] = e end,
    now_ms = function() return now.ms end,
    gen_id = function()
      cap_seq = cap_seq + 1
      return "cap-" .. tostring(cap_seq)
    end,
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
  return {
    inv = inv, router = router, obs = obs,
    events = events, bus = bus,
    clock = { advance = function(ms) now.ms = now.ms + ms end },
  }
end

local function events_for(h, id)
  local out = {}
  for _, e in ipairs(h.events) do
    if e.id == id then out[#out + 1] = e end
  end
  return out
end

local function kinds_of(evts)
  local out = {}
  for i, e in ipairs(evts) do out[i] = e.kind end
  return out
end

-- ==================================================================
-- (a) sync actors: strict busy/idle alternation per activation, busy_ms from
-- the clock, ready before the first busy, idle(upstream) before
-- busy(dependent) along the mag.Unit dependency edge
-- ==================================================================

do
  local h
  h = harness({
    worker = {
      declaration = { name = "worker", params = {}, inputs = { input = "seed.In" }, outputs = {} },
      make = function()
        return { deliver = function()
          h.clock.advance(40)
          return "ok"
        end }
      end,
    },
    follower = {
      declaration = { name = "follower", params = {}, inputs = { input = "mag.Unit" }, outputs = {} },
      make = function()
        return { deliver = function() return "ok" end }
      end,
    },
  })

  local res = h.obs:apply({
    actors = {
      { id = "w", factory = "worker", params = {}, routes = { ["mag.Unit"] = { { actor = "f", wire = "mag.Unit" } } } },
      { id = "f", factory = "follower", params = {}, routes = {} },
    },
    messages = { { to = "w", content = { kind = "seed.In" } } },
  })
  assert_true(res.ok, "apply: " .. tostring(res.error))

  -- Wire order for w: spawned, ready (construction), busy, idle.
  local w = events_for(h, "w")
  local wk = kinds_of(w)
  assert_eq(wk[1], "mag.actor_spawned", "w: spawned first")
  assert_eq(wk[2], "mag.actor_ready", "w: ready at construction")
  assert_eq(wk[3], "mag.actor_busy", "w: busy at activation delivery, after ready")
  assert_eq(wk[4], "mag.actor_idle", "w: idle at the sync settle")
  assert_eq(w[4].busy_ms, 40, "w: busy_ms spans the deliver window")

  -- The dependent's busy follows the upstream's idle on the wire.
  local order = {}
  for _, e in ipairs(h.events) do
    if e.kind == "mag.actor_busy" or e.kind == "mag.actor_idle" then
      order[#order + 1] = e.kind .. ":" .. e.id
    end
  end
  assert_eq(order[1], "mag.actor_busy:w", "w goes busy first")
  assert_eq(order[2], "mag.actor_idle:w", "w settles before its dependent starts")
  assert_eq(order[3], "mag.actor_busy:f", "the mag.Unit edge fires f after w's idle")
  assert_eq(order[4], "mag.actor_idle:f", "f settles")

  -- A second activation is a fresh window: strict alternation, new busy_ms.
  h.obs:apply({ messages = { { to = "w", content = { kind = "seed.In" } } } })
  local wk2 = kinds_of(events_for(h, "w"))
  assert_eq(wk2[5], "mag.actor_busy", "w: the second activation opens a new window")
  assert_eq(wk2[6], "mag.actor_idle", "w: and settles it")
  assert_eq(#wk2, 6, "w: exactly one busy/idle pair per activation")
end

-- ==================================================================
-- (b) async capability actor: busy at activation, pending emits no idle, the
-- bus_response settle emits idle spanning the whole wait
-- ==================================================================

do
  local h = harness({
    caller = {
      declaration = { name = "caller", params = {}, inputs = { input = "seed.In" }, outputs = {} },
      make = function(id, emit)
        return {
          deliver = function(activation)
            if activation.kind == "reply" then
              return "ok"
            end
            emit({ kind = "capability.invoke", from = id, capability = "search", request = {}, ref = "r1" })
            return { status = "pending" }
          end,
        }
      end,
    },
  })

  h.obs:apply({
    actors = { { id = "c", factory = "caller", params = {}, routes = {} } },
    messages = { { to = "c", content = { kind = "seed.In" } } },
  })

  local ck = kinds_of(events_for(h, "c"))
  assert_eq(ck[#ck], "mag.actor_busy", "a pending completion leaves the actor busy — no idle yet")

  -- The capability round-trip resolves the pending completion.
  h.clock.advance(250)
  assert_eq(h.bus[1].kind, "tool.invoke", "the capability request reached the bus")
  local claimed = h.router:bus_response({ id = h.bus[1].id, result = { hits = 3 } })
  assert_true(claimed, "the reply correlates to our request")

  local c = events_for(h, "c")
  assert_eq(c[#c].kind, "mag.actor_idle", "the reply settle emits idle")
  assert_eq(c[#c].busy_ms, 250, "busy_ms spans activation → reply settle")
end

-- ==================================================================
-- (c) async settle through a mag.complete emit (drain-flush shape)
-- ==================================================================

do
  local h = harness({
    deferred = {
      declaration = { name = "deferred", params = {}, inputs = { input = "seed.In" }, outputs = {} },
      make = function(id, emit)
        return {
          deliver = function() return { status = "pending" } end,
          finish = function() emit({ kind = "mag.complete", from = id }) end,
        }
      end,
    },
  })

  h.obs:apply({
    actors = { { id = "d", factory = "deferred", params = {}, routes = {} } },
    messages = { { to = "d", content = { kind = "seed.In" } } },
  })
  local dk = kinds_of(events_for(h, "d"))
  assert_eq(dk[#dk], "mag.actor_busy", "deferred actor is busy while pending")

  h.clock.advance(70)
  h.router.instances["d"].finish()
  local d = events_for(h, "d")
  assert_eq(d[#d].kind, "mag.actor_idle", "the mag.complete ack settles the window")
  assert_eq(d[#d].busy_ms, 70, "busy_ms spans the deferred window")
end

-- ==================================================================
-- (d) failed settles still emit idle — both the routed-failure path and the
-- unrouted escalation (mag.run_failed)
-- ==================================================================

do
  local h = harness({
    flaky = {
      declaration = { name = "flaky", params = {}, inputs = { input = "seed.In" }, outputs = { "flaky.Err" } },
      make = function()
        return { deliver = function()
          return { status = "failed", failure = "flaky.Err", value = { error = "boom" } }
        end }
      end,
    },
    catcher = {
      declaration = { name = "catcher", params = {}, inputs = { input = "flaky.Err" }, outputs = {} },
      make = function()
        return { deliver = function() return "ok" end }
      end,
    },
  })

  -- Routed failure: flaky.Err reaches the catcher, and flaky still idles.
  h.obs:apply({
    actors = {
      { id = "flaky", factory = "flaky", params = {}, routes = { ["flaky.Err"] = { { actor = "catch", wire = "flaky.Err" } } } },
      { id = "catch", factory = "catcher", params = {}, routes = {} },
    },
    messages = { { to = "flaky", content = { kind = "seed.In" } } },
  })
  local fk = kinds_of(events_for(h, "flaky"))
  assert_eq(fk[#fk], "mag.actor_idle", "a routed failed settle emits idle")

  -- Unrouted failure: the escalation (mag.run_failed) rides with an idle too.
  local h2 = harness({
    flaky = {
      declaration = { name = "flaky", params = {}, inputs = { input = "seed.In" }, outputs = { "flaky.Err" } },
      make = function()
        return { deliver = function()
          return { status = "failed", failure = "flaky.Err", value = { error = "boom" } }
        end }
      end,
    },
  })
  h2.obs:apply({
    actors = { { id = "solo", factory = "flaky", params = {}, routes = {} } },
    messages = { { to = "solo", content = { kind = "seed.In" } } },
  })
  local sk = kinds_of(events_for(h2, "solo"))
  assert_eq(sk[#sk], "mag.actor_idle", "an unrouted failed settle emits idle")
  local escalated = false
  for _, e in ipairs(h2.events) do
    if e.kind == "mag.run_failed" and e.from == "solo" then escalated = true end
  end
  assert_true(escalated, "the unrouted failure still escalates as mag.run_failed")
end

print("mag-kernel activity_test: all cases passed")
