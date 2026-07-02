-- tests/lua/mag-kernel/barrier_test.lua — the program-start ready barrier
-- (starter/mag-kernel/barrier.lua) composed with the fold (inventory.lua),
-- factory construction, and routing. Driven from
-- engine/tests/starter_mag_kernel_test.rs.
--
-- Covers the task's done-when list:
--   * normal path — every actor's ready is observed strictly before any initial
--     message is delivered, and the seed reaches the entry actor intact;
--   * one actor never confirms — the run fails naming exactly that actor and no
--     initial message is delivered;
--   * mixed — two confirm, one doesn't — the failure names only the straggler;
--   * injected-clock deadline — a late confirm before the deadline releases the
--     barrier; a straggler at/after the deadline fails.
--
-- The harness wires inventory + registry + routing + the construct/deliver
-- hooks exactly the way init.lua does, but with capturing test factories so the
-- ordering of readies vs. deliveries is observable.

local inventory = require("inventory")
local Registry  = require("registry")
local routing   = require("routing")
local barrier   = require("barrier")

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

local function assert_contains(haystack, needle, msg)
  if type(haystack) ~= "string" or not haystack:find(needle, 1, true) then
    error(string.format("assertion failed: %s\n  %q does not contain %q",
      msg or "substring missing", tostring(haystack), needle), 2)
  end
end

local function new_logger()
  local rec = { info = {}, warn = {}, error = {} }
  local function sink(bucket)
    return function(m) bucket[#bucket + 1] = m end
  end
  return { info = sink(rec.info), warn = sink(rec.warn), error = sink(rec.error) }, rec
end

-- A test factory that appends to a shared `events` log on both ready and
-- deliver, so a test can assert the barrier's ordering. `spec`:
--   name / input      declaration name + single input tag
--   output (optional) a declared output tag re-emitted on deliver (cascade)
--   defer  (optional) when true the constructor does NOT emit ready — the test
--                     drives a late confirm via router:on_ready to simulate an
--                     async factory.
local function make_factory(events, spec)
  return {
    declaration = {
      name = spec.name,
      inputs = { input = spec.input },
      outputs = spec.output and { spec.output } or {},
    },
    construct = function(id, params, emit, deps)
      local inst = { id = id }
      inst.deliver = function(activation)
        local one = (activation.messages or {})[1] or {}
        events[#events + 1] = { t = "deliver", id = id, message = one.message, tag = one.tag }
        if spec.output then
          emit({ kind = spec.output, from = id, payload = (one.message or {}).payload })
        end
        return "ok"
      end
      if not spec.defer then
        events[#events + 1] = { t = "ready", id = id }
        emit({ kind = "mag.ready", from = id })
      end
      return inst
    end,
  }
end

-- Stand up inventory + registry + routing with the construct/deliver hooks
-- wired like init.lua. `factories` is a map name -> { declaration, construct }.
local function harness(factories)
  local log, rec = new_logger()
  local inv = inventory.new({ log = log })
  local reg = Registry.new()
  for _, f in pairs(factories) do
    local _, err = reg:register({ declaration = f.declaration, construct = f.construct })
    assert_true(err == nil, "factory registers: " .. tostring(err))
  end
  local seq = 0
  local router = routing.new({
    inventory = inv,
    registry = reg,
    log = log,
    bus_emit = function() end,
    gen_id = function() seq = seq + 1; return "req-" .. seq end,
  })
  inv.set_on_kill(function(id) router:forget(id) end)
  inv.set_construct(function(record)
    local emit = router:emitter(record.id)
    local inst, err = reg:construct(record.factory, record.id, record.params, emit, {})
    if inst and not err then router:bind(record.id, inst) end
  end)
  inv.set_deliver(function(to, from, content)
    content = content or {}
    router:deliver(to, from, content.kind, content)
  end)
  return { inv = inv, reg = reg, router = router, log = rec }
end

-- Index of the last "ready" event and the first "deliver" event.
local function last_ready(events)
  local idx = 0
  for i, e in ipairs(events) do if e.t == "ready" then idx = i end end
  return idx
end
local function first_deliver(events)
  for i, e in ipairs(events) do if e.t == "deliver" then return i end end
  return nil
end
local function count_deliver(events, id)
  local n = 0
  for _, e in ipairs(events) do if e.t == "deliver" and (id == nil or e.id == id) then n = n + 1 end end
  return n
end

-- ==================================================================
-- normal path — initial messages delivered strictly after all readies
-- ==================================================================

do
  local events = {}
  local h = harness({
    entry    = make_factory(events, { name = "entry", input = "seed.In", output = "hop.Ping" }),
    consumer = make_factory(events, { name = "consumer", input = "hop.Ping" }),
  })

  local handle = barrier.start({
    inventory = h.inv,
    router = h.router,
    now = function() return 0 end,
    mod = {
      actors = {
        { id = "entry",    factory = "entry",    params = {}, routes = { ["hop.Ping"] = { "consumer" } } },
        { id = "consumer", factory = "consumer", params = {}, routes = {} },
      },
      messages = {
        { to = "entry", content = { kind = "seed.In", payload = "go" } },
      },
    },
  })

  assert_true(handle.done, "synchronous constellation settles at start")
  assert_true(handle.ok, "barrier passes: " .. tostring(handle.error))
  assert_true(handle.released, "barrier released the initial messages")

  -- Strict ordering: every ready precedes every delivery.
  local lr = last_ready(events)
  local fd = first_deliver(events)
  assert_true(fd ~= nil, "at least one delivery happened")
  assert_eq(lr, 2, "both actors readied (two ready events)")
  assert_true(lr < fd, "all readies observed strictly before the first delivery")

  -- The seed reached the entry actor intact, and the cascade reached consumer.
  assert_eq(count_deliver(events, "entry"), 1, "entry received the seed")
  assert_eq(count_deliver(events, "consumer"), 1, "consumer received the cascaded hop.Ping")
  local entry_deliver
  for _, e in ipairs(events) do if e.t == "deliver" and e.id == "entry" then entry_deliver = e end end
  assert_eq(entry_deliver.message.payload, "go", "seed payload delivered intact")
end

-- ==================================================================
-- one actor never confirms — run fails naming exactly that actor,
-- and no initial message is delivered
-- ==================================================================

do
  local events = {}
  local h = harness({
    good = make_factory(events, { name = "good", input = "seed.In" }),
    bad  = make_factory(events, { name = "bad",  input = "seed.In", defer = true }),
  })

  local handle = barrier.start({
    inventory = h.inv,
    router = h.router,
    now = function() return 0 end,
    deadline_ms = 1000,
    mod = {
      actors = {
        { id = "good", factory = "good", params = {}, routes = {} },
        { id = "bad",  factory = "bad",  params = {}, routes = {} },
      },
      messages = {
        { to = "good", content = { kind = "seed.In", payload = "x" } },
      },
    },
  })

  assert_true(not handle.done, "barrier is still awaiting bad's confirmation at start")

  -- Deadline passes with bad still unconfirmed.
  barrier.poll(handle, 2000)
  assert_true(handle.done, "barrier settles once the deadline passes")
  assert_true(not handle.ok, "the run fails: bad never confirmed")
  assert_true(not handle.released, "a failed start releases nothing")
  assert_eq(#handle.stragglers, 1, "exactly one straggler")
  assert_eq(handle.stragglers[1], "bad", "the straggler is named")
  assert_contains(handle.error, "bad", "the error names the straggler")
  assert_true(not handle.error:find("good", 1, true), "the error does not name the actor that confirmed")

  -- No initial message delivered on a failed start (good's seed stays queued).
  assert_eq(count_deliver(events), 0, "no delivery on a failed barrier")
end

-- ==================================================================
-- mixed — two confirm, one doesn't — the failure names only the straggler
-- ==================================================================

do
  local events = {}
  local h = harness({
    a1  = make_factory(events, { name = "a1",  input = "seed.In" }),
    a2  = make_factory(events, { name = "a2",  input = "seed.In" }),
    lag = make_factory(events, { name = "lag", input = "seed.In", defer = true }),
  })

  local handle = barrier.start({
    inventory = h.inv,
    router = h.router,
    now = function() return 0 end,
    deadline_ms = 1000,
    mod = {
      actors = {
        { id = "a1",  factory = "a1",  params = {}, routes = {} },
        { id = "a2",  factory = "a2",  params = {}, routes = {} },
        { id = "lag", factory = "lag", params = {}, routes = {} },
      },
    },
  })

  assert_true(not handle.done, "two confirmed, one lagging — still awaiting")
  barrier.poll(handle, 5000)
  assert_true(not handle.ok, "the run fails on the lagging actor")
  assert_eq(#handle.stragglers, 1, "only the lagging actor is a straggler")
  assert_eq(handle.stragglers[1], "lag", "names only the straggler")
  assert_true(not handle.error:find("a1", 1, true) and not handle.error:find("a2", 1, true),
    "the confirmed actors are not named")
end

-- ==================================================================
-- injected-clock deadline — a late confirm before the deadline releases;
-- the boundary (t == deadline) with a straggler fails
-- ==================================================================

do
  -- (a) late-but-in-time confirm releases the barrier.
  local events = {}
  local h = harness({
    sync  = make_factory(events, { name = "sync",  input = "seed.In" }),
    async = make_factory(events, { name = "async", input = "seed.In", defer = true }),
  })

  local handle = barrier.start({
    inventory = h.inv,
    router = h.router,
    now = function() return 0 end,
    deadline_ms = 1000,
    mod = {
      actors = {
        { id = "sync",  factory = "sync",  params = {}, routes = {} },
        { id = "async", factory = "async", params = {}, routes = {} },
      },
      messages = {
        { to = "async", content = { kind = "seed.In", payload = "late" } },
      },
    },
  })
  assert_true(not handle.done, "still awaiting the async actor before it confirms")
  assert_eq(count_deliver(events), 0, "nothing delivered while awaiting")

  -- The async factory confirms at t=500 (before the 1000 deadline).
  h.router:on_ready("async")
  barrier.poll(handle, 500)
  assert_true(handle.done and handle.ok, "a confirm before the deadline releases the barrier")
  assert_true(handle.released, "the barrier delivered the initial messages")
  assert_eq(count_deliver(events, "async"), 1, "async received its seed after the late confirm")

  -- (b) boundary: a straggler at exactly the deadline fails (t >= deadline).
  local h2 = harness({
    never = make_factory({}, { name = "never", input = "seed.In", defer = true }),
  })
  local h2h = barrier.start({
    inventory = h2.inv,
    router = h2.router,
    now = function() return 0 end,
    deadline_ms = 1000,
    mod = { actors = { { id = "never", factory = "never", params = {}, routes = {} } } },
  })
  assert_true(not h2h.done, "awaiting the never-confirming actor")
  barrier.poll(h2h, 1000) -- exactly at the deadline
  assert_true(h2h.done and not h2h.ok, "t == deadline with a straggler fails")
  assert_eq(h2h.stragglers[1], "never", "boundary failure names the straggler")
end

print("mag-kernel barrier_test: all cases passed")
