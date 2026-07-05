-- tests/lua/mag-kernel/validation_test.lua — apply-time route/port contract
-- validation (inventory.lua validate_routes + registry.lua
-- validate_modification with a resolver) and the delivery layer's
-- no-input-port escalation (routing.lua fire).
--
-- The bug class under test (session d66360ff): a modification routed
-- `mag.LoopExhausted` into an actor whose factory declared no accepting input
-- port; the delivery layer warn-dropped the message, the destination starved,
-- and the run hung forever with zero pending work. Two layers now make that
-- unrepresentable:
--   1. apply-time: a route whose tag no destination port accepts REJECTS the
--      modification (mag.modification_rejected, precise wiring error);
--   2. runtime backstop: a dynamic mismatch the validator cannot see (a
--      modification MESSAGE with an unaccepted kind) escalates to
--      mag.run_failed instead of warn-dropping. Sends to dead targets stay
--      drop-and-log (settled race semantics).
--
-- Driven from engine/tests/starter_mag_kernel_test.rs: bare Lua VM, stub
-- nefor.log, package.path at starter/mag-kernel/.

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

local function assert_contains(haystack, needle, msg)
  if type(haystack) ~= "string" or not haystack:find(needle, 1, true) then
    error(string.format(
      "assertion failed: %s\n  expected to contain: %s\n  actual: %s",
      msg or "substring missing", tostring(needle), tostring(haystack)), 2)
  end
end

local function new_logger()
  local rec = { info = {}, warn = {}, error = {} }
  local function sink(bucket)
    return function(m) bucket[#bucket + 1] = m end
  end
  return { info = sink(rec.info), warn = sink(rec.warn), error = sink(rec.error) }, rec
end

-- ------------------------------------------------------------------
-- harness — inventory + registry + routing + observer wired the way
-- init.lua does, INCLUDING the registry injected into the inventory so
-- apply validates routes (the seam under test).
-- ------------------------------------------------------------------

-- Test factory set:
--   src   — outputs t.Out and t.Alt; accepts t.In        (the sender)
--   dst   — accepts t.Out only                           (the narrow receiver)
--   fan   — union input (t.Out | t.Alt)                  (the wide receiver)
--   unit  — accepts mag.Unit                             (a dependency consumer)
--   gate  — accepts t.Sub; records deliveries            (ApprovalReply bypass)
local function harness()
  local log, rec = new_logger()
  local events = {}
  local delivered = {} -- gate deliveries, for the bypass test
  local reg = Registry.new()

  local function simple_factory(name, inputs, outputs)
    return {
      declaration = { name = name, params = {}, inputs = inputs, outputs = outputs },
      construct = function(id, params, emit, deps)
        local inst = { id = id }
        function inst.deliver(activation)
          if name == "gate" then
            delivered[#delivered + 1] = activation
          end
          return { status = "ok" }
        end
        emit({ kind = "mag.ready", from = id })
        return inst
      end,
    }
  end

  for _, f in ipairs({
    simple_factory("src", { i = "t.In" }, { "t.Out", "t.Alt" }),
    simple_factory("dst", { i = "t.Out" }, {}),
    simple_factory("fan", { b = { "t.Out", "t.Alt" } }, {}),
    simple_factory("unit", { u = "mag.Unit" }, {}),
    simple_factory("gate", { s = "t.Sub" }, {}),
  }) do
    local _, err = reg:register(f)
    assert_true(err == nil, "factory registers: " .. tostring(err))
  end

  local inv = inventory.new({ log = log, registry = reg })
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
  return {
    inv = inv, reg = reg, router = router, obs = obs,
    log = rec, events = events, delivered = delivered,
  }
end

local function events_of_kind(h, kind)
  local out = {}
  for _, e in ipairs(h.events) do
    if e.kind == kind then out[#out + 1] = e end
  end
  return out
end

-- ==================================================================
-- (1) a route whose tag no destination port accepts REJECTS the
-- modification, naming actor, tag, and destination
-- ==================================================================

do
  local h = harness()
  local result = h.obs:apply({
    actors = {
      { id = "s", factory = "src", params = {}, routes = { ["t.Alt"] = { "d" } } },
      { id = "d", factory = "dst", params = {}, routes = {} },
    },
  })
  assert_eq(result.ok, false, "the incompatible wiring rejects the modification")
  assert_contains(result.error, 'wiring "s" -t.Alt-> "d"', "the error names actor, tag, destination")
  assert_contains(result.error, 'no input of factory "dst"', "the error names the destination factory")

  -- Rejection machinery: mag.modification_rejected carries the same error,
  -- and the fold stayed untouched (nothing spawned).
  local rejected = events_of_kind(h, "mag.modification_rejected")
  assert_eq(#rejected, 1, "the rejection surfaced as mag.modification_rejected")
  assert_contains(rejected[1].error, 'wiring "s" -t.Alt-> "d"', "the event carries the wiring error")
  assert_eq(h.inv.state_of("s"), "never-existed", "a rejected modification spawns nothing")
  assert_eq(h.inv.state_of("d"), "never-existed", "a rejected modification spawns nothing (dest)")
end

-- ==================================================================
-- (2) a route key that is not a declared output rejects
-- ==================================================================

do
  local h = harness()
  local result = h.obs:apply({
    actors = {
      { id = "s", factory = "src", params = {}, routes = { ["t.Nope"] = { "d" } } },
      { id = "d", factory = "dst", params = {}, routes = {} },
    },
  })
  assert_eq(result.ok, false, "an undeclared route key rejects")
  assert_contains(result.error, 'route key "t.Nope" is not a declared output', "the error names the key")
end

-- ==================================================================
-- (3) good wiring passes: single ports, union ports, reserved
-- kernel-synthesized route keys (mag.Unit / mag.Failed bypass the
-- declared-output check but destination acceptance still holds)
-- ==================================================================

do
  local h = harness()
  local result = h.obs:apply({
    actors = {
      {
        id = "s", factory = "src", params = {},
        routes = {
          ["t.Out"] = { "d", "f" }, -- fanout: single port + union port
          ["t.Alt"] = { "f" }, -- second union variant
          ["mag.Unit"] = { "u" }, -- dependency edge: reserved key, undeclared
        },
      },
      { id = "d", factory = "dst", params = {}, routes = {} },
      { id = "f", factory = "fan", params = {}, routes = {} },
      { id = "u", factory = "unit", params = {}, routes = {} },
    },
  })
  assert_eq(result.ok, true, "compatible wiring (union ports, reserved keys) applies: "
    .. tostring(result.error))
  assert_eq(h.inv.state_of("s"), "alive", "the modification applied")
end

-- ==================================================================
-- (4) a reserved route key still checks destination acceptance —
-- a mag.Unit dependency edge into a non-Unit port rejects
-- ==================================================================

do
  local h = harness()
  local result = h.obs:apply({
    actors = {
      { id = "s", factory = "src", params = {}, routes = { ["mag.Unit"] = { "d" } } },
      { id = "d", factory = "dst", params = {}, routes = {} },
    },
  })
  assert_eq(result.ok, false, "a dependency edge into a non-Unit port rejects")
  assert_contains(result.error, 'wiring "s" -mag.Unit-> "d"', "the error names the dependency edge")
end

-- ==================================================================
-- (5) destinations resolve against the POST-APPLY set: already-live
-- destinations are validated too (bad tag rejects), never-existed
-- destinations reject, dead destinations are skipped (race semantics)
-- ==================================================================

do
  local h = harness()
  -- d lives from an earlier modification.
  local first = h.obs:apply({
    actors = { { id = "d", factory = "dst", params = {}, routes = {} } },
  })
  assert_eq(first.ok, true, "the first modification applies")

  -- A later spawn routing an unaccepted tag AT THE LIVE d rejects.
  local bad = h.obs:apply({
    actors = { { id = "s", factory = "src", params = {}, routes = { ["t.Alt"] = { "d" } } } },
  })
  assert_eq(bad.ok, false, "wiring into a live destination is validated")
  assert_contains(bad.error, 'wiring "s" -t.Alt-> "d"', "the live-destination error is precise")

  -- A route at a never-existed id rejects (a typo, not a race).
  local ghost = h.obs:apply({
    actors = { { id = "s2", factory = "src", params = {}, routes = { ["t.Out"] = { "ghost" } } } },
  })
  assert_eq(ghost.ok, false, "a never-existed route destination rejects")
  assert_contains(ghost.error, '"ghost" does not exist', "the error names the missing destination")

  -- Kill d; a route at the DEAD d is a race artifact and passes validation
  -- (delivery drops those sends as logged no-ops).
  assert_eq(h.obs:apply({ kills = { "d" } }).ok, true, "the kill applies")
  local dead = h.obs:apply({
    actors = { { id = "s3", factory = "src", params = {}, routes = { ["t.Out"] = { "d" } } } },
  })
  assert_eq(dead.ok, true, "a route at a dead destination passes (settled race semantics): "
    .. tostring(dead.error))
end

-- ==================================================================
-- (6) runtime backstop: a dynamic mismatch (a modification MESSAGE whose
-- kind no port accepts) fails the run via mag.run_failed — not a silent
-- warn-drop
-- ==================================================================

do
  local h = harness()
  local result = h.obs:apply({
    actors = { { id = "d", factory = "dst", params = {}, routes = {} } },
    messages = { { to = "d", content = { kind = "t.Weird", payload = 1 } } },
  })
  -- The modification itself applies (message kinds are dynamic; only routes
  -- are statically checkable) — the mismatch surfaces as a run failure.
  assert_eq(result.ok, true, "the modification applies; the mismatch is a runtime event")
  local failed = events_of_kind(h, "mag.run_failed")
  assert_eq(#failed, 1, "the unaccepted delivery escalated to mag.run_failed")
  assert_eq(failed[1].failure, "no-input-port", "the failure names the class")
  assert_contains(failed[1].error, "actor 'd'", "the detail names the destination actor")
  assert_contains(failed[1].error, "tag 't.Weird'", "the detail names the tag")
end

-- ==================================================================
-- (7) sends to DEAD targets still drop-and-log (never escalate) —
-- the settled race semantics are untouched
-- ==================================================================

do
  local h = harness()
  assert_eq(h.obs:apply({
    actors = { { id = "d", factory = "dst", params = {}, routes = {} } },
  }).ok, true, "spawn applies")
  assert_eq(h.obs:apply({ kills = { "d" } }).ok, true, "kill applies")
  h.router:deliver("d", "elsewhere", "t.Out", { kind = "t.Out" })
  assert_eq(#events_of_kind(h, "mag.run_failed"), 0, "a dead-target send never escalates")
  local logged = false
  for _, line in ipairs(h.log.info) do
    if line:find("dead", 1, true) then logged = true end
  end
  assert_true(logged, "the dead-target drop is logged")
end

-- ==================================================================
-- (8) mag.ApprovalReply bypasses declared ports: a CONSTRUCTED actor
-- receives it as a tagged delivery; an unconstructed one drops it logged,
-- and neither path escalates
-- ==================================================================

do
  local h = harness()
  assert_eq(h.obs:apply({
    actors = { { id = "g", factory = "gate", params = {}, routes = {} } },
    messages = { { to = "g", content = { kind = "t.Sub", text = "subject" } } },
  }).ok, true, "the gate spawns and constructs on its subject")
  assert_eq(#h.delivered, 1, "the subject activated the gate")

  h.router:deliver("g", "mag.control", "mag.ApprovalReply", { approved = true })
  assert_eq(#h.delivered, 2, "the reply reached the constructed gate past the declared ports")
  assert_eq(h.delivered[2].messages[1].tag, "mag.ApprovalReply", "the delivery carries the reply tag")
  assert_eq(#events_of_kind(h, "mag.run_failed"), 0, "a bypass delivery never escalates")

  -- A reply at a spawned-but-never-constructed gate: nothing awaits it.
  assert_eq(h.obs:apply({
    actors = { { id = "g2", factory = "gate", params = {}, routes = {} } },
  }).ok, true, "the second gate registers")
  h.router:deliver("g2", "mag.control", "mag.ApprovalReply", { approved = true })
  assert_eq(#h.delivered, 2, "an unconstructed gate receives nothing")
  assert_eq(#events_of_kind(h, "mag.run_failed"), 0, "the stray reply never escalates")
end

print("mag-kernel validation_test: all assertions passed")
