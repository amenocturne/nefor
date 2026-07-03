-- tests/lua/mag-kernel/observability_test.lua — unit tests for the mag-kernel
-- observability layer (modlog.lua, observer.lua, and routing.lua's event +
-- output-persistence seams). Driven from engine/tests/starter_mag_kernel_test.rs.
--
-- Covers the task's done-when list:
--   1. a scripted run yields a complete, ordered modification log
--      (applied / rejected / no-op) and replay folds it back to a state that
--      matches the live inventory;
--   2. lifecycle events land on a stubbed emitter in order across the fold
--      (observer) and the delivery layer (routing: ready, run-complete);
--   3. every actor's output lands via a stubbed writer keyed by node id —
--      non-sink actors through routing's persist_output, the sink through its
--      injected deps.writer.

local inventory = require("inventory")
local Registry = require("registry")
local routing = require("routing")
local modlog = require("modlog")
local observer = require("observer")
local sink = require("factories.sink")

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

local function deep_equal(a, b)
  if a == b then
    return true
  end
  if type(a) ~= "table" or type(b) ~= "table" then
    return false
  end
  for k, v in pairs(a) do
    if not deep_equal(v, b[k]) then
      return false
    end
  end
  for k in pairs(b) do
    if a[k] == nil then
      return false
    end
  end
  return true
end

local function noop() end
local function silent_log()
  return { info = noop, warn = noop, error = noop }
end

local function actor_spec(id, factory, params, routes)
  return { id = id, factory = factory, params = params or {}, routes = routes or {} }
end

-- ==================================================================
-- 1. modification log — ordered outcomes + deterministic replay
-- ==================================================================

do
  local inv = inventory.new({ log = silent_log() })
  local mlog = modlog.new({}) -- default no-op persist; in-memory log only
  local obs = observer.new({ inventory = inv, emit_event = noop, modlog = mlog })

  -- A scripted run mixing all three outcomes.
  local r1 = obs:apply({
    actors = {
      actor_spec("A", "worker", {}, { ["stub.Out"] = { "B" } }),
      actor_spec("B", "worker", {}, {}),
    },
    messages = { { to = "A", content = { kind = "seed" } } },
  })
  assert_true(r1.ok, "spawn A+B applies")

  local r2 = obs:apply({ actors = { actor_spec("A", "worker", {}, { ["stub.Out"] = { "B" } }) } })
  assert_true(r2.ok, "duplicate spawn A is a no-op, not a rejection")

  local r3 = obs:apply({ kills = { "B" } })
  assert_true(r3.ok, "kill B applies")

  local r4 = obs:apply({ messages = { { to = "ghost", content = {} } } })
  assert_true(not r4.ok, "message to a never-existed target is rejected")

  local r5 = obs:apply({ kills = { "B" } })
  assert_true(r5.ok, "kill-on-dead B is a no-op")

  -- The log is complete and ordered, one entry per apply, seqs monotone.
  local entries = mlog:all()
  assert_eq(mlog:count(), 5, "one modification-log entry per apply")
  local outcomes = {}
  for i, e in ipairs(entries) do
    assert_eq(e.seq, i, "entry seq is monotone from 1")
    outcomes[i] = e.outcome
  end
  assert_eq(outcomes[1], "applied", "spawn is applied")
  assert_eq(outcomes[2], "noop", "duplicate spawn is a no-op")
  assert_eq(outcomes[3], "applied", "kill is applied")
  assert_eq(outcomes[4], "rejected", "bad message is rejected")
  assert_eq(outcomes[5], "noop", "kill-on-dead is a no-op")

  -- Outcome payloads carry the flavor detail.
  assert_eq(entries[1].spawned[1], "A", "applied entry records the spawned ids")
  assert_eq(entries[2].noops[1].flavor, "duplicate-alive", "no-op entry records the spawn flavor")
  assert_eq(entries[3].killed[1], "B", "applied kill entry records the killed ids")
  assert_true(entries[4].error:find("unknown message target", 1, true) ~= nil,
    "rejected entry carries the validator error")
  assert_eq(entries[5].noops[1].flavor, "already-dead", "kill-on-dead no-op flavor recorded")

  -- Replay folds the log back into a fresh inventory; state must match live.
  local replayed = modlog.replay(entries, inventory.new({ log = silent_log() }))

  local function actor_ids(i)
    local ids = {}
    for id in i.pairs() do
      ids[id] = true
    end
    return ids
  end
  assert_true(deep_equal(actor_ids(inv), actor_ids(replayed)),
    "replayed inventory has the same actor set")

  for id in inv.pairs() do
    assert_eq(replayed.state_of(id), inv.state_of(id),
      "replayed lifecycle state matches live for " .. id)
    local live, rep = inv.get(id), replayed.get(id)
    assert_eq(rep.factory, live.factory, "replayed factory matches for " .. id)
    assert_true(deep_equal(rep.routes, live.routes), "replayed routes match for " .. id)
    assert_true(deep_equal(rep.mailbox, live.mailbox), "replayed mailbox matches for " .. id)
  end
end

-- ==================================================================
-- 2. lifecycle events — observed in order across fold + delivery
-- ==================================================================

do
  local events = {}
  local function emit_event(e)
    events[#events + 1] = e
  end

  local inv = inventory.new({ log = silent_log() })
  local reg = Registry.new()
  reg:register({
    declaration = {
      name = "worker",
      params = {},
      inputs = { input = "stub.In" },
      outputs = { "stub.Out" },
    },
    construct = function(id) return { id = id } end,
  })
  local router = routing.new({
    inventory = inv,
    registry = reg,
    log = silent_log(),
    events = emit_event,
  })
  inv.set_on_kill(function(id) router:forget(id) end)
  local obs = observer.new({ inventory = inv, emit_event = emit_event })

  -- Scripted run: start, spawn two, one becomes ready, the sink completes, kill.
  obs:run_started({ run_id = "run-1", run_name = "demo" })
  obs:apply({
    actors = {
      actor_spec("A", "worker", {}, { ["stub.Out"] = { "B" } }),
      actor_spec("B", "worker", {}, {}),
    },
  })

  -- The factory confirms A ready through its id-signed emitter (as it does
  -- when lazy construction builds it at first activation).
  local emit_a = router:emitter("A")
  emit_a({ kind = "mag.ready", from = "A" })

  -- The sink signals terminal completion through the delivery layer.
  local emit_sink = router:emitter("sink")
  emit_sink({ kind = "mag.RunComplete", from = "sink", result = { text = "done" }, persisted = true })

  obs:apply({ kills = { "B" } })

  local kinds = {}
  for i, e in ipairs(events) do
    kinds[i] = e.kind
  end
  local expected = {
    "mag.run_started",
    "mag.actor_spawned",
    "mag.actor_spawned",
    "mag.modification_applied",
    "mag.actor_ready",
    "mag.run_complete",
    "mag.actor_killed",
    "mag.modification_applied",
  }
  assert_eq(#kinds, #expected, "exactly the scripted lifecycle events fired")
  for i, k in ipairs(expected) do
    assert_eq(kinds[i], k, string.format("event %d is %s", i, k))
  end

  -- Field-level checks on the payloads.
  assert_eq(events[2].id, "A", "first spawn is A (mod.actors order)")
  assert_eq(events[3].id, "B", "second spawn is B")
  assert_eq(events[5].id, "A", "ready event names the confirmed id")
  assert_eq(events[6].result.text, "done", "run-complete carries the final result")
  assert_eq(events[6].persisted, true, "run-complete carries the persisted flag")
  assert_eq(events[7].id, "B", "kill event names the killed id")
end

-- ==================================================================
-- 3. output persistence — every actor's output, keyed by node id
-- ==================================================================

do
  -- Non-sink actor: its declared output persists through routing, keyed by id.
  local persisted = {}
  local inv = inventory.new({ log = silent_log() })
  local reg = Registry.new()
  reg:register({
    declaration = { name = "worker", params = {}, inputs = { input = "stub.In" }, outputs = { "stub.Out" } },
    construct = function(id) return { id = id } end,
  })
  local router = routing.new({
    inventory = inv,
    registry = reg,
    log = silent_log(),
    persist_output = function(node_id, output) persisted[node_id] = output end,
  })

  inv.apply({ actors = { actor_spec("W", "worker", {}, {}) } })
  local emit_w = router:emitter("W")
  emit_w({ kind = "stub.Out", from = "W", payload = "the-output" })

  assert_true(persisted["W"] ~= nil, "the actor's declared output landed, keyed by node id")
  assert_eq(persisted["W"].payload, "the-output", "the persisted output is the emitted message")

  -- Kernel-synthesized status types are NOT actor outputs and must not persist:
  -- apply_completion routes mag.Unit directly, bypassing the persistence seam.
  router:apply_completion("W", { status = "ok" })
  assert_eq(persisted["W"].payload, "the-output",
    "mag.Unit completion did not overwrite the per-node output file")

  -- Sink: its per-node file lands through the injected deps.writer (the sink
  -- declares no downstream output, so its 'output' is the final answer).
  local sink_persisted = {}
  local inst = sink.construct("sink", {}, noop, {
    writer = function(output) sink_persisted["sink"] = output end,
  })
  inst.deliver({
    shape = "single",
    messages = { { from = "up", tag = "generic-provider.FinalAnswer", message = { text = "final" } } },
  })
  assert_true(sink_persisted["sink"] ~= nil, "the sink's final output landed via deps.writer, keyed by id")
  assert_eq(sink_persisted["sink"].text, "final", "the sink persisted the final answer")
end

-- ==================================================================
-- 4. the initial modification is recorded in the modlog, and its seeds fire
-- through lazy construction inside the same observer-wrapped apply — one
-- composition point, modification #0 is modlogged like every later one
-- ==================================================================

do
  local inv = inventory.new({ log = silent_log() })
  local reg = Registry.new()
  local delivered = {}
  reg:register({
    declaration = { name = "entry", params = {}, inputs = { input = "seed.In" }, outputs = {} },
    construct = function(id, params, emit)
      local i = { id = id }
      i.deliver = function(activation)
        delivered[#delivered + 1] = activation.messages[1].message
        return "ok"
      end
      emit({ kind = "mag.ready", from = id })
      return i
    end,
  })
  local router = routing.new({ inventory = inv, registry = reg, log = silent_log() })
  inv.set_on_kill(function(id) router:forget(id) end)
  router:set_construct(function(record)
    return reg:construct(record.factory, record.id, record.params, router:emitter(record.id), {})
  end)
  inv.set_deliver(function(to, from, content)
    content = content or {}
    router:deliver(to, from, content.kind, content)
  end)

  local mlog = modlog.new({}) -- in-memory only
  local obs = observer.new({ inventory = inv, emit_event = noop, modlog = mlog })

  local mod = {
    actors = { actor_spec("entry", "entry", {}, {}) },
    messages = { { to = "entry", content = { kind = "seed.In", payload = "go" } } },
  }
  local res = obs:apply(mod)
  assert_true(res.ok, "the initial modification applies")
  assert_eq(#delivered, 1, "the seed constructed the entry actor and fired it")
  assert_eq(delivered[1].payload, "go", "seed payload delivered intact")

  assert_eq(mlog:count(), 1, "the initial modification is recorded (modification #0)")
  local entry = mlog:all()[1]
  assert_eq(entry.outcome, "applied", "the initial modification is logged as applied")
  assert_eq(entry.modification, mod, "the logged entry carries the initial modification")
  assert_eq(entry.spawned[1], "entry", "the entry records the spawned actor id")
end

print("mag-kernel observability_test: all cases passed")
