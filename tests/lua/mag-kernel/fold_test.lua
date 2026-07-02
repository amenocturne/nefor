-- tests/lua/mag-kernel/fold_test.lua — unit tests for the mag-kernel fold
-- (starter/mag-kernel/inventory.lua). Driven from
-- engine/tests/starter_mag_kernel_test.rs.
--
-- Exercises the fold over scripted modification sequences: normal apply,
-- duplicate spawn (identical + different spec), kill-on-dead, unknown
-- message-target rejection, id-collision rejection, non-empty rules
-- rejection, plus lifecycle monotonicity and route retention.

local inventory = require("inventory")

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

-- A capturing logger so tests can assert the level a no-op was logged at.
local function new_logger()
  local rec = { info = {}, warn = {}, error = {} }
  local function sink(bucket)
    return function(m) bucket[#bucket + 1] = m end
  end
  local log = { info = sink(rec.info), warn = sink(rec.warn), error = sink(rec.error) }
  return log, rec
end

local function new_inv()
  local log, rec = new_logger()
  return inventory.new({ log = log }), rec
end

local function actor_spec(id, factory, params, routes)
  return { id = id, factory = factory, params = params or {}, routes = routes or {} }
end

-- ------------------------------------------------------------------
-- normal apply — spawns register alive, routes retained, message queued
-- ------------------------------------------------------------------

do
  local inv = new_inv()
  local res = inv.apply({
    actors = {
      actor_spec("docs-explorer.entry", "adapter", { seed = "provider-in" },
        { ["generic-provider.ProviderOut"] = { "docs-explorer.llm" } }),
      actor_spec("docs-explorer.llm", "llm", { model = "opus" }, {}),
    },
    messages = {
      { to = "docs-explorer.entry", content = { kind = "task", prompt = "go" } },
    },
    kills = {},
    rules = {},
  })

  assert_true(res.ok, "normal modification applies")
  assert_eq(inv.state_of("docs-explorer.entry"), "alive", "entry alive")
  assert_eq(inv.state_of("docs-explorer.llm"), "alive", "llm alive")
  assert_eq(inv.state_of("nobody"), "never-existed", "unseen id is never-existed")

  local entry = inv.get("docs-explorer.entry")
  assert_eq(entry.factory, "adapter", "factory retained")
  assert_eq(entry.routes["generic-provider.ProviderOut"][1], "docs-explorer.llm",
    "routes retained verbatim, kernel-side")
  assert_eq(#entry.mailbox, 1, "message queued in pending mailbox")
  assert_eq(entry.mailbox[1].prompt, "go", "queued message content preserved")
end

-- ------------------------------------------------------------------
-- duplicate spawn — identical spec = info no-op; different spec = warn
-- ------------------------------------------------------------------

do
  local inv, rec = new_inv()
  inv.apply({ actors = { actor_spec("a", "llm", { model = "opus" }, {}) } })

  -- identical spec re-spawn → info no-op, still alive, spec unchanged
  local res = inv.apply({ actors = { actor_spec("a", "llm", { model = "opus" }, {}) } })
  assert_true(res.ok, "identical duplicate spawn is not a rejection")
  assert_eq(inv.state_of("a"), "alive", "identical duplicate leaves a alive")
  assert_eq(#rec.info, 1, "identical duplicate logged at info")
  assert_eq(#rec.warn, 0, "identical duplicate did not warn")
  assert_contains(rec.info[1], "identical spec", "info names the identical-spec case")
end

do
  local inv, rec = new_inv()
  inv.apply({ actors = { actor_spec("a", "llm", { model = "opus" }, {}) } })

  -- different spec re-spawn → warn no-op, original spec retained (ignored)
  local res = inv.apply({ actors = { actor_spec("a", "llm", { model = "haiku" }, {}) } })
  assert_true(res.ok, "different-spec duplicate spawn is not a rejection")
  assert_eq(inv.state_of("a"), "alive", "different-spec duplicate leaves a alive")
  assert_eq(inv.get("a").params.model, "opus", "original spec kept; duplicate ignored")
  assert_eq(#rec.warn, 1, "different-spec duplicate logged at warn")
  assert_eq(#rec.info, 0, "different-spec duplicate did not info")
end

-- ------------------------------------------------------------------
-- kill, then kill-on-dead — monotone, second kill is an info no-op
-- ------------------------------------------------------------------

do
  local inv, rec = new_inv()
  inv.apply({ actors = { actor_spec("a", "llm", {}, {}) } })

  local k1 = inv.apply({ kills = { "a" } })
  assert_true(k1.ok, "kill applies")
  assert_eq(inv.state_of("a"), "dead", "killed actor is dead")

  local k2 = inv.apply({ kills = { "a" } })
  assert_true(k2.ok, "kill-on-dead is not a rejection")
  assert_eq(inv.state_of("a"), "dead", "kill-on-dead leaves a dead")
  assert_eq(#rec.info, 1, "kill-on-dead logged at info")
  assert_contains(rec.info[1], "already dead", "info names the dead case")
end

-- ------------------------------------------------------------------
-- monotone: spawn on a dead id cannot revive it (warn no-op)
-- ------------------------------------------------------------------

do
  local inv, rec = new_inv()
  inv.apply({ actors = { actor_spec("a", "llm", {}, {}) } })
  inv.apply({ kills = { "a" } })

  local res = inv.apply({ actors = { actor_spec("a", "llm", {}, {}) } })
  assert_true(res.ok, "spawn-on-dead is a no-op, not a rejection")
  assert_eq(inv.state_of("a"), "dead", "spawn cannot revive a dead id")
  assert_eq(#rec.warn, 1, "spawn-on-dead logged at warn")
end

-- ------------------------------------------------------------------
-- message target created within the same modification is valid
-- ------------------------------------------------------------------

do
  local inv = new_inv()
  local res = inv.apply({
    actors = { actor_spec("b", "llm", {}, {}) },
    messages = { { to = "b", content = { kind = "seed" } } },
  })
  assert_true(res.ok, "message to an id spawned in the same modification is valid")
  assert_eq(#inv.get("b").mailbox, 1, "message queued for same-modification spawn")
end

-- ------------------------------------------------------------------
-- unknown message target — rejected, inventory untouched, run continues
-- ------------------------------------------------------------------

do
  local inv, rec = new_inv()
  inv.apply({ actors = { actor_spec("a", "llm", {}, {}) } })

  local res = inv.apply({ messages = { { to = "ghost", content = {} } } })
  assert_true(not res.ok, "message to an unknown target is rejected")
  assert_contains(res.error, "unknown message target", "error names the unknown target")
  assert_eq(inv.state_of("ghost"), "never-existed", "rejected modification did not mutate")
  assert_true(#rec.error >= 1, "rejection logged at error")

  -- run continues: a subsequent valid modification still applies
  local ok = inv.apply({ actors = { actor_spec("c", "llm", {}, {}) } })
  assert_true(ok.ok, "run continues after a rejection")
end

-- ------------------------------------------------------------------
-- message to a dead target — dropped as a logged no-op, not rejected
-- (race artifact of first-applied-wins: the sender computed the send
-- while the target lived; docs/ir.md)
-- ------------------------------------------------------------------

do
  local inv, rec = new_inv()
  inv.apply({ actors = { actor_spec("a", "llm", {}, {}), actor_spec("b", "llm", {}, {}) } })
  inv.apply({ kills = { "a" } })

  local res = inv.apply({ messages = {
    { to = "a", content = {} },
    { to = "b", content = { note = "sibling delivery" } },
  } })
  assert_true(res.ok, "modification with a dead-target send still applies")
  assert_eq(#inv.get("b").mailbox, 1, "sibling send in the same modification delivered")
  local dropped = false
  for _, m in ipairs(rec.info) do
    if m:find("send dropped", 1, true) then dropped = true end
  end
  assert_true(dropped, "dead-target send logged as dropped at info")
end

-- ------------------------------------------------------------------
-- id collision within one modification — rejected
-- ------------------------------------------------------------------

do
  local inv = new_inv()
  local res = inv.apply({
    actors = {
      actor_spec("dup", "llm", {}, {}),
      actor_spec("dup", "adapter", {}, {}),
    },
  })
  assert_true(not res.ok, "same id twice in one modification is rejected")
  assert_contains(res.error, "id collision", "error names the collision")
  assert_eq(inv.state_of("dup"), "never-existed", "rejected modification spawned nothing")
end

-- ------------------------------------------------------------------
-- non-empty rules — rejected "not implemented"
-- ------------------------------------------------------------------

do
  local inv = new_inv()
  local res = inv.apply({
    actors = { actor_spec("a", "llm", {}, {}) },
    rules = { { on = "a", fn = "handle" } },
  })
  assert_true(not res.ok, "non-empty rules are rejected")
  assert_contains(res.error, "not implemented", "rules rejection says not implemented")
  assert_eq(inv.state_of("a"), "never-existed", "rules rejection applied nothing")
end

-- an empty rules list is accepted (the static-graph case)
do
  local inv = new_inv()
  local res = inv.apply({ actors = { actor_spec("a", "llm", {}, {}) }, rules = {} })
  assert_true(res.ok, "empty rules list is accepted")
end

-- ------------------------------------------------------------------
-- malformed shape — rejected
-- ------------------------------------------------------------------

do
  local inv = new_inv()
  local res = inv.apply({ actors = { { id = "x" } } })
  assert_true(not res.ok, "actor missing factory is rejected")
  assert_contains(res.error, "factory", "error names the missing factory")

  local res2 = inv.apply({ actors = { actor_spec("y", "llm", {},
    { ["T"] = { "ok", 7 } }) } })
  assert_true(not res2.ok, "non-string route destination is rejected")
end

print("mag-kernel fold_test: all cases passed")
