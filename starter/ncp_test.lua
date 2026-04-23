-- starter/ncp_test.lua — unit tests for ncp.step semantics.
--
-- Loaded by `crates/nefor/tests/starter_ncp_test.rs`. The Rust test:
--   * Installs a mock `nefor.engine` that records every `send` call and
--     returns a controllable plugin list from `plugins()`.
--   * Sets `package.path` so `require("ncp")` and `require("lib.json")`
--     resolve from this directory.
--   * Loads and runs this file; any `assert` failure surfaces as a Lua
--     error, which fails the Rust test.
--
-- Test helpers (`assert_eq`, `entry_plugin`, `entry_step`, `make_ready`,
-- `make_event`) are defined below and kept local to this file — the real
-- NCP module has no dependency on them.

local json = require("lib.json")
local ncp = require("ncp")

-- Every test begins by clearing module state and the mock's recorded
-- calls, so tests are order-independent.
local function reset()
  ncp._reset()
  _test.calls_clear()
  _test.set_plugins({})
end

-- Equality assertion with a message showing both values.
local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ",
      tostring(expected),
      tostring(actual)
    ))
  end
end

local function assert_true(cond, msg)
  if not cond then
    error("assertion failed: " .. (msg or "condition was false"))
  end
end

-- Log-entry builders. Tests never construct entries by hand.
local function entry_plugin(origin, payload)
  return { ts = "2026-04-23T00:00:00.000Z", origin = origin, target = nil, payload = payload }
end

local function entry_step(target, payload)
  return { ts = "2026-04-23T00:00:00.000Z", origin = "step", target = target, payload = payload }
end

-- NCP envelope builders.
local function make_ready(version)
  return json.encode({
    type = "system",
    body = { kind = "ready", protocol_version = version },
  })
end

local function make_event(body)
  return json.encode({ type = "event", body = body })
end

-- Convenience: run step with a single inbound entry appended.
local function step_with(origin, payload)
  local entry = entry_plugin(origin, payload)
  ncp.step({}, { entry })
end

-- ------------------------------------------------------------------
-- 1. ready triggers ready_ok reply
-- ------------------------------------------------------------------
local function test_ready_triggers_ready_ok_reply()
  reset()
  _test.set_plugins({ "mock-plugin" })
  step_with("mock-plugin", make_ready("0.1"))

  local calls = _test.calls()
  assert_eq(#calls, 1, "exactly one send on ready")
  assert_eq(calls[1].target, "mock-plugin", "reply targeted at readying plugin")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.type, "system", "system message")
  assert_eq(decoded.body.kind, "ready_ok", "kind=ready_ok")
  assert_true(type(decoded.body.engine_version) == "string",
    "engine_version present and a string")
end

-- ------------------------------------------------------------------
-- 2. wrong version triggers protocol_version_mismatch error
-- ------------------------------------------------------------------
local function test_ready_with_wrong_version_triggers_error()
  reset()
  _test.set_plugins({ "p" })
  step_with("p", make_ready("0.9"))

  local calls = _test.calls()
  assert_eq(#calls, 1, "one send for error")
  assert_eq(calls[1].target, "p", "error targeted at sender")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.body.kind, "error", "error kind")
  assert_eq(decoded.body.code, "protocol_version_mismatch", "correct code")
end

-- ------------------------------------------------------------------
-- 3. malformed ready body triggers invalid_ready
-- ------------------------------------------------------------------
local function test_malformed_ready_body_triggers_error()
  reset()
  _test.set_plugins({ "p" })
  -- Missing protocol_version field.
  local bad = json.encode({ type = "system", body = { kind = "ready" } })
  step_with("p", bad)

  local calls = _test.calls()
  assert_eq(#calls, 1, "one error")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.body.code, "invalid_ready", "invalid_ready")
end

-- ------------------------------------------------------------------
-- 4. event from ready plugin broadcasts to other ready plugins
-- ------------------------------------------------------------------
local function test_event_from_ready_plugin_broadcasts_to_others()
  reset()
  _test.set_plugins({ "a", "b", "c" })

  -- Ready all three.
  local log = {}
  for _, name in ipairs({ "a", "b", "c" }) do
    log[#log + 1] = entry_plugin(name, make_ready("0.1"))
    ncp.step({}, log)
  end
  _test.calls_clear()

  -- 'a' emits an event. Should reach b and c only.
  local ev = make_event({ kind = "test.ping" })
  log[#log + 1] = entry_plugin("a", ev)
  ncp.step({}, log)

  local calls = _test.calls()
  local seen = { a = false, b = false, c = false }
  for _, c in ipairs(calls) do
    if c.target and seen[c.target] ~= nil then
      seen[c.target] = true
    end
  end
  assert_eq(seen.b, true, "b received event")
  assert_eq(seen.c, true, "c received event")
end

-- ------------------------------------------------------------------
-- 5. event from ready plugin excludes the sender
-- ------------------------------------------------------------------
local function test_event_from_ready_plugin_excludes_sender()
  reset()
  _test.set_plugins({ "a", "b" })

  local log = {}
  log[#log + 1] = entry_plugin("a", make_ready("0.1"))
  ncp.step({}, log)
  log[#log + 1] = entry_plugin("b", make_ready("0.1"))
  ncp.step({}, log)
  _test.calls_clear()

  local ev = make_event({ kind = "sub" })
  log[#log + 1] = entry_plugin("a", ev)
  ncp.step({}, log)

  local calls = _test.calls()
  for _, c in ipairs(calls) do
    assert_true(c.target ~= "a", "sender 'a' must not receive its own event")
  end
end

-- ------------------------------------------------------------------
-- 6. event from non-ready plugin is errored
-- ------------------------------------------------------------------
local function test_event_from_non_ready_plugin_is_errored()
  reset()
  _test.set_plugins({ "a", "b" })

  -- 'a' emits an event without readying first.
  local log = { entry_plugin("a", make_event({ kind = "x" })) }
  ncp.step({}, log)

  local calls = _test.calls()
  assert_eq(#calls, 1, "one send: the error reply")
  assert_eq(calls[1].target, "a", "error targeted at offender")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.body.kind, "error", "error")
  assert_eq(decoded.body.code, "malformed_envelope", "malformed_envelope code")
end

-- ------------------------------------------------------------------
-- 7. malformed JSON triggers malformed_envelope error
-- ------------------------------------------------------------------
local function test_malformed_json_triggers_error()
  reset()
  _test.set_plugins({ "p" })
  step_with("p", "{not valid json")

  local calls = _test.calls()
  assert_eq(#calls, 1, "one send: error")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.body.code, "malformed_envelope", "malformed_envelope")
end

-- ------------------------------------------------------------------
-- 8. second ready from same plugin: documented as `invalid_ready`
-- ------------------------------------------------------------------
--
-- Policy: the spec defines `ready` as "first message after connecting".
-- A second ready is an implementation bug on the plugin side; we surface
-- it as `invalid_ready` and do not re-replay. This is *not* idempotent —
-- the plugin sees a clear error code.
local function test_second_ready_from_same_plugin_errors()
  reset()
  _test.set_plugins({ "p" })
  step_with("p", make_ready("0.1"))
  _test.calls_clear()

  -- Second ready — still just one log entry from the test's point of
  -- view because we pass the tail only; step looks at current_log[#current_log].
  local log = {
    entry_plugin("p", make_ready("0.1")),
    entry_plugin("p", make_ready("0.1")),  -- the second ready
  }
  ncp.step({}, log)

  local calls = _test.calls()
  assert_eq(#calls, 1, "one send: the error")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.body.code, "invalid_ready", "invalid_ready code")
end

-- ------------------------------------------------------------------
-- 9. late attacher receives prior events in order
-- ------------------------------------------------------------------
local function test_late_attacher_receives_prior_events_in_order()
  reset()
  _test.set_plugins({ "a" })

  -- 'a' readies, then emits three events.
  local log = { entry_plugin("a", make_ready("0.1")) }
  ncp.step({}, log)

  for _, k in ipairs({ "e1", "e2", "e3" }) do
    log[#log + 1] = entry_plugin("a", make_event({ kind = k }))
    -- Simulate step broadcasting: add one step entry per event for each
    -- connected-but-not-a peer. 'a' is the only ready plugin so no broadcasts
    -- actually happen; the entry is appended by ncp.step's own broadcast
    -- logic (via the mock `send`). We mirror that here to keep the log
    -- realistic: current_log in production contains both the inbound and
    -- step's outbound fanout.
    ncp.step({}, log)
  end

  -- 'b' joins and readies. Expect three replayed events in order.
  _test.set_plugins({ "a", "b" })
  _test.calls_clear()
  log[#log + 1] = entry_plugin("b", make_ready("0.1"))
  ncp.step({}, log)

  local calls = _test.calls()
  -- First call is the ready_ok reply; subsequent calls are the replayed
  -- events (3 of them).
  local replayed = {}
  for _, c in ipairs(calls) do
    if c.target == "b" then
      local d = json.decode(c.payload)
      if d.type == "event" then
        replayed[#replayed + 1] = d.body.kind
      end
    end
  end
  assert_eq(#replayed, 3, "three events replayed")
  assert_eq(replayed[1], "e1", "first is e1")
  assert_eq(replayed[2], "e2", "second is e2")
  assert_eq(replayed[3], "e3", "third is e3")
end

-- ------------------------------------------------------------------
-- 10. saved_log is not replayed in v1 (explicit documentation test)
-- ------------------------------------------------------------------
local function test_saved_log_is_not_replayed_in_v1()
  reset()
  _test.set_plugins({ "p" })

  -- Saved log from a parent session has prior events. We should not
  -- resend them to 'p' on its ready — only current-session events count.
  local saved = {
    entry_plugin("prior", make_event({ kind = "from-past-life" })),
  }
  local current = { entry_plugin("p", make_ready("0.1")) }
  ncp.step(saved, current)

  local calls = _test.calls()
  -- Exactly one send: the ready_ok. No replays from saved_log.
  assert_eq(#calls, 1, "no saved-log replay; only ready_ok")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.body.kind, "ready_ok", "ready_ok only")
end

-- ------------------------------------------------------------------
-- driver
-- ------------------------------------------------------------------

local tests = {
  { name = "ready_triggers_ready_ok_reply", fn = test_ready_triggers_ready_ok_reply },
  { name = "ready_with_wrong_version_triggers_error", fn = test_ready_with_wrong_version_triggers_error },
  { name = "malformed_ready_body_triggers_error", fn = test_malformed_ready_body_triggers_error },
  { name = "event_from_ready_plugin_broadcasts_to_others", fn = test_event_from_ready_plugin_broadcasts_to_others },
  { name = "event_from_ready_plugin_excludes_sender", fn = test_event_from_ready_plugin_excludes_sender },
  { name = "event_from_non_ready_plugin_is_errored", fn = test_event_from_non_ready_plugin_is_errored },
  { name = "malformed_json_triggers_error", fn = test_malformed_json_triggers_error },
  { name = "second_ready_from_same_plugin_errors", fn = test_second_ready_from_same_plugin_errors },
  { name = "late_attacher_receives_prior_events_in_order", fn = test_late_attacher_receives_prior_events_in_order },
  { name = "saved_log_is_not_replayed_in_v1", fn = test_saved_log_is_not_replayed_in_v1 },
}

for _, t in ipairs(tests) do
  local ok, err = pcall(t.fn)
  if not ok then
    error("test '" .. t.name .. "' FAILED:\n" .. tostring(err), 0)
  end
end
