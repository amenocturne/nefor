-- lua/core/actor_test.lua — unit tests for the generic
-- identity-passthrough actor spec helper in core.actor.
--
-- Loaded by `crates/nefor/tests/core_actor_test.rs` against a mock
-- `nefor.engine` surface that records send/deliver calls into a shared
-- `_test.calls()` buffer.

local json = nefor.json
local actor = require("core.actor")
local event = require("core.event")

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ",
      tostring(expected),
      tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then
    error("assertion failed: " .. (msg or "condition was false"), 2)
  end
end

-- ----------------------------------------------------------------
-- identity_spec preserves name and command
-- ----------------------------------------------------------------
do
  local spec = actor.identity_spec("some-plugin", { "/path/to/bin", "--flag" })
  assert_eq(spec.name, "some-plugin", "spec.name")
  assert_eq(spec.command[1], "/path/to/bin", "spec.command[1]")
  assert_eq(spec.command[2], "--flag", "spec.command[2]")
  assert_eq(type(spec.from_plugin), "function", "from_plugin is fn")
  assert_eq(type(spec.to_plugin), "function", "to_plugin is fn")
  assert_eq(type(spec.receive_msg), "function", "receive_msg is fn")
end

-- ----------------------------------------------------------------
-- event.decode accepts only event-shaped payloads with table body+kind
-- ----------------------------------------------------------------
do
  local evt, err = event.decode({
    payload = json.encode({ type = "event", body = { kind = "tool.result", id = "x" } }),
  })
  assert_true(evt ~= nil, "valid event decodes: " .. tostring(err))
  assert_eq(evt.kind, "tool.result", "decoded kind")
  assert_eq(evt.body.id, "x", "decoded body preserved")
end

do
  local evt = event.decode({
    payload = json.encode({ type = "event", body = "not a body" }),
  })
  assert_eq(evt, nil, "non-table body rejected")
end

do
  local evt = event.decode({
    payload = json.encode({ type = "event", body = { id = "missing kind" } }),
  })
  assert_eq(evt, nil, "missing body.kind rejected")
end

-- ----------------------------------------------------------------
-- from_plugin publishes every envelope verbatim (broadcast)
-- ----------------------------------------------------------------
do
  _test.calls_clear()
  local spec = actor.identity_spec("p", { "x" })
  spec.from_plugin({
    { type = "event", from = "p", body = { kind = "k1" } },
    { type = "event", from = "p", body = { kind = "k2" } },
  })
  local calls = _test.calls()
  assert_eq(#calls, 2, "publishes one envelope per input")
  for _, c in ipairs(calls) do
    assert_eq(c.target, nil, "broadcast (target nil)")
    local env = json.decode(c.payload)
    assert_eq(env.type, "event", "type preserved")
    assert_eq(env.from, "p", "from preserved")
  end
end

-- ----------------------------------------------------------------
-- from_plugin defaults env.from to spec name when missing
-- ----------------------------------------------------------------
do
  _test.calls_clear()
  local spec = actor.identity_spec("p", { "x" })
  spec.from_plugin({
    { type = "event", body = { kind = "k" } }, -- no `from`
  })
  local calls = _test.calls()
  assert_eq(#calls, 1, "one publish")
  local env = json.decode(calls[1].payload)
  assert_eq(env.from, "p", "from defaults to spec name when missing")
end

-- ----------------------------------------------------------------
-- from_plugin skips envelopes with non-table body
-- ----------------------------------------------------------------
do
  _test.calls_clear()
  local spec = actor.identity_spec("p", { "x" })
  spec.from_plugin({
    { type = "event", from = "p", body = nil },
    { type = "event", from = "p", body = "not a table" },
  })
  assert_eq(#_test.calls(), 0, "non-table bodies are skipped")
end

-- ----------------------------------------------------------------
-- to_plugin: skips env.replay
-- ----------------------------------------------------------------
do
  _test.calls_clear()
  local spec = actor.identity_spec("p", { "x" })
  spec.to_plugin({
    { type = "event", from = "other", ts = "t1", replay = true,
      body = { kind = "anything" } },
  })
  assert_eq(#_test.calls(), 0, "replay envelopes are skipped")
end

-- ----------------------------------------------------------------
-- to_plugin: skips self-emissions
-- ----------------------------------------------------------------
do
  _test.calls_clear()
  local spec = actor.identity_spec("p", { "x" })
  spec.to_plugin({
    { type = "event", from = "p", ts = "t1", body = { kind = "echo" } },
  })
  assert_eq(#_test.calls(), 0, "self-emissions are skipped")
end

-- ----------------------------------------------------------------
-- to_plugin: delivers normal envelopes, strips framework-only fields
-- ----------------------------------------------------------------
do
  _test.calls_clear()
  local spec = actor.identity_spec("p", { "x" })
  spec.to_plugin({
    { type = "event", from = "peer", ts = "t1",
      body = { kind = "tool.invoke" } },
  })
  local calls = _test.calls()
  assert_eq(#calls, 1, "one delivery")
  assert_eq(calls[1].target, "p", "delivered to p")
  local env = json.decode(calls[1].payload)
  assert_eq(env.from, "peer", "from preserved")
  assert_eq(env.replay, nil, "replay field stripped (would not exist anyway)")
  assert_eq(env.body.kind, "tool.invoke", "body preserved")
end

-- ----------------------------------------------------------------
-- argument validation
-- ----------------------------------------------------------------
do
  local ok = pcall(actor.identity_spec, nil, { "x" })
  assert_true(not ok, "rejects nil name")

  ok = pcall(actor.identity_spec, "", { "x" })
  assert_true(not ok, "rejects empty name")

  ok = pcall(actor.identity_spec, "p", nil)
  assert_true(not ok, "rejects nil command")

  ok = pcall(actor.identity_spec, "p", "not a table")
  assert_true(not ok, "rejects non-table command")
end
