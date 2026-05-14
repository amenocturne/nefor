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
-- openai-provider wrapper: static_token injects auth.set on ready
-- ------------------------------------------------------------------
--
-- Post-Phase-3a the per-provider translation lives in the
-- `openai-provider` actor folder rather than agentic_workflow's
-- `for_provider()` factory. The behaviour is preserved verbatim — we
-- exercise it through the new wrapper's `from_plugin` directly.

local agentic_loop_mod = require("agentic-loop")
local openai_provider  = require("openai-provider")

local function build_provider_chain(name, opts)
  -- spawn_spec returns an actor table with from_plugin / to_plugin
  -- already wired. Tests just need the transform pair.
  local spec = openai_provider.spawn_spec(name, { "/bin/true" }, opts)
  return { from_plugin = spec.from_plugin, to_plugin = spec.to_plugin }
end

local function reset_loop()
  agentic_loop_mod._internals.reset()
end

local function test_openai_adapter_static_token_injects_auth_set_on_ready()
  reset()
  reset_loop()
  local ad = build_provider_chain("ollama", { static_token = "local" })
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
  reset_loop()
  local ad = build_provider_chain("ollama")  -- no opts → no static_token
  ad.from_plugin({
    type = "event", from = "ollama",
    body = { kind = "ollama.ready" },
  })
  assert_eq(#_test.calls(), 0, "no static_token → no injection")
end

-- ------------------------------------------------------------------
-- reasoners actor + openai-provider wrapper: bridges reasoner-graph ↔
-- openai-provider / tool-gate via per-firing pending state.
-- ------------------------------------------------------------------
--
-- The reasoners actor consumes `tool.invoke { name=<reasoner-type> }`
-- and runs the handler; provider firings spawn `<provider>.chat.create
-- / append / complete` and the openai-provider wrapper's from_plugin
-- correlates the eventual `<provider>.chat.complete.result` back to a
-- `tool.result { id=firing_id, result }` (with next_state inside
-- result for cycle re-fires). State (chat_id maps, pending entries)
-- lives on the agentic-loop actor and is exposed to wrappers via
-- helpers.
--
-- The Rust harness installs `nefor.engine.send` as the recording mock
-- so envelopes emitted via `nefor.engine.send` land in `_test.calls()`
-- exactly like ncp.lua's own broadcasts.

local reasoners = require("reasoners")

local function find_call_with_kind(target_kind)
  for _, c in ipairs(_test.calls()) do
    local d = json.decode(c.payload)
    if d and d.body and d.body.kind == target_kind then
      return d, c.target
    end
  end
  return nil
end

local function decode_all_calls()
  local out = {}
  for _, c in ipairs(_test.calls()) do
    out[#out + 1] = { decoded = json.decode(c.payload), target = c.target }
  end
  return out
end

-- Drive a `tool.invoke { name=<reasoner-type>, id=firing_id, args:
-- { run_id, node_id, args, inputs, prev_state } }` envelope through
-- the reasoners actor's receive_msg. Takes the legacy run-node body
-- shape (`{ run_id, node_id, firing_id, args, inputs, prev_state, kind
-- = "<type>.run_node" }`) and rewraps it into the canonical tool
-- contract — keeps test call sites readable.
local function dispatch_run_node(body)
  local kind = body.kind or ""
  local name = kind:match("^([^.]+)%.run_node$") or kind
  local invoke_body = {
    kind = "tool.invoke",
    id   = body.firing_id,
    name = name,
    args = {
      run_id     = body.run_id,
      node_id    = body.node_id,
      args       = body.args,
      inputs     = body.inputs,
      prev_state = body.prev_state,
    },
  }
  local entry = {
    ts      = "2026-04-23T00:00:00.000Z",
    origin  = "reasoner-graph",
    payload = json.encode({ type = "event", from = "reasoner-graph", body = invoke_body }),
  }
  reasoners.receive_msg(entry)
end

local function reset_rga()
  reset()
  reset_loop()
  reasoners._internals.reset()
end

-- Pending-count introspection (used to be agentic_workflow._pending_count).
local function pending_count()
  local n = 0
  for _ in pairs(agentic_loop_mod._internals.state.pending) do n = n + 1 end
  return n
end

local function test_rga_dummy_dispatch_drives_provider_chain()
  -- Smoke: dispatching the dummy reasoner via tool.invoke fires
  -- chat.create / chat.append / chat.complete on the underlying
  -- provider. Acks are gone in the canonical contract.
  reset_rga()
  _test.set_plugins({ "ollama", "nefor-tui" })
  dispatch_run_node({
    kind = "dummy.run_node",
    run_id = "r1", node_id = "n1", firing_id = "f1",
    args = { provider = "ollama", prompt = "hi" },
    inputs = {},
  })

  local create = find_call_with_kind("ollama.chat.create")
  assert_true(create ~= nil, "chat.create dispatched")
  local complete = find_call_with_kind("ollama.chat.complete")
  assert_true(complete ~= nil, "chat.complete dispatched")
end

local function test_rga_unknown_type_is_ignored()
  -- An unknown reasoner type is not for us — the reasoners actor must
  -- not synthesize any reply (no acks in the canonical contract; the
  -- Rust scheduler's peer-set check + ack-timeout watchdog handle
  -- "reasoner not connected" upstream).
  reset_rga()
  _test.set_plugins({ "x" })
  dispatch_run_node({
    kind = "no-such-type.run_node",
    run_id = "r9", node_id = "nx", firing_id = "fx",
    args = {}, inputs = {},
  })

  assert_eq(#_test.calls(), 0,
    "unknown reasoner type emits nothing (Rust handles peer-set failure)")
end

local function test_rga_dispatch_drives_provider_chat_create_and_complete()
  -- A first-firing dummy run_node must mint a chat.create + chat.append
  -- + chat.complete sequence on the underlying provider, with no
  -- prev_state present. Verifies the type-driven dispatch routes to
  -- the openai-provider for `dummy`.
  reset_rga()
  _test.set_plugins({ "ollama" })
  dispatch_run_node({
    kind = "dummy.run_node",
    run_id = "r2", node_id = "wrap", firing_id = "f2",
    args = { provider = "ollama", prompt = "hello", model = "qwen2.5-coder:7b" },
    inputs = {},
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
  -- adapter must NOT mint chat.create — it reuses the prior chat.
  reset_rga()
  _test.set_plugins({ "ollama" })
  dispatch_run_node({
    kind = "provider-wrapper.run_node",
    run_id = "r3", node_id = "wrap", firing_id = "f-second",
    args = { provider = "ollama" },
    inputs = { up = { output = { messages = { { role = "user", content = "again" } } } } },
    prev_state = { chat_id = "chat-existing" },
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

local function test_rga_provider_result_emits_tool_result_with_next_state()
  -- When openai-provider replies with chat.complete.result, the wrapper
  -- must translate it into a `tool.result { id=firing_id, result }`
  -- on the canonical contract — with `next_state.chat_id` packed
  -- INSIDE result so the Rust scheduler picks it up on cycle re-fires
  -- (wire-protocol spec coordination point 1).
  reset_rga()
  _test.set_plugins({ "ollama" })
  dispatch_run_node({
    kind = "dummy.run_node",
    run_id = "rR", node_id = "nR", firing_id = "fR",
    args = { provider = "ollama", prompt = "p" },
    inputs = {},
  })
  local create = find_call_with_kind("ollama.chat.create")
  assert_true(create ~= nil, "create envelope captured")
  local chat_id = create.body.chat_id
  _test.calls_clear()

  local prov_hook = build_provider_chain("ollama")
  local passed = prov_hook.from_plugin({
    type = "event", from = "ollama",
    body = {
      kind = "ollama.chat.complete.result",
      chat_id = chat_id,
      output = { text = "the answer" },
    },
  })
  assert_eq(passed, nil, "owned chat.complete.result is dropped (consumed)")

  local result = find_call_with_kind("tool.result")
  assert_true(result ~= nil, "tool.result emitted")
  assert_eq(result.body.id, "fR", "tool.result keyed by firing_id")
  assert_true(type(result.body.result) == "table", "result is an object")
  assert_eq(result.body.result.text, "the answer",
    "output fields preserved verbatim inside result")
  assert_true(type(result.body.result.next_state) == "table",
    "next_state present inside result")
  assert_eq(result.body.result.next_state.chat_id, chat_id,
    "next_state.chat_id matches the chat we ran")
end

local function test_rga_provider_result_for_unknown_chat_passes_through()
  -- A chat.complete.result that doesn't belong to a pending firing must
  -- pass through unchanged so any other consumer can see it.
  reset_rga()
  local prov_hook = build_provider_chain("ollama")
  local env = {
    type = "event", from = "ollama",
    body = {
      kind = "ollama.chat.complete.result",
      chat_id = "not-ours",
      output = { text = "..." },
    },
  }
  local out = prov_hook.from_plugin(env)
  -- The outer adapter passes ollama.chat.complete.result through
  -- unchanged (the inner adapter only intercepts owned chats; outer
  -- doesn't translate this kind). The envelope reference may be the
  -- same table or a translated equivalent; verify by kind.
  assert_true(out ~= nil, "non-pending chat.complete.result passes through")
  assert_eq(out.body.kind, "ollama.chat.complete.result",
    "kind unchanged on pass-through")
  assert_eq(#_test.calls(), 0, "no tool.result emitted")
end

local function test_rga_per_firing_keying_distinct_firings_resolve_independently()
  -- Two firings for the same node must mint two chat_ids; the wrapper
  -- correlates each provider reply back to the right firing.
  reset_rga()
  _test.set_plugins({ "ollama" })

  dispatch_run_node({
    kind = "dummy.run_node",
    run_id = "rP", node_id = "nP", firing_id = "fA",
    args = { provider = "ollama", prompt = "a" },
    inputs = {},
  })
  local create_a = find_call_with_kind("ollama.chat.create")
  local chat_a = create_a.body.chat_id
  _test.calls_clear()

  dispatch_run_node({
    kind = "dummy.run_node",
    run_id = "rP", node_id = "nP", firing_id = "fB",
    args = { provider = "ollama", prompt = "b" },
    inputs = {},
  })
  local create_b = find_call_with_kind("ollama.chat.create")
  local chat_b = create_b.body.chat_id
  assert_true(chat_a ~= chat_b, "two firings mint two distinct chat_ids")
  _test.calls_clear()

  local prov = build_provider_chain("ollama")
  prov.from_plugin({
    type = "event", from = "ollama",
    body = {
      kind = "ollama.chat.complete.result",
      chat_id = chat_b,
      output = { text = "B-ans" },
    },
  })
  local resB = find_call_with_kind("tool.result")
  assert_true(resB ~= nil, "result for B fired")
  assert_eq(resB.body.id, "fB", "result correlated to firing B")
  _test.calls_clear()

  prov.from_plugin({
    type = "event", from = "ollama",
    body = {
      kind = "ollama.chat.complete.result",
      chat_id = chat_a,
      output = { text = "A-ans" },
    },
  })
  local resA = find_call_with_kind("tool.result")
  assert_true(resA ~= nil, "result for A fired")
  assert_eq(resA.body.id, "fA", "result correlated to firing A")
  assert_eq(pending_count(), 0,
    "all pending firings resolved → pending_count == 0")
end

local function test_rga_register_type_dispatches_custom_handler()
  -- Type-driven dispatch is extensible: a custom reasoner type can be
  -- registered and a `tool.invoke { name=<custom> }` must invoke its
  -- handler with the unwrapped body shape.
  reset_rga()
  _test.set_plugins({ "x" })
  local seen = {}
  reasoners._internals.handlers["my-leaf"] = function(body)
    seen.run_id    = body.run_id
    seen.firing_id = body.firing_id
    seen.prev_state = body.prev_state
    return nil
  end

  dispatch_run_node({
    kind = "my-leaf.run_node",
    run_id = "rZ", node_id = "nZ", firing_id = "fZ",
    args = {}, inputs = {},
    prev_state = { hint = "preserved" },
  })

  assert_eq(seen.run_id, "rZ", "handler received run_id")
  assert_eq(seen.firing_id, "fZ", "handler received firing_id")
  assert_true(type(seen.prev_state) == "table",
    "handler received prev_state table")
  assert_eq(seen.prev_state.hint, "preserved",
    "prev_state fields passed through verbatim")

  -- Cleanup: remove the custom handler so it doesn't bleed into other
  -- tests in the same suite.
  reasoners._internals.handlers["my-leaf"] = nil
end

local function test_rga_terminal_handler_emits_tool_result_synchronously()
  -- The terminal handler is synchronous Lua: it must emit `tool.result
  -- { id=firing_id, result }` before receive_msg returns. The
  -- "_already_replied" code path.
  reset_rga()
  _test.set_plugins({ "x" })
  dispatch_run_node({
    kind = "terminal.run_node",
    run_id = "rT", node_id = "term", firing_id = "fT",
    args = {}, inputs = { up = { output = { text = "the final word" } } },
  })

  local result = find_call_with_kind("tool.result")
  assert_true(result ~= nil, "terminal tool.result emitted synchronously")
  assert_eq(result.body.id, "fT", "id == firing_id forwarded")
  assert_eq(result.body.result.text, "the final word",
    "FinalAnswer from inputs echoed as result.text")
  assert_eq(pending_count(), 0, "no pending after synchronous handler")
end

local function test_rga_for_reasoner_graph_passes_through_unrelated_kinds()
  -- Anything that isn't `<token>.run_node` must pass through unchanged
  -- so other consumers on the bus continue to see it. We verify by
  -- driving a non-run_node envelope through receive_msg and ensuring
  -- no synthesised events fired.
  reset_rga()
  dispatch_run_node({
    kind = "graph.run_started",
    run_id = "r",
  })
  assert_eq(#_test.calls(), 0, "no synthesised events for unrelated kinds")
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
  { name = "openai_adapter_static_token_injects_auth_set_on_ready", fn = test_openai_adapter_static_token_injects_auth_set_on_ready },
  { name = "openai_adapter_no_static_token_skips_injection", fn = test_openai_adapter_no_static_token_skips_injection },
  { name = "rga_dummy_dispatch_drives_provider_chain", fn = test_rga_dummy_dispatch_drives_provider_chain },
  { name = "rga_unknown_type_is_ignored", fn = test_rga_unknown_type_is_ignored },
  { name = "rga_dispatch_drives_provider_chat_create_and_complete", fn = test_rga_dispatch_drives_provider_chat_create_and_complete },
  { name = "rga_prev_state_chat_id_skips_create", fn = test_rga_prev_state_chat_id_skips_create },
  { name = "rga_provider_result_emits_tool_result_with_next_state", fn = test_rga_provider_result_emits_tool_result_with_next_state },
  { name = "rga_provider_result_for_unknown_chat_passes_through", fn = test_rga_provider_result_for_unknown_chat_passes_through },
  { name = "rga_per_firing_keying_distinct_firings_resolve_independently", fn = test_rga_per_firing_keying_distinct_firings_resolve_independently },
  { name = "rga_register_type_dispatches_custom_handler", fn = test_rga_register_type_dispatches_custom_handler },
  { name = "rga_terminal_handler_emits_tool_result_synchronously", fn = test_rga_terminal_handler_emits_tool_result_synchronously },
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
