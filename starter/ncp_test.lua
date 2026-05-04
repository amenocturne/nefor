-- starter/ncp_test.lua — unit tests for ncp.dispatch semantics.
--
-- Loaded by `crates/nefor/tests/starter_ncp_test.rs`. The Rust test:
--   * Installs a mock `nefor.engine` that records every `send` call and
--     returns a controllable plugin list from `plugins()`.
--   * Sets `package.path` so `require("ncp")` resolves from this directory.
--   * Loads and runs this file; any `assert` failure surfaces as a Lua
--     error, which fails the Rust test.
--
-- Test helpers (`assert_eq`, `entry_plugin`, `entry_step`, `make_ready`,
-- `make_event`) are defined below and kept local to this file — the real
-- NCP module has no dependency on them.

local json = nefor.json
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

-- Convenience: run dispatch with a single inbound entry appended.
local function dispatch_with(origin, payload)
  local entry = entry_plugin(origin, payload)
  ncp.dispatch({ entry })
end

-- ------------------------------------------------------------------
-- 1. ready triggers ready_ok reply
-- ------------------------------------------------------------------
local function test_ready_triggers_ready_ok_reply()
  reset()
  _test.set_plugins({ "mock-plugin" })
  dispatch_with("mock-plugin", make_ready("0.1"))

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
  dispatch_with("p", make_ready("0.9"))

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
  dispatch_with("p", bad)

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
    ncp.dispatch(log)
  end
  _test.calls_clear()

  -- 'a' emits an event. Should reach b and c only.
  local ev = make_event({ kind = "test.ping" })
  log[#log + 1] = entry_plugin("a", ev)
  ncp.dispatch(log)

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
  ncp.dispatch(log)
  log[#log + 1] = entry_plugin("b", make_ready("0.1"))
  ncp.dispatch(log)
  _test.calls_clear()

  local ev = make_event({ kind = "sub" })
  log[#log + 1] = entry_plugin("a", ev)
  ncp.dispatch(log)

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
  ncp.dispatch(log)

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
  dispatch_with("p", "{not valid json")

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
  dispatch_with("p", make_ready("0.1"))
  _test.calls_clear()

  -- Second ready — still just one log entry from the test's point of
  -- view because we pass the tail only; step looks at current_log[#current_log].
  local log = {
    entry_plugin("p", make_ready("0.1")),
    entry_plugin("p", make_ready("0.1")),  -- the second ready
  }
  ncp.dispatch(log)

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
  ncp.dispatch(log)

  for _, k in ipairs({ "e1", "e2", "e3" }) do
    log[#log + 1] = entry_plugin("a", make_event({ kind = k }))
    -- Simulate step broadcasting: add one step entry per event for each
    -- connected-but-not-a peer. 'a' is the only ready plugin so no broadcasts
    -- actually happen; the entry is appended by ncp.dispatch's own broadcast
    -- logic (via the mock `send`). We mirror that here to keep the log
    -- realistic: current_log in production contains both the inbound and
    -- step's outbound fanout.
    ncp.dispatch(log)
  end

  -- 'b' joins and readies. Expect three replayed events in order.
  _test.set_plugins({ "a", "b" })
  _test.calls_clear()
  log[#log + 1] = entry_plugin("b", make_ready("0.1"))
  ncp.dispatch(log)

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
-- transforms: from_plugin rewrites event before broadcast
-- ------------------------------------------------------------------
-- Helper: ready each name in order, calling step after every append so
-- ncp.dispatch sees one new tail entry per call (the production pattern).
local function ready_in_order(log, names)
  for _, n in ipairs(names) do
    log[#log + 1] = entry_plugin(n, make_ready("0.1"))
    ncp.dispatch(log)
  end
end

local function test_from_plugin_transform_rewrites_event_kind()
  reset()
  _test.set_plugins({ "src", "dst" })

  -- src has a from_plugin transform that rewrites cc.* → chat.*.
  ncp._test_set_transforms("src", {
    from_plugin = function(env)
      if env.body and type(env.body.kind) == "string" then
        local k = env.body.kind
        if k:sub(1, 3) == "cc." then
          env.body.kind = "chat." .. k:sub(4)
        end
      end
      return env
    end,
  })

  local log = {}
  ready_in_order(log, { "src", "dst" })
  _test.calls_clear()

  log[#log + 1] = entry_plugin("src", make_event({ kind = "cc.stream.end", text = "hi" }))
  ncp.dispatch(log)

  local calls = _test.calls()
  assert_eq(#calls, 1, "exactly one peer (dst) received the event")
  assert_eq(calls[1].target, "dst", "delivered to dst")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.body.kind, "chat.stream.end", "kind was rewritten by from_plugin")
  assert_eq(decoded.body.text, "hi", "body fields preserved")
  assert_eq(decoded.from, "src", "from preserved as origin plugin")
end

-- ------------------------------------------------------------------
-- transforms: from_plugin returning nil drops the envelope entirely
-- ------------------------------------------------------------------
local function test_from_plugin_transform_returning_nil_drops_envelope()
  reset()
  _test.set_plugins({ "src", "dst" })

  ncp._test_set_transforms("src", {
    from_plugin = function(_env) return nil end,
  })

  local log = {}
  ready_in_order(log, { "src", "dst" })
  _test.calls_clear()

  log[#log + 1] = entry_plugin("src", make_event({ kind = "any" }))
  ncp.dispatch(log)

  assert_eq(#_test.calls(), 0, "no peers received the dropped event")
end

-- ------------------------------------------------------------------
-- transforms: to_plugin rewrites only for that target
-- ------------------------------------------------------------------
local function test_to_plugin_transform_rewrites_per_target_only()
  reset()
  _test.set_plugins({ "src", "a", "b" })

  -- Only 'a' has a to_plugin transform; 'b' should see the unrewritten event.
  ncp._test_set_transforms("a", {
    to_plugin = function(env)
      env.body.kind = "rewritten"
      return env
    end,
  })

  local log = {}
  ready_in_order(log, { "src", "a", "b" })
  _test.calls_clear()

  log[#log + 1] = entry_plugin("src", make_event({ kind = "original" }))
  ncp.dispatch(log)

  local seen = {}
  for _, c in ipairs(_test.calls()) do
    local d = json.decode(c.payload)
    seen[c.target] = d.body.kind
  end
  assert_eq(seen.a, "rewritten", "'a' saw rewritten event via to_plugin")
  assert_eq(seen.b, "original",  "'b' saw original event (no transform)")
end

-- ------------------------------------------------------------------
-- transforms: to_plugin returning nil drops for that target only
-- ------------------------------------------------------------------
local function test_to_plugin_transform_returning_nil_drops_for_target_only()
  reset()
  _test.set_plugins({ "src", "a", "b" })

  ncp._test_set_transforms("a", {
    to_plugin = function(_env) return nil end,
  })

  local log = {}
  ready_in_order(log, { "src", "a", "b" })
  _test.calls_clear()

  log[#log + 1] = entry_plugin("src", make_event({ kind = "x" }))
  ncp.dispatch(log)

  local targets = {}
  for _, c in ipairs(_test.calls()) do
    targets[c.target] = (targets[c.target] or 0) + 1
  end
  assert_eq(targets.a or 0, 0, "'a' was filtered by its to_plugin")
  assert_eq(targets.b or 0, 1, "'b' still received the event")
end

-- ------------------------------------------------------------------
-- transforms: errors in from_plugin emit transform_error and drop
-- ------------------------------------------------------------------
local function test_from_plugin_transform_error_emits_transform_error()
  reset()
  _test.set_plugins({ "src", "dst" })

  ncp._test_set_transforms("src", {
    from_plugin = function(_env) error("boom") end,
  })

  local log = {}
  ready_in_order(log, { "src", "dst" })
  _test.calls_clear()

  log[#log + 1] = entry_plugin("src", make_event({ kind = "x" }))
  ncp.dispatch(log)

  local calls = _test.calls()
  assert_eq(#calls, 1, "one send: the error reply to source")
  assert_eq(calls[1].target, "src", "error targeted at the source plugin")
  local d = json.decode(calls[1].payload)
  assert_eq(d.body.kind, "error", "error envelope")
  assert_eq(d.body.code, "transform_error", "transform_error code")
end

-- ------------------------------------------------------------------
-- transforms: replayed events also pass through transforms
-- ------------------------------------------------------------------
local function test_replayed_events_pass_through_from_plugin_transform()
  reset()
  _test.set_plugins({ "src" })

  ncp._test_set_transforms("src", {
    from_plugin = function(env)
      env.body.kind = "rewritten"
      return env
    end,
  })

  -- src readies, then emits two events while alone on the bus.
  local log = { entry_plugin("src", make_ready("0.1")) }
  ncp.dispatch(log)
  for _, k in ipairs({ "e1", "e2" }) do
    log[#log + 1] = entry_plugin("src", make_event({ kind = k }))
    ncp.dispatch(log)
  end

  -- 'late' joins. Replay should deliver both events with rewritten kind.
  _test.set_plugins({ "src", "late" })
  _test.calls_clear()
  log[#log + 1] = entry_plugin("late", make_ready("0.1"))
  ncp.dispatch(log)

  local replayed_kinds = {}
  for _, c in ipairs(_test.calls()) do
    if c.target == "late" then
      local d = json.decode(c.payload)
      if d.type == "event" then
        replayed_kinds[#replayed_kinds + 1] = d.body.kind
      end
    end
  end
  assert_eq(#replayed_kinds, 2, "both prior events replayed")
  assert_eq(replayed_kinds[1], "rewritten", "first replay used from_plugin")
  assert_eq(replayed_kinds[2], "rewritten", "second replay used from_plugin")
end


-- ------------------------------------------------------------------
-- targeted routing: kind "<peer>.<rest>" delivers only to <peer>
-- ------------------------------------------------------------------
local function test_kind_prefix_targets_named_peer_only()
  reset()
  _test.set_plugins({ "src", "nefor-tui", "other" })

  local log = {}
  ready_in_order(log, { "src", "nefor-tui", "other" })
  _test.calls_clear()

  -- src emits a "nefor-tui.grid.line" event. The kind prefix matches the
  -- nefor-tui peer (and src is not nefor-tui), so it should deliver only
  -- to nefor-tui — not "other".
  log[#log + 1] = entry_plugin("src", make_event({ kind = "nefor-tui.grid.line", row = 0 }))
  ncp.dispatch(log)

  local targets = {}
  for _, c in ipairs(_test.calls()) do
    targets[c.target] = (targets[c.target] or 0) + 1
  end
  assert_eq(targets["nefor-tui"] or 0, 1, "nefor-tui got the targeted event")
  assert_eq(targets["other"] or 0, 0, "'other' did not receive targeted event")
  assert_eq(targets["src"] or 0, 0, "sender did not receive its own event")
end

local function test_kind_prefix_self_announces_to_all_peers()
  reset()
  _test.set_plugins({ "nefor-tui", "a", "b" })

  local log = {}
  ready_in_order(log, { "nefor-tui", "a", "b" })
  _test.calls_clear()

  -- nefor-tui announces "nefor-tui.ready" — prefix matches the sender
  -- itself, so this is a self-announcement and broadcasts to all peers.
  log[#log + 1] = entry_plugin("nefor-tui", make_event({ kind = "nefor-tui.ready" }))
  ncp.dispatch(log)

  local targets = {}
  for _, c in ipairs(_test.calls()) do
    targets[c.target] = (targets[c.target] or 0) + 1
  end
  assert_eq(targets["a"] or 0, 1, "'a' got the announcement")
  assert_eq(targets["b"] or 0, 1, "'b' got the announcement")
  assert_eq(targets["nefor-tui"] or 0, 0, "sender excluded from broadcast")
end

-- ------------------------------------------------------------------
-- mock_plugin_adapter: rewrites cc.* ↔ chat.*
-- ------------------------------------------------------------------
local cc = require("mock_plugin_adapter")

local function test_cc_adapter_renames_stream_events_to_chat()
  local out = cc.from_plugin({
    type = "event",
    from = "mock-plugin",
    body = { kind = "cc.stream.delta", text = "hi" },
  })
  assert_eq(out.body.kind, "chat.stream.delta", "cc.stream.delta → chat.stream.delta")
  assert_eq(out.body.text, "hi", "text preserved")

  local end_out = cc.from_plugin({
    type = "event",
    from = "mock-plugin",
    body = {
      kind = "cc.stream.end", text = "done",
      model = "claude-sonnet-4-6", duration_ms = 1500,
      cost_usd = 0.001, num_turns = 1,
    },
  })
  assert_eq(end_out.body.kind, "chat.stream.end", "cc.stream.end renamed")
  assert_eq(end_out.body.text, "done", "text preserved")
  assert_eq(end_out.body.cost_usd, nil, "cost_usd stripped (lives on session.stats)")
  assert_eq(end_out.body.num_turns, nil, "num_turns stripped")
  assert_eq(end_out.body.model, "claude-sonnet-4-6", "model preserved for footer")
  assert_eq(end_out.body.duration_ms, 1500, "duration_ms preserved for footer")
end

local function test_cc_adapter_renames_session_stats_and_tool()
  local s = cc.from_plugin({
    type = "event", from = "mock-plugin",
    body = { kind = "cc.session.stats", model = "claude-opus-4-7", turns = 3 },
  })
  assert_eq(s.body.kind, "chat.session.stats", "session.stats renamed")
  assert_eq(s.body.model, "claude-opus-4-7", "model preserved")
  assert_eq(s.body.turns, 3, "turns preserved")

  local t = cc.from_plugin({
    type = "event", from = "mock-plugin",
    body = { kind = "cc.tool.start", name = "Bash", input = { command = "ls" } },
  })
  assert_eq(t.body.kind, "chat.tool.start", "tool.start renamed")
  assert_eq(t.body.name, "Bash", "tool name preserved")
end

local function test_cc_adapter_drops_assistant_usage()
  local out = cc.from_plugin({
    type = "event", from = "mock-plugin",
    body = { kind = "cc.assistant.usage", input_tokens = 10, output_tokens = 20 },
  })
  assert_eq(out, nil, "cc.assistant.usage is dropped (subsumed by session.stats)")
end

local function test_cc_adapter_surfaces_turn_error_as_system_message()
  local out = cc.from_plugin({
    type = "event", from = "mock-plugin",
    body = { kind = "cc.turn.error", message = "rate limit" },
  })
  assert_eq(out.body.kind, "chat.message.append", "turn.error becomes message.append")
  assert_eq(out.body.role, "system", "role=system")
  assert_true(out.body.text:find("rate limit", 1, true) ~= nil,
    "error message text included")
end

local function test_cc_adapter_passes_through_lifecycle_events()
  for _, k in ipairs({ "cc.hello", "cc.ready", "cc.goodbye" }) do
    local out = cc.from_plugin({
      type = "event", from = "mock-plugin",
      body = { kind = k },
    })
    assert_eq(out.body.kind, k, k .. " passes through unchanged")
  end
end

local function test_cc_adapter_to_plugin_rewrites_input_submit()
  local out = cc.to_plugin({
    type = "event", from = "nefor-tui",
    body = { kind = "chat.input.submit", text = "ping" },
  })
  assert_eq(out.body.kind, "cc.prompt", "chat.input.submit → cc.prompt")
  assert_eq(out.body.text, "ping", "text preserved")

  local r = cc.to_plugin({
    type = "event", from = "nefor-tui",
    body = { kind = "chat.resume", session_id = "abc" },
  })
  assert_eq(r.body.kind, "cc.resume", "chat.resume → cc.resume")
  assert_eq(r.body.session_id, "abc", "session_id preserved")
end

local function test_cc_adapter_to_plugin_rewrites_interrupt()
  -- Mid-turn abort path: nefor-tui ESC emits chat.interrupt, the adapter
  -- renames it to cc.interrupt before delivery so mock-plugin's
  -- dispatch-loop can find the running child and kill it.
  local out = cc.to_plugin({
    type = "event", from = "nefor-tui",
    body = { kind = "chat.interrupt" },
  })
  assert_eq(out.body.kind, "cc.interrupt", "chat.interrupt → cc.interrupt")
end

-- ------------------------------------------------------------------
-- agentic_workflow.for_provider: static_token injects auth.set on ready
-- ------------------------------------------------------------------
--
-- Behaviour preserved from the prior openai_provider_adapter.make. The
-- factory now composes inner (rg-style chat-completion correlation +
-- stream gating) AND outer (chat-contract rename + static-token
-- injection) into one transform pair; the static-token tests still
-- exercise the outer-adapter behaviour through that composed pair.
local rga = require("agentic_workflow")

local function test_openai_adapter_static_token_injects_auth_set_on_ready()
  reset()
  rga._reset()
  local ad = rga.for_provider("ollama", { static_token = "local" })
  -- Pre-ready and unrelated events shouldn't trigger an injection.
  ad.from_plugin({
    type = "event", from = "ollama",
    body = { kind = "ollama.hello" },
  })
  assert_eq(#_test.calls(), 0, "hello must not trigger injection")
  -- The first ready triggers a synthetic auth.set targeted at the same plugin.
  local out = ad.from_plugin({
    type = "event", from = "ollama",
    body = { kind = "ollama.ready" },
  })
  assert_eq(out, nil, "ready is dropped from upstream view")
  local calls = _test.calls()
  assert_eq(#calls, 1, "exactly one synthesized send on ready")
  assert_eq(calls[1].target, "ollama", "auth.set targets the ollama plugin")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.type, "event", "type=event")
  assert_eq(decoded.body.kind, "ollama.auth.set", "kind=ollama.auth.set")
  assert_eq(decoded.body.token, "local", "token carries static_token value")
  -- A second ready (defensive) doesn't double-inject.
  _test.calls_clear()
  ad.from_plugin({
    type = "event", from = "ollama",
    body = { kind = "ollama.ready" },
  })
  assert_eq(#_test.calls(), 0, "second ready must not re-inject")
end

local function test_openai_adapter_no_static_token_skips_injection()
  reset()
  rga._reset()
  local ad = rga.for_provider("ollama")  -- no opts → no static_token
  ad.from_plugin({
    type = "event", from = "ollama",
    body = { kind = "ollama.ready" },
  })
  assert_eq(#_test.calls(), 0, "no static_token → no injection")
end

-- ------------------------------------------------------------------
-- agentic_workflow reasoner-graph wiring: bridges reasoner-graph ↔
-- openai-provider / tool-gate via per-firing pending state.
-- ------------------------------------------------------------------
--
-- The transforms intercept three streams:
--   * `<type>.run_node` from reasoner-graph  → drives the worker plugin,
--     emits `<type>.run_node.ack` and a future `graph.node_result`.
--   * `<provider>.chat.complete.result`      → resolves a pending firing
--     for a `dummy` / `provider-wrapper` node, emits `graph.node_result`.
--   * `tool.result`                          → resolves a pending firing
--     for a `tool-executor` node.
--
-- The Rust harness installs `nefor.engine.send` as the recording mock,
-- so envelopes emitted via `nefor.engine.send` land in `_test.calls()`
-- exactly like ncp.lua's own broadcasts.

-- Helper: after `for_reasoner_graph().from_plugin(env)` runs, locate the
-- first recorded send whose body.kind is `target_kind`. Returns the
-- decoded envelope.
local function find_call_with_kind(target_kind)
  for _, c in ipairs(_test.calls()) do
    local d = json.decode(c.payload)
    if d and d.body and d.body.kind == target_kind then
      return d, c.target
    end
  end
  return nil
end

-- Helper: filter all recorded calls into a list of decoded {body, target}.
local function decode_all_calls()
  local out = {}
  for _, c in ipairs(_test.calls()) do
    out[#out + 1] = { decoded = json.decode(c.payload), target = c.target }
  end
  return out
end

-- Reset both the adapter's pending state and the recorded send log.
local function reset_rga()
  reset()
  rga._reset()
end

local function test_rga_dummy_run_node_emits_ack_immediately()
  -- The scheduler's ack_deadline applies to <type>.run_node.ack. The
  -- adapter must emit the ack synchronously inside from_plugin (before
  -- returning) so the watchdog stops well before any provider I/O.
  reset_rga()
  _test.set_plugins({ "ollama", "nefor-tui" })
  local hook = rga.for_reasoner_graph()
  local out = hook.from_plugin({
    type = "event", from = "reasoner-graph",
    body = {
      kind = "dummy.run_node",
      run_id = "r1", node_id = "n1", firing_id = "f1",
      args = { provider = "ollama", prompt = "hi" },
      inputs = {},
    },
  })
  assert_eq(out, nil, "run_node envelope is dropped after handling")

  local ack = find_call_with_kind("dummy.run_node.ack")
  assert_true(ack ~= nil, "ack envelope was emitted")
  assert_eq(ack.body.run_id, "r1", "ack carries run_id")
  assert_eq(ack.body.firing_id, "f1", "ack carries firing_id")
end

local function test_rga_unknown_type_acks_then_errors_node_result()
  -- An unknown reasoner type still gets an ack so the scheduler stops
  -- the deadline watchdog; the failure surfaces as a graph.node_result
  -- error rather than leaving the firing dangling.
  reset_rga()
  _test.set_plugins({ "x" })
  local hook = rga.for_reasoner_graph()
  hook.from_plugin({
    type = "event", from = "reasoner-graph",
    body = {
      kind = "no-such-type.run_node",
      run_id = "r9", node_id = "nx", firing_id = "fx",
      args = {}, inputs = {},
    },
  })

  local ack = find_call_with_kind("no-such-type.run_node.ack")
  assert_true(ack ~= nil, "ack still fired for unknown type")
  local result = find_call_with_kind("graph.node_result")
  assert_true(result ~= nil, "graph.node_result emitted")
  assert_true(result.body.error ~= nil, "result carries error message")
  assert_true(
    result.body.error:find("no-such-type", 1, true) ~= nil,
    "error names the unknown reasoner type"
  )
end

local function test_rga_dispatch_drives_provider_chat_create_and_complete()
  -- A first-firing dummy run_node must mint a chat.create + chat.append
  -- + chat.complete sequence on the underlying provider, with no
  -- prev_state present. Verifies the type-driven dispatch routes to
  -- the openai-provider for `dummy`.
  reset_rga()
  _test.set_plugins({ "ollama" })
  local hook = rga.for_reasoner_graph()
  hook.from_plugin({
    type = "event", from = "reasoner-graph",
    body = {
      kind = "dummy.run_node",
      run_id = "r2", node_id = "wrap", firing_id = "f2",
      args = { provider = "ollama", prompt = "hello", model = "qwen2.5-coder:7b" },
      inputs = {},
    },
  })

  local create = find_call_with_kind("ollama.chat.create")
  assert_true(create ~= nil, "chat.create dispatched")
  assert_true(type(create.body.chat_id) == "string" and #create.body.chat_id > 0,
    "chat_id minted")
  assert_eq(create.body.model, "qwen2.5-coder:7b", "model forwarded from args")

  local append = find_call_with_kind("ollama.chat.append")
  assert_true(append ~= nil, "chat.append dispatched")
  assert_eq(append.body.message.role, "user", "prompt appended as user")
  assert_eq(append.body.message.content, "hello", "prompt content forwarded")

  local complete = find_call_with_kind("ollama.chat.complete")
  assert_true(complete ~= nil, "chat.complete dispatched")
  assert_eq(complete.body.chat_id, create.body.chat_id, "complete on same chat")
end

local function test_rga_prev_state_chat_id_skips_create()
  -- provider-wrapper second firing: prev_state carries a chat_id, so the
  -- adapter must NOT mint chat.create — it reuses the prior chat. This
  -- is the loop mechanism described in the parent spec §3 ("Reasoner
  -- state across firings").
  reset_rga()
  _test.set_plugins({ "ollama" })
  local hook = rga.for_reasoner_graph()
  hook.from_plugin({
    type = "event", from = "reasoner-graph",
    body = {
      kind = "provider-wrapper.run_node",
      run_id = "r3", node_id = "wrap", firing_id = "f-second",
      args = { provider = "ollama" },
      inputs = { up = { output = { messages = { { role = "user", content = "again" } } } } },
      prev_state = { chat_id = "chat-existing" },
    },
  })

  -- chat.create must not appear.
  for _, c in ipairs(decode_all_calls()) do
    assert_true(
      c.decoded.body.kind ~= "ollama.chat.create",
      "chat.create suppressed when prev_state.chat_id is supplied"
    )
  end

  local complete = find_call_with_kind("ollama.chat.complete")
  assert_true(complete ~= nil, "chat.complete still dispatched")
  assert_eq(complete.body.chat_id, "chat-existing",
    "complete reuses prev_state.chat_id verbatim")
end

local function test_rga_provider_result_emits_node_result_with_next_state()
  -- When openai-provider replies with chat.complete.result, the adapter
  -- must translate it into graph.node_result carrying next_state with
  -- the chat_id so the scheduler stores it on the node for the next
  -- firing.
  reset_rga()
  _test.set_plugins({ "ollama" })
  -- 1) Dispatch a firing so the adapter learns the chat_id.
  rga.for_reasoner_graph().from_plugin({
    type = "event", from = "reasoner-graph",
    body = {
      kind = "dummy.run_node",
      run_id = "rR", node_id = "nR", firing_id = "fR",
      args = { provider = "ollama", prompt = "p" },
      inputs = {},
    },
  })
  -- Capture the chat_id from the create envelope.
  local create = find_call_with_kind("ollama.chat.create")
  assert_true(create ~= nil, "create envelope captured")
  local chat_id = create.body.chat_id
  _test.calls_clear()

  -- 2) Provider replies. The provider-side hook must emit graph.node_result.
  local prov_hook = rga.for_provider("ollama")
  local passed = prov_hook.from_plugin({
    type = "event", from = "ollama",
    body = {
      kind = "ollama.chat.complete.result",
      chat_id = chat_id,
      output = { text = "the answer" },
    },
  })
  assert_eq(passed, nil, "owned chat.complete.result is dropped (consumed)")

  local result = find_call_with_kind("graph.node_result")
  assert_true(result ~= nil, "graph.node_result emitted")
  assert_eq(result.body.run_id, "rR", "run_id propagated")
  assert_eq(result.body.node_id, "nR", "node_id propagated")
  assert_eq(result.body.firing_id, "fR", "firing_id propagated")
  assert_true(type(result.body.output) == "table", "output forwarded as object")
  assert_eq(result.body.output.text, "the answer", "output payload preserved")
  assert_true(type(result.body.next_state) == "table", "next_state present")
  assert_eq(result.body.next_state.chat_id, chat_id,
    "next_state.chat_id matches the chat we ran")
end

local function test_rga_provider_result_for_unknown_chat_passes_through()
  -- A chat.complete.result that doesn't belong to a pending firing must
  -- pass through unchanged so any other consumer (the chat adapter) can
  -- see it.
  reset_rga()
  local prov_hook = rga.for_provider("ollama")
  local env = {
    type = "event", from = "ollama",
    body = {
      kind = "ollama.chat.complete.result",
      chat_id = "not-ours",
      output = { text = "..." },
    },
  }
  local out = prov_hook.from_plugin(env)
  assert_true(out == env, "non-pending chat.complete.result passes through")
  assert_eq(#_test.calls(), 0, "no graph.node_result emitted")
end

local function test_rga_per_firing_keying_distinct_firings_resolve_independently()
  -- The same node firing twice must get two distinct firing_ids; the
  -- adapter must correlate each provider reply back to the right firing.
  -- We dispatch firing A then firing B (different firing_ids, different
  -- chats), then resolve B first and A second — both graph.node_results
  -- must carry the correct firing_id.
  reset_rga()
  _test.set_plugins({ "ollama" })
  local hook = rga.for_reasoner_graph()

  hook.from_plugin({
    type = "event", from = "reasoner-graph",
    body = {
      kind = "dummy.run_node",
      run_id = "rP", node_id = "nP", firing_id = "fA",
      args = { provider = "ollama", prompt = "a" },
      inputs = {},
    },
  })
  local create_a = find_call_with_kind("ollama.chat.create")
  local chat_a = create_a.body.chat_id
  _test.calls_clear()

  hook.from_plugin({
    type = "event", from = "reasoner-graph",
    body = {
      kind = "dummy.run_node",
      run_id = "rP", node_id = "nP", firing_id = "fB",
      args = { provider = "ollama", prompt = "b" },
      inputs = {},
    },
  })
  local create_b = find_call_with_kind("ollama.chat.create")
  local chat_b = create_b.body.chat_id
  assert_true(chat_a ~= chat_b, "two firings mint two distinct chat_ids")
  _test.calls_clear()

  local prov = rga.for_provider("ollama")
  -- Resolve B first.
  prov.from_plugin({
    type = "event", from = "ollama",
    body = {
      kind = "ollama.chat.complete.result",
      chat_id = chat_b,
      output = { text = "B-ans" },
    },
  })
  local resB = find_call_with_kind("graph.node_result")
  assert_true(resB ~= nil, "result for B fired")
  assert_eq(resB.body.firing_id, "fB", "result correlated to firing B")
  _test.calls_clear()

  -- Resolve A second.
  prov.from_plugin({
    type = "event", from = "ollama",
    body = {
      kind = "ollama.chat.complete.result",
      chat_id = chat_a,
      output = { text = "A-ans" },
    },
  })
  local resA = find_call_with_kind("graph.node_result")
  assert_true(resA ~= nil, "result for A fired")
  assert_eq(resA.body.firing_id, "fA", "result correlated to firing A")
  -- And the adapter has cleared its pending map.
  assert_eq(rga._pending_count(), 0,
    "all pending firings resolved → pending_count == 0")
end

local function test_rga_register_type_dispatches_custom_handler()
  -- Type-driven dispatch is extensible: a custom reasoner type can be
  -- registered and `<custom>.run_node` must invoke its handler.
  reset_rga()
  _test.set_plugins({ "x" })
  local seen = {}
  rga.register_type("my-leaf", function(body)
    seen.run_id    = body.run_id
    seen.firing_id = body.firing_id
    seen.prev_state = body.prev_state
    return nil  -- async handler; ack will fire and we own the eventual result
  end)

  local hook = rga.for_reasoner_graph()
  hook.from_plugin({
    type = "event", from = "reasoner-graph",
    body = {
      kind = "my-leaf.run_node",
      run_id = "rZ", node_id = "nZ", firing_id = "fZ",
      args = {}, inputs = {},
      prev_state = { hint = "preserved" },
    },
  })

  assert_eq(seen.run_id, "rZ", "handler received run_id")
  assert_eq(seen.firing_id, "fZ", "handler received firing_id")
  assert_true(type(seen.prev_state) == "table",
    "handler received prev_state table")
  assert_eq(seen.prev_state.hint, "preserved",
    "prev_state fields passed through verbatim")
  -- Ack still fires for the async handler.
  local ack = find_call_with_kind("my-leaf.run_node.ack")
  assert_true(ack ~= nil, "ack emitted for custom type")
end

local function test_rga_terminal_handler_emits_ack_and_node_result_synchronously()
  -- The terminal handler is synchronous Lua: it must emit BOTH the ack
  -- and the graph.node_result before from_plugin returns. This is the
  -- "_already_replied" code path.
  reset_rga()
  _test.set_plugins({ "x" })
  local hook = rga.for_reasoner_graph()
  hook.from_plugin({
    type = "event", from = "reasoner-graph",
    body = {
      kind = "terminal.run_node",
      run_id = "rT", node_id = "term", firing_id = "fT",
      args = {}, inputs = { up = { output = { text = "the final word" } } },
    },
  })

  local ack = find_call_with_kind("terminal.run_node.ack")
  assert_true(ack ~= nil, "terminal ack emitted")
  local result = find_call_with_kind("graph.node_result")
  assert_true(result ~= nil, "terminal node_result emitted synchronously")
  assert_eq(result.body.run_id, "rT", "run_id forwarded")
  assert_eq(result.body.firing_id, "fT", "firing_id forwarded")
  assert_eq(result.body.output.text, "the final word",
    "FinalAnswer from inputs echoed as output")
  -- No pending state should remain — terminal is synchronous.
  assert_eq(rga._pending_count(), 0, "no pending after synchronous handler")
end

local function test_rga_for_reasoner_graph_passes_through_unrelated_kinds()
  -- Anything that isn't `<token>.run_node` must pass through unchanged
  -- so other consumers on the bus continue to see it.
  reset_rga()
  local hook = rga.for_reasoner_graph()
  local env = {
    type = "event", from = "reasoner-graph",
    body = { kind = "graph.run_started", run_id = "r" },
  }
  local out = hook.from_plugin(env)
  assert_true(out == env, "non-run_node envelope passes through unchanged")
  assert_eq(#_test.calls(), 0, "no synthesised events for unrelated kinds")
end

local function test_cc_adapter_renames_interrupted_turn_error_to_marker()
  -- Inverse direction: mock-plugin's interrupt path emits
  -- cc.turn.error{message="interrupted"}. The adapter folds it into a
  -- friendly "[interrupted]" system message rather than an "Error: …" line.
  local out = cc.from_plugin({
    type = "event", from = "mock-plugin",
    body = { kind = "cc.turn.error", message = "interrupted" },
  })
  assert_eq(out.body.kind, "chat.message.append", "becomes a system message")
  assert_eq(out.body.role, "system", "role=system")
  assert_eq(out.body.text, "[interrupted]", "deliberate-looking marker, not Error: …")
end

-- ------------------------------------------------------------------
-- M.spawn — config-load-time validation of cfg fields
-- ------------------------------------------------------------------
--
-- The Rust harness (`crates/nefor/tests/starter_ncp_test.rs`) installs a
-- mock `nefor.engine` but doesn't install `nefor.plugins`, so we stub the
-- spawn binding ourselves for these tests. Calls land in `spawn_calls` so
-- accept-tests can verify the wrapper forwards the engine-known fields.
local spawn_calls = {}
local function install_spawn_stub()
  spawn_calls = {}
  nefor.plugins = {
    spawn = function(opts)
      table.insert(spawn_calls, opts)
    end,
  }
end

-- pcall-wrapping helper: catches the error thrown by `M.spawn` and asserts
-- the message contains an expected substring. Returns the full error
-- string for test-side inspection if needed.
local function assert_spawn_errors_with(cfg, expected_substring, msg)
  local ok, err = pcall(ncp.spawn, cfg)
  assert_true(not ok, (msg or "spawn must error") .. " (was accepted)")
  err = tostring(err)
  if not err:find(expected_substring, 1, true) then
    error(string.format(
      "assertion failed: %s\n  expected error to contain: %s\n  actual error: %s",
      msg or "wrong error message",
      expected_substring,
      err
    ))
  end
  return err
end

local function test_spawn_rejects_env_field_with_hint()
  reset()
  install_spawn_stub()
  assert_spawn_errors_with(
    {
      name    = "p",
      command = { "/bin/echo" },
      env     = { FOO = "bar" },
    },
    "unknown field 'env'",
    "env field rejected"
  )
  -- Hint must point at the command-array workaround.
  local _, err = pcall(ncp.spawn, {
    name = "p", command = { "/bin/echo" }, env = { FOO = "bar" },
  })
  assert_true(
    tostring(err):find("command array", 1, true) ~= nil,
    "env hint mentions command array"
  )
  assert_eq(#spawn_calls, 0, "engine spawn must not be invoked on rejection")
end

local function test_spawn_rejects_args_field_with_hint()
  reset()
  install_spawn_stub()
  local err = assert_spawn_errors_with(
    {
      name    = "p",
      command = { "/bin/echo" },
      args    = { "--flag" },
    },
    "unknown field 'args'",
    "args field rejected"
  )
  assert_true(
    err:find("command array", 1, true) ~= nil,
    "args hint mentions command array"
  )
end

local function test_spawn_rejects_cwd_field_with_hint()
  reset()
  install_spawn_stub()
  local err = assert_spawn_errors_with(
    {
      name    = "p",
      command = { "/bin/echo" },
      cwd     = "/tmp",
    },
    "unknown field 'cwd'",
    "cwd field rejected"
  )
  assert_true(
    err:find("<plugin-dir>/<name>", 1, true) ~= nil,
    "cwd hint mentions plugin-dir/name policy"
  )
end

local function test_spawn_rejects_unknown_field()
  reset()
  install_spawn_stub()
  -- Anything outside the recognised set falls through to the generic
  -- "unknown field '<key>'" branch — no special hint.
  assert_spawn_errors_with(
    {
      name      = "p",
      command   = { "/bin/echo" },
      mystery   = 42,
    },
    "unknown field 'mystery'",
    "unknown field rejected"
  )
end

-- ------------------------------------------------------------------
-- engine-originated envelopes: engine.plugin_failed → chat.popup
-- ------------------------------------------------------------------
--
-- The engine emits synthetic `{type:"event", from:"engine", body:{kind="engine.plugin_failed", ...}}`
-- envelopes when a plugin fails to spawn or crashes at runtime. Step
-- translates them into a `chat.popup{level="error"}` targeted at nefor-tui
-- so the user sees the failure instead of it vanishing into engine logs.
--
-- Manual-test recipes (when you want to eyeball the rendered popup):
--   * Spawn failure: edit `starter/init.lua` to make a spawn's `command[0]`
--     point at a non-existent path (e.g. `bin("nonexistent")`). Run
--     `just run`. Expect a chat.popup error popup naming the plugin.
--   * Runtime crash: replace a spawn's command with a wrapper that exits
--     after a few seconds, e.g. `command = { "sh", "-c", "sleep 3; exit 1" }`.
--     Run `just run`, wait, observe the popup before engine winds down.
local function entry_engine(payload)
  return { ts = "2026-04-23T00:00:00.000Z", origin = "engine", target = nil, payload = payload }
end

local function make_engine_plugin_failed(plugin, phase, reason, code)
  return json.encode({
    type = "event",
    from = "engine",
    ts   = "2026-04-23T00:00:00.000Z",
    body = {
      kind   = "engine.plugin_failed",
      plugin = plugin,
      phase  = phase,
      reason = reason,
      code   = code,
    },
  })
end

local function test_engine_plugin_failed_routes_to_chat_popup()
  reset()
  -- nefor-tui must be ready before it can receive popups — its NCP layer
  -- drops every pre-ready_ok inbound (per spec §5.1). Drive the ready
  -- handshake first, then clear calls so the assertion below counts only
  -- the popup send.
  _test.set_plugins({ "nefor-tui" })
  local log = { entry_plugin("nefor-tui", make_ready("0.1")) }
  ncp.dispatch(log)
  _test.calls_clear()

  local payload = make_engine_plugin_failed(
    "ollama", "spawn", "binary not found", "missing_dir"
  )
  log[#log + 1] = entry_engine(payload)
  ncp.dispatch(log)

  local calls = _test.calls()
  assert_eq(#calls, 1, "exactly one send: the chat.popup")
  assert_eq(calls[1].target, "nefor-tui", "popup targeted at nefor-tui")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.type, "event", "type=event")
  assert_eq(decoded.from, "engine", "from=engine")
  assert_eq(decoded.body.kind, "chat.popup", "kind=chat.popup")
  assert_eq(decoded.body.level, "error", "level=error")
  assert_eq(decoded.body.source, "engine", "source=engine")
  assert_true(
    decoded.body.message:find("ollama", 1, true) ~= nil,
    "message includes plugin name"
  )
  assert_true(
    decoded.body.message:find("binary not found", 1, true) ~= nil,
    "message includes reason"
  )
  assert_true(
    decoded.body.message:find("spawn", 1, true) ~= nil,
    "message includes phase"
  )
end

local function test_engine_plugin_failed_drops_when_chat_not_connected()
  reset()
  -- nefor-tui absent from the bus — the failure has nowhere to render, so
  -- step silently drops the envelope. (The user can't see it; better than
  -- a `send` to a non-existent peer that the engine warns about.)
  _test.set_plugins({ "ollama" })

  local payload = make_engine_plugin_failed(
    "nefor-tui", "runtime", "crashed", "crash"
  )
  local log = { entry_engine(payload) }
  ncp.dispatch(log)

  assert_eq(#_test.calls(), 0,
    "no send when nefor-tui isn't on the bus")
end

local function test_engine_origin_does_not_trigger_ready_handshake_error()
  -- Pre-fix regression guard: the engine origin used to fall through to
  -- handle_event(), which emits a malformed_envelope error back to the
  -- origin because the engine "hasn't readied". That error would target
  -- "engine" and the binding rejects reserved names, breaking the test.
  -- After the fix, engine envelopes route through their own dispatcher
  -- and never reach the ready check.
  reset()
  _test.set_plugins({ "nefor-tui" })

  local payload = make_engine_plugin_failed(
    "x", "spawn", "y", "missing_dir"
  )
  ncp.dispatch({ entry_engine(payload) })

  -- All sends must target nefor-tui (the popup), not "engine".
  for _, c in ipairs(_test.calls()) do
    assert_true(
      c.target ~= "engine",
      "no send may target the engine origin"
    )
  end
end

local function test_engine_plugin_failed_buffers_until_chat_readies()
  -- Real-world scenario: engine reports a spawn failure during boot, before
  -- nefor-tui completes its handshake. Step must buffer the popup and
  -- flush it once nefor-tui readies, otherwise the popup hits a pre-
  -- ready_ok plugin and gets dropped (per §5.1).
  reset()
  _test.set_plugins({ "nefor-tui" })

  -- Engine envelope arrives first — chat isn't ready yet.
  local payload = make_engine_plugin_failed(
    "ollama", "spawn", "binary not found", "missing_dir"
  )
  local log = { entry_engine(payload) }
  ncp.dispatch(log)
  assert_eq(#_test.calls(), 0,
    "no send while nefor-tui is pre-ready (popup buffered)")

  -- Now chat readies — handshake reply + buffered popup must both fire.
  log[#log + 1] = entry_plugin("nefor-tui", make_ready("0.1"))
  ncp.dispatch(log)

  local calls = _test.calls()
  assert_eq(#calls, 2,
    "two sends after ready: ready_ok + buffered popup")
  assert_eq(calls[1].target, "nefor-tui", "ready_ok targets chat")
  local ready_ok = json.decode(calls[1].payload)
  assert_eq(ready_ok.body.kind, "ready_ok", "first send is ready_ok")
  assert_eq(calls[2].target, "nefor-tui", "popup targets chat")
  local popup = json.decode(calls[2].payload)
  assert_eq(popup.body.kind, "chat.popup", "second send is the buffered popup")
  assert_eq(popup.body.level, "error", "level=error")
  assert_true(
    popup.body.message:find("ollama", 1, true) ~= nil,
    "popup message includes plugin name"
  )
end

local function test_engine_envelopes_skipped_in_replay_to_late_attachers()
  -- A late-attaching plugin must not see raw `engine.*` kinds replayed onto
  -- it: those are private to the translation layer. Pre-fix behavior leaked
  -- `engine.plugin_failed` to every late attacher.
  reset()
  _test.set_plugins({ "nefor-tui", "late" })

  -- Chat readies first; engine reports a failure; then a late plugin
  -- attaches. Replay should NOT carry the engine entry to it.
  local log = { entry_plugin("nefor-tui", make_ready("0.1")) }
  ncp.dispatch(log)
  log[#log + 1] = entry_engine(
    make_engine_plugin_failed("x", "spawn", "y", "missing_dir")
  )
  ncp.dispatch(log)
  _test.calls_clear()

  log[#log + 1] = entry_plugin("late", make_ready("0.1"))
  ncp.dispatch(log)

  -- Late plugin gets ready_ok; nothing else (no engine.plugin_failed
  -- replay, no chat.popup replay since the chat.popup was a step-origin
  -- entry and replay already skips those).
  local calls = _test.calls()
  for _, c in ipairs(calls) do
    if c.target == "late" then
      local decoded = json.decode(c.payload)
      assert_true(
        decoded.body.kind ~= "engine.plugin_failed",
        "engine.plugin_failed must not replay to late attachers"
      )
    end
  end
end

local function test_spawn_accepts_the_four_valid_fields_without_error()
  reset()
  install_spawn_stub()
  -- All four valid fields present; both transforms registered.
  ncp.spawn({
    name        = "p",
    command     = { "/bin/echo", "hi" },
    from_plugin = function(env) return env end,
    to_plugin   = function(env) return env end,
  })
  assert_eq(#spawn_calls, 1, "engine spawn invoked once")
  assert_eq(spawn_calls[1].name, "p", "name forwarded")
  assert_true(
    type(spawn_calls[1].command) == "table",
    "command forwarded as a table"
  )
  assert_eq(spawn_calls[1].command[1], "/bin/echo", "command[1] preserved")
  -- Transforms must be stripped from the engine-bound payload.
  assert_eq(spawn_calls[1].from_plugin, nil,
    "from_plugin must not leak to engine spawn")
  assert_eq(spawn_calls[1].to_plugin, nil,
    "to_plugin must not leak to engine spawn")
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
  { name = "from_plugin_transform_rewrites_event_kind", fn = test_from_plugin_transform_rewrites_event_kind },
  { name = "from_plugin_transform_returning_nil_drops_envelope", fn = test_from_plugin_transform_returning_nil_drops_envelope },
  { name = "to_plugin_transform_rewrites_per_target_only", fn = test_to_plugin_transform_rewrites_per_target_only },
  { name = "to_plugin_transform_returning_nil_drops_for_target_only", fn = test_to_plugin_transform_returning_nil_drops_for_target_only },
  { name = "from_plugin_transform_error_emits_transform_error", fn = test_from_plugin_transform_error_emits_transform_error },
  { name = "replayed_events_pass_through_from_plugin_transform", fn = test_replayed_events_pass_through_from_plugin_transform },
  { name = "kind_prefix_targets_named_peer_only", fn = test_kind_prefix_targets_named_peer_only },
  { name = "kind_prefix_self_announces_to_all_peers", fn = test_kind_prefix_self_announces_to_all_peers },
  { name = "cc_adapter_renames_stream_events_to_chat", fn = test_cc_adapter_renames_stream_events_to_chat },
  { name = "cc_adapter_renames_session_stats_and_tool", fn = test_cc_adapter_renames_session_stats_and_tool },
  { name = "cc_adapter_drops_assistant_usage", fn = test_cc_adapter_drops_assistant_usage },
  { name = "cc_adapter_surfaces_turn_error_as_system_message", fn = test_cc_adapter_surfaces_turn_error_as_system_message },
  { name = "cc_adapter_passes_through_lifecycle_events", fn = test_cc_adapter_passes_through_lifecycle_events },
  { name = "cc_adapter_to_plugin_rewrites_input_submit", fn = test_cc_adapter_to_plugin_rewrites_input_submit },
  { name = "cc_adapter_to_plugin_rewrites_interrupt", fn = test_cc_adapter_to_plugin_rewrites_interrupt },
  { name = "cc_adapter_renames_interrupted_turn_error_to_marker", fn = test_cc_adapter_renames_interrupted_turn_error_to_marker },
  { name = "openai_adapter_static_token_injects_auth_set_on_ready", fn = test_openai_adapter_static_token_injects_auth_set_on_ready },
  { name = "openai_adapter_no_static_token_skips_injection", fn = test_openai_adapter_no_static_token_skips_injection },
  { name = "rga_dummy_run_node_emits_ack_immediately", fn = test_rga_dummy_run_node_emits_ack_immediately },
  { name = "rga_unknown_type_acks_then_errors_node_result", fn = test_rga_unknown_type_acks_then_errors_node_result },
  { name = "rga_dispatch_drives_provider_chat_create_and_complete", fn = test_rga_dispatch_drives_provider_chat_create_and_complete },
  { name = "rga_prev_state_chat_id_skips_create", fn = test_rga_prev_state_chat_id_skips_create },
  { name = "rga_provider_result_emits_node_result_with_next_state", fn = test_rga_provider_result_emits_node_result_with_next_state },
  { name = "rga_provider_result_for_unknown_chat_passes_through", fn = test_rga_provider_result_for_unknown_chat_passes_through },
  { name = "rga_per_firing_keying_distinct_firings_resolve_independently", fn = test_rga_per_firing_keying_distinct_firings_resolve_independently },
  { name = "rga_register_type_dispatches_custom_handler", fn = test_rga_register_type_dispatches_custom_handler },
  { name = "rga_terminal_handler_emits_ack_and_node_result_synchronously", fn = test_rga_terminal_handler_emits_ack_and_node_result_synchronously },
  { name = "rga_for_reasoner_graph_passes_through_unrelated_kinds", fn = test_rga_for_reasoner_graph_passes_through_unrelated_kinds },
  { name = "spawn_rejects_env_field_with_hint", fn = test_spawn_rejects_env_field_with_hint },
  { name = "spawn_rejects_args_field_with_hint", fn = test_spawn_rejects_args_field_with_hint },
  { name = "spawn_rejects_cwd_field_with_hint", fn = test_spawn_rejects_cwd_field_with_hint },
  { name = "spawn_rejects_unknown_field", fn = test_spawn_rejects_unknown_field },
  { name = "spawn_accepts_the_four_valid_fields_without_error", fn = test_spawn_accepts_the_four_valid_fields_without_error },
  { name = "engine_plugin_failed_routes_to_chat_popup", fn = test_engine_plugin_failed_routes_to_chat_popup },
  { name = "engine_plugin_failed_drops_when_chat_not_connected", fn = test_engine_plugin_failed_drops_when_chat_not_connected },
  { name = "engine_origin_does_not_trigger_ready_handshake_error", fn = test_engine_origin_does_not_trigger_ready_handshake_error },
  { name = "engine_plugin_failed_buffers_until_chat_readies", fn = test_engine_plugin_failed_buffers_until_chat_readies },
  { name = "engine_envelopes_skipped_in_replay_to_late_attachers", fn = test_engine_envelopes_skipped_in_replay_to_late_attachers },
}

for _, t in ipairs(tests) do
  local ok, err = pcall(t.fn)
  if not ok then
    error("test '" .. t.name .. "' FAILED:\n" .. tostring(err), 0)
  end
end
