-- starter/reasoners/loop_counter_test.lua — unit tests for
-- `loop_counter.lua`. Driven from
-- `crates/nefor/tests/starter_loop_counter_reasoner_test.rs`.

local json = nefor.json

local loop_counter = require("reasoners.loop_counter")

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ",
      tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

local function dispatch(firing_id, args, prev_state)
  return loop_counter.handle({
    firing_id  = firing_id,
    args       = args,
    inputs     = {},
    prev_state = prev_state,
  })
end

local function find_tool_result_for(id)
  for _, c in ipairs(_test.calls()) do
    local d = json.decode(c.payload)
    if d and d.body and d.body.kind == "tool.result" and d.body.id == id then
      return d.body
    end
  end
  return nil
end

-- ------------------------------------------------------------------
-- single firing — count 1, not exceeded under limit 3
-- ------------------------------------------------------------------

do
  _test.calls_clear()
  local ret = dispatch("f1", { limit = 3, key = "retry-build" })
  assert_eq(ret, "_already_replied", "loop-counter handle reports synchronous reply")
  local r = find_tool_result_for("f1")
  assert_true(r ~= nil, "tool.result emitted for f1")
  assert_eq(r.result.count, 1, "first firing → count = 1")
  assert_eq(r.result.exceeded, false, "first firing → not exceeded under limit 3")
  assert_eq(r.result.limit, 3, "limit echoed back")
  assert_eq(r.result.key, "retry-build", "key echoed back")
  assert_eq(r.result.next_state.count, 1, "next_state.count carries forward")
end

-- ------------------------------------------------------------------
-- three firings — counts 1,2,3; never exceeded under limit 3
-- ------------------------------------------------------------------

do
  _test.calls_clear()
  -- Re-fire 1 (no prev_state).
  dispatch("g1", { limit = 3 }, nil)
  local r1 = find_tool_result_for("g1")
  assert_eq(r1.result.count, 1, "re-fire 1 → count 1")
  assert_eq(r1.result.exceeded, false, "re-fire 1 → not exceeded")

  _test.calls_clear()
  -- Re-fire 2 (prev_state from r1).
  dispatch("g2", { limit = 3 }, r1.result.next_state)
  local r2 = find_tool_result_for("g2")
  assert_eq(r2.result.count, 2, "re-fire 2 → count 2")
  assert_eq(r2.result.exceeded, false, "re-fire 2 → not exceeded")

  _test.calls_clear()
  -- Re-fire 3 (prev_state from r2).
  dispatch("g3", { limit = 3 }, r2.result.next_state)
  local r3 = find_tool_result_for("g3")
  assert_eq(r3.result.count, 3, "re-fire 3 → count 3 == limit, NOT exceeded yet")
  assert_eq(r3.result.exceeded, false, "count == limit → still allowed")
end

-- ------------------------------------------------------------------
-- fourth firing — exceeded
-- ------------------------------------------------------------------

do
  _test.calls_clear()
  dispatch("h4", { limit = 3 }, { count = 3 })
  local r = find_tool_result_for("h4")
  assert_eq(r.result.count, 4, "fourth firing → count 4")
  assert_eq(r.result.exceeded, true, "count > limit → exceeded")
end

-- ------------------------------------------------------------------
-- per-key independence — two counters with different keys in the same
-- graph don't share state. The reasoner-graph already gives each node
-- its own state cell, so the test we can run here is simpler: two
-- DIFFERENT firings whose prev_states are independent end up with the
-- counts that match each prev_state, regardless of key.
-- ------------------------------------------------------------------

do
  _test.calls_clear()
  -- Counter A: at count 2 going into firing.
  dispatch("kA", { limit = 5, key = "A" }, { count = 2 })
  local rA = find_tool_result_for("kA")
  assert_eq(rA.result.count, 3, "counter A advances to 3")
  assert_eq(rA.result.key, "A", "counter A's key preserved")

  _test.calls_clear()
  -- Counter B: at count 0 going into firing — independent of A.
  dispatch("kB", { limit = 5, key = "B" }, nil)
  local rB = find_tool_result_for("kB")
  assert_eq(rB.result.count, 1, "counter B starts at 1, independent of A")
  assert_eq(rB.result.key, "B", "counter B's key preserved")
end

-- ------------------------------------------------------------------
-- bad args — limit missing / non-positive
-- ------------------------------------------------------------------

do
  _test.calls_clear()
  local ret = dispatch("bad-1", { limit = 0 }, nil)
  assert_eq(ret, "_already_replied", "bad limit still synchronous reply")
  local r = find_tool_result_for("bad-1")
  assert_true(r ~= nil, "error tool.result emitted")
  assert_true(type(r.error) == "string" and #r.error > 0,
    "error message present")
  assert_eq(r.result, nil, "no result struct on bad args")
end

do
  _test.calls_clear()
  local ret = dispatch("bad-2", {}, nil)
  assert_eq(ret, "_already_replied", "missing limit still synchronous reply")
  local r = find_tool_result_for("bad-2")
  assert_true(r ~= nil, "error tool.result emitted for missing limit")
  assert_true(type(r.error) == "string" and #r.error > 0,
    "error message present for missing limit")
end
