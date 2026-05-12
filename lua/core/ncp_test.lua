-- core/ncp_test.lua — unit tests for ncp.invoke_from_plugin / dispatch.
--
-- Loaded by `crates/nefor/tests/starter_ncp_test.rs`. The Rust test:
--   * Installs a mock `nefor.engine` that records every `send` and
--     `deliver` call (both into `_test.calls()`), accumulates a synthetic
--     bus log on each `send` (`_test.bus_log()`), and returns a
--     controllable plugin list from `plugins()`.
--   * Sets `package.path` so `require("core.ncp")` resolves from this directory.
--   * Loads and runs this file.
--
-- ## Driving model post-callback-refactor
--
-- The framework no longer auto-logs plugin emissions. Tests that simulate
-- "plugin sends a line" call `ncp.invoke_from_plugin(name, payload)`. The
-- framework decodes, handles ready handshake, or routes events to the
-- wrapper's `from_plugin` callback (or the default which republishes via
-- `nefor.engine.send`).
--
-- For tests that exercise the bus-event dispatch path (wrapper
-- `to_plugin` callbacks fire on each new bus entry), call
-- `ncp.dispatch(_test.bus_log())` after the inbound call so the dispatch
-- hook iterates the accumulated log.

local json = nefor.json
local ncp = require("core.ncp")

local function reset()
  ncp._reset()
  _test.calls_clear()
  _test.bus_log_clear()
  _test.set_plugins({})
end

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

-- Convenience: drive an inbound payload through the new
-- invoke_from_plugin entry point, then dispatch the accumulated bus log
-- so wrapper to_plugin callbacks fire (matches the production broker
-- ordering: invoke_from_plugin → drain → dispatch).
local function drive_inbound(origin, payload)
  ncp.invoke_from_plugin(origin, payload)
  ncp.dispatch(_test.bus_log())
end

-- Make a peer ready by driving a real ready handshake. Useful in tests
-- that want to skip past the handshake details and focus on event
-- behavior; equivalent to the old `dispatch_with(name, make_ready)`.
local function ready(name)
  drive_inbound(name, make_ready("0.1"))
end

local function ready_each(names)
  for _, n in ipairs(names) do
    ready(n)
  end
end

-- ------------------------------------------------------------------
-- 1. ready triggers ready_ok reply
-- ------------------------------------------------------------------
local function test_ready_triggers_ready_ok_reply()
  reset()
  _test.set_plugins({ "mock-plugin" })
  drive_inbound("mock-plugin", make_ready("0.1"))

  local calls = _test.calls()
  assert_eq(#calls, 1, "exactly one send/deliver on ready")
  assert_eq(calls[1].target, "mock-plugin", "reply targeted at readying plugin")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.type, "system", "system message")
  assert_eq(decoded.body.kind, "ready_ok", "kind=ready_ok")
  assert_true(type(decoded.body.engine_version) == "string",
    "engine_version present and a string")
end

local function test_ready_with_wrong_version_triggers_error()
  reset()
  _test.set_plugins({ "p" })
  drive_inbound("p", make_ready("0.9"))

  local calls = _test.calls()
  assert_eq(#calls, 1, "one call for error")
  assert_eq(calls[1].target, "p", "error targeted at sender")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.body.kind, "error", "error kind")
  assert_eq(decoded.body.code, "protocol_version_mismatch", "correct code")
end

local function test_malformed_ready_body_triggers_error()
  reset()
  _test.set_plugins({ "p" })
  local bad = json.encode({ type = "system", body = { kind = "ready" } })
  drive_inbound("p", bad)

  local calls = _test.calls()
  assert_eq(#calls, 1, "one error")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.body.code, "invalid_ready", "invalid_ready")
end

-- ------------------------------------------------------------------
-- event from ready plugin reaches OTHER ready peers via default to_plugin
-- ------------------------------------------------------------------
local function test_event_from_ready_plugin_broadcasts_to_others()
  reset()
  _test.set_plugins({ "a", "b", "c" })

  ready_each({ "a", "b", "c" })
  _test.calls_clear()

  -- 'a' emits an event. Default from_plugin republishes via send;
  -- dispatch then fires every wrapper's to_plugin (default = deliver to
  -- peer if env.from != peer). 'b' and 'c' should each receive a
  -- delivery; 'a' should not (default skips self).
  drive_inbound("a", make_event({ kind = "test.ping" }))

  local seen = { a = false, b = false, c = false }
  for _, c in ipairs(_test.calls()) do
    if c.target and seen[c.target] ~= nil then
      local d = json.decode(c.payload)
      if d and d.body and d.body.kind == "test.ping" then
        seen[c.target] = true
      end
    end
  end
  assert_eq(seen.b, true, "b received event")
  assert_eq(seen.c, true, "c received event")
  assert_eq(seen.a, false, "a (sender) did not receive its own event")
end

local function test_event_from_ready_plugin_excludes_sender()
  reset()
  _test.set_plugins({ "a", "b" })
  ready_each({ "a", "b" })
  _test.calls_clear()

  drive_inbound("a", make_event({ kind = "sub" }))

  for _, c in ipairs(_test.calls()) do
    if c.target == "a" then
      local d = json.decode(c.payload)
      assert_true(
        not (d and d.body and d.body.kind == "sub"),
        "sender 'a' must not receive its own event"
      )
    end
  end
end

-- ------------------------------------------------------------------
-- event from non-ready plugin is errored
-- ------------------------------------------------------------------
local function test_event_from_non_ready_plugin_is_errored()
  reset()
  _test.set_plugins({ "a", "b" })

  -- 'a' emits an event without readying first.
  drive_inbound("a", make_event({ kind = "x" }))

  local calls = _test.calls()
  assert_eq(#calls, 1, "one call: the error reply")
  assert_eq(calls[1].target, "a", "error targeted at offender")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.body.kind, "error", "error")
  assert_eq(decoded.body.code, "malformed_envelope", "malformed_envelope code")
end

local function test_malformed_json_triggers_error()
  reset()
  _test.set_plugins({ "p" })
  drive_inbound("p", "{not valid json")

  local calls = _test.calls()
  assert_eq(#calls, 1, "one call: error")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.body.code, "malformed_envelope", "malformed_envelope")
end

local function test_second_ready_from_same_plugin_errors()
  reset()
  _test.set_plugins({ "p" })
  drive_inbound("p", make_ready("0.1"))
  _test.calls_clear()

  drive_inbound("p", make_ready("0.1"))

  local calls = _test.calls()
  assert_eq(#calls, 1, "one call: the error")
  local decoded = json.decode(calls[1].payload)
  assert_eq(decoded.body.code, "invalid_ready", "invalid_ready code")
end

-- ------------------------------------------------------------------
-- late attacher receives prior events in order via replay-on-attach
-- ------------------------------------------------------------------
local function test_late_attacher_receives_prior_events_in_order()
  reset()
  _test.set_plugins({ "a" })

  ready("a")
  for _, k in ipairs({ "e1", "e2", "e3" }) do
    drive_inbound("a", make_event({ kind = k }))
  end

  -- 'b' joins and readies. Replay-on-attach calls b's default to_plugin
  -- with each prior bus entry; default delivers verbatim.
  _test.set_plugins({ "a", "b" })
  _test.calls_clear()
  drive_inbound("b", make_ready("0.1"))

  local replayed = {}
  for _, c in ipairs(_test.calls()) do
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
-- transforms: from_plugin callback decides what to publish
-- ------------------------------------------------------------------
local function test_from_plugin_callback_can_transform_kind()
  reset()
  _test.set_plugins({ "src", "dst" })

  -- src has a from_plugin callback that rewrites cc.* → chat.* and
  -- republishes via send. We register the wrapper on both ends so the
  -- dispatch can route to dst. Batched signature: envs is a list per
  -- invocation; the broker hands one inbound line at a time so this
  -- usually iterates a single-element list.
  ncp._test_set_transforms("src", {
    from_plugin = function(envs)
      for _, env in ipairs(envs) do
        if env.body and type(env.body.kind) == "string" then
          local k = env.body.kind
          if k:sub(1, 3) == "cc." then
            env.body.kind = "chat." .. k:sub(4)
          end
        end
        nefor.engine.send(json.encode({
          type = "event", from = "src",
          ts = nefor.engine.now(), body = env.body,
        }))
      end
    end,
  })

  ready_each({ "src", "dst" })
  _test.calls_clear()

  drive_inbound("src", make_event({ kind = "cc.stream.end", text = "hi" }))

  -- Find delivery to dst.
  local got
  for _, c in ipairs(_test.calls()) do
    if c.target == "dst" then
      local d = json.decode(c.payload)
      if d and d.type == "event" then got = d end
    end
  end
  assert_true(got ~= nil, "dst received delivered event")
  assert_eq(got.body.kind, "chat.stream.end", "kind rewritten by from_plugin")
  assert_eq(got.body.text, "hi", "body fields preserved")
  assert_eq(got.from, "src", "from preserved as origin plugin")
end

local function test_from_plugin_callback_can_drop()
  reset()
  _test.set_plugins({ "src", "dst" })

  -- A from_plugin that does nothing (no `send`) drops the envelope
  -- entirely from the bus. Batched signature still drops because it
  -- iterates envs and emits nothing.
  ncp._test_set_transforms("src", {
    from_plugin = function(_envs) end,
  })

  ready_each({ "src", "dst" })
  _test.calls_clear()

  drive_inbound("src", make_event({ kind = "any" }))

  for _, c in ipairs(_test.calls()) do
    if c.target == "dst" then
      local d = json.decode(c.payload)
      assert_true(
        not (d and d.body and d.body.kind == "any"),
        "no peer received the dropped event"
      )
    end
  end
end

local function test_to_plugin_callback_per_target_only()
  reset()
  _test.set_plugins({ "src", "a", "b" })

  -- Only 'a' has a to_plugin callback that rewrites kind before
  -- delivering. 'b' uses the default callback (delivers verbatim).
  -- Batched signature: iterate envs, mutate + deliver each.
  ncp._test_set_transforms("a", {
    to_plugin = function(envs)
      for _, env in ipairs(envs) do
        env.body.kind = "rewritten"
        nefor.engine.deliver("a", json.encode(env))
      end
    end,
  })

  ready_each({ "src", "a", "b" })
  _test.calls_clear()

  drive_inbound("src", make_event({ kind = "original" }))

  local seen = {}
  for _, c in ipairs(_test.calls()) do
    if c.target then
      local d = json.decode(c.payload)
      if d and d.type == "event" then
        seen[c.target] = d.body.kind
      end
    end
  end
  assert_eq(seen.a, "rewritten", "'a' saw rewritten event via to_plugin")
  assert_eq(seen.b, "original",  "'b' saw original event (default callback)")
end

local function test_to_plugin_callback_can_drop()
  reset()
  _test.set_plugins({ "src", "a", "b" })

  -- 'a's to_plugin does nothing → no delivery to a. 'b' uses default →
  -- delivery happens. Batched signature: iterating an empty list is
  -- the same as the no-op callback.
  ncp._test_set_transforms("a", {
    to_plugin = function(_envs) end,
  })

  ready_each({ "src", "a", "b" })
  _test.calls_clear()

  drive_inbound("src", make_event({ kind = "x" }))

  local targets = {}
  for _, c in ipairs(_test.calls()) do
    if c.target then
      local d = json.decode(c.payload)
      if d and d.type == "event" and d.body.kind == "x" then
        targets[c.target] = (targets[c.target] or 0) + 1
      end
    end
  end
  assert_eq(targets.a or 0, 0, "'a' was filtered by its to_plugin")
  assert_eq(targets.b or 0, 1, "'b' still received the event")
end

local function test_from_plugin_callback_error_emits_transform_error()
  reset()
  _test.set_plugins({ "src", "dst" })

  ncp._test_set_transforms("src", {
    from_plugin = function(_envs) error("boom") end,
  })

  ready_each({ "src", "dst" })
  _test.calls_clear()

  drive_inbound("src", make_event({ kind = "x" }))

  -- The framework reports a transform_error back to the source via
  -- deliver. (Same shape as before; only the trigger differs.)
  local found
  for _, c in ipairs(_test.calls()) do
    if c.target == "src" then
      local d = json.decode(c.payload)
      if d and d.body and d.body.kind == "error" then found = d end
    end
  end
  assert_true(found ~= nil, "transform_error reply to source")
  assert_eq(found.body.code, "transform_error", "transform_error code")
end

local function test_replayed_events_pass_through_from_plugin_callback()
  reset()
  _test.set_plugins({ "src" })

  -- src republishes events with rewritten kind. Late attacher should
  -- see the *published* (already-rewritten) envelopes during replay —
  -- replay-on-attach walks the bus log and re-delivers. Since the bus
  -- log holds the post-callback envelopes (what was published), the
  -- replayed events naturally carry the rewritten kind.
  ncp._test_set_transforms("src", {
    from_plugin = function(envs)
      for _, env in ipairs(envs) do
        env.body.kind = "rewritten"
        nefor.engine.send(json.encode({
          type = "event", from = "src",
          ts = nefor.engine.now(), body = env.body,
        }))
      end
    end,
  })

  ready("src")
  for _, k in ipairs({ "e1", "e2" }) do
    drive_inbound("src", make_event({ kind = k }))
  end

  _test.set_plugins({ "src", "late" })
  _test.calls_clear()
  drive_inbound("late", make_ready("0.1"))

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
  assert_eq(replayed_kinds[1], "rewritten", "first replay carries rewrite")
  assert_eq(replayed_kinds[2], "rewritten", "second replay carries rewrite")
end

-- ------------------------------------------------------------------
-- targeted routing: kind "<peer>.<rest>" delivers only to <peer>
-- ------------------------------------------------------------------
local function test_kind_prefix_targets_named_peer_only()
  reset()
  _test.set_plugins({ "src", "nefor-tui", "other" })

  ready_each({ "src", "nefor-tui", "other" })
  _test.calls_clear()

  -- src emits "nefor-tui.grid.line"; the default to_plugin's prefix
  -- routing skips delivery to peers whose name doesn't match the
  -- prefix.
  drive_inbound("src", make_event({ kind = "nefor-tui.grid.line", row = 0 }))

  local targets = {}
  for _, c in ipairs(_test.calls()) do
    if c.target then
      local d = json.decode(c.payload)
      if d and d.type == "event" and d.body.kind == "nefor-tui.grid.line" then
        targets[c.target] = (targets[c.target] or 0) + 1
      end
    end
  end
  assert_eq(targets["nefor-tui"] or 0, 1, "nefor-tui got the targeted event")
  assert_eq(targets["other"] or 0, 0, "'other' did not receive targeted event")
  assert_eq(targets["src"] or 0, 0, "sender did not receive its own event")
end

local function test_kind_prefix_self_announces_to_all_peers()
  reset()
  _test.set_plugins({ "nefor-tui", "a", "b" })

  ready_each({ "nefor-tui", "a", "b" })
  _test.calls_clear()

  -- nefor-tui announces "nefor-tui.ready" — prefix == sender, so the
  -- default to_plugin treats it as a regular broadcast.
  drive_inbound("nefor-tui", make_event({ kind = "nefor-tui.ready" }))

  local targets = {}
  for _, c in ipairs(_test.calls()) do
    if c.target then
      local d = json.decode(c.payload)
      if d and d.type == "event" and d.body.kind == "nefor-tui.ready" then
        targets[c.target] = (targets[c.target] or 0) + 1
      end
    end
  end
  assert_eq(targets["a"] or 0, 1, "'a' got the announcement")
  assert_eq(targets["b"] or 0, 1, "'b' got the announcement")
  assert_eq(targets["nefor-tui"] or 0, 0, "sender excluded from broadcast")
end

-- ------------------------------------------------------------------
-- openai-provider wrapper: static_token injects auth.set on ready
-- ------------------------------------------------------------------
--
-- The wrapper's `from_plugin` is now a side-effecting callback, not a
-- transform. We exercise it by calling the callback directly with a
-- pre-decoded envelope; assertion is on what `_test.calls()` recorded.

local agentic_loop_mod = require("agentic-loop")
local openai_provider  = require("provider")

local function build_provider_chain(name, opts)
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
  ad.from_plugin({ {
    type = "event", from = "ollama",
    body = { kind = "ollama.hello" },
  } })
  -- hello + default republish: 1 send (broadcast). Strip those for the
  -- injection assertion.
  _test.calls_clear()

  ad.from_plugin({ {
    type = "event", from = "ollama",
    body = { kind = "ollama.ready" },
  } })

  -- The wrapper synthesizes a targeted send (auth.set to ollama) and
  -- drops the ready (no republish). Assert the targeted send is there.
  local injected
  for _, c in ipairs(_test.calls()) do
    if c.target == "ollama" then
      local d = json.decode(c.payload)
      if d and d.body and d.body.kind == "ollama.auth.set" then
        injected = d
      end
    end
  end
  assert_true(injected ~= nil, "auth.set targeted at ollama")
  assert_eq(injected.body.token, "local", "token carries static_token value")

  -- A second ready must not re-inject.
  _test.calls_clear()
  ad.from_plugin({ {
    type = "event", from = "ollama",
    body = { kind = "ollama.ready" },
  } })
  for _, c in ipairs(_test.calls()) do
    local d = json.decode(c.payload)
    assert_true(
      not (d and d.body and d.body.kind == "ollama.auth.set"),
      "second ready must not re-inject auth.set"
    )
  end
end

local function test_openai_adapter_no_static_token_skips_injection()
  reset()
  reset_loop()
  local ad = build_provider_chain("ollama")
  ad.from_plugin({ {
    type = "event", from = "ollama",
    body = { kind = "ollama.ready" },
  } })
  for _, c in ipairs(_test.calls()) do
    local d = json.decode(c.payload)
    assert_true(
      not (d and d.body and d.body.kind == "ollama.auth.set"),
      "no static_token → no injection"
    )
  end
end

-- ------------------------------------------------------------------
-- reasoners actor + openai-provider wrapper
-- ------------------------------------------------------------------

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

local function pending_count()
  local n = 0
  for _ in pairs(agentic_loop_mod._internals.state.pending) do n = n + 1 end
  return n
end

local function test_rga_dummy_dispatch_drives_provider_chain()
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
  reset_rga()
  _test.set_plugins({ "x" })
  dispatch_run_node({
    kind = "no-such-type.run_node",
    run_id = "r9", node_id = "nx", firing_id = "fx",
    args = {}, inputs = {},
  })

  assert_eq(#_test.calls(), 0,
    "unknown reasoner type emits nothing")
end

local function test_rga_dispatch_drives_provider_chat_create_and_complete()
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
  reset_rga()
  _test.set_plugins({ "ollama" })
  dispatch_run_node({
    kind = "provider-wrapper.run_node",
    run_id = "r3", node_id = "wrap", firing_id = "f-second",
    args = { provider = "ollama" },
    inputs = { up = { output = { messages = { { role = "user", content = "again" } } } } },
    prev_state = { chat_id = "chat-existing" },
  })

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
  prov_hook.from_plugin({ {
    type = "event", from = "ollama",
    body = {
      kind = "ollama.chat.complete.result",
      chat_id = chat_id,
      output = { text = "the answer" },
    },
  } })

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
  reset_rga()
  local prov_hook = build_provider_chain("ollama")
  prov_hook.from_plugin({ {
    type = "event", from = "ollama",
    body = {
      kind = "ollama.chat.complete.result",
      chat_id = "not-ours",
      output = { text = "..." },
    },
  } })
  -- The wrapper's outer_from passes ollama.chat.complete.result through
  -- and republishes it (no firing matched). Bus should not see a
  -- tool.result.
  for _, c in ipairs(_test.calls()) do
    local d = json.decode(c.payload)
    assert_true(
      not (d and d.body and d.body.kind == "tool.result"),
      "no tool.result emitted for unknown chat"
    )
  end
end

local function test_rga_per_firing_keying_distinct_firings_resolve_independently()
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
  prov.from_plugin({ {
    type = "event", from = "ollama",
    body = {
      kind = "ollama.chat.complete.result",
      chat_id = chat_b,
      output = { text = "B-ans" },
    },
  } })
  local resB = find_call_with_kind("tool.result")
  assert_true(resB ~= nil, "result for B fired")
  assert_eq(resB.body.id, "fB", "result correlated to firing B")
  _test.calls_clear()

  prov.from_plugin({ {
    type = "event", from = "ollama",
    body = {
      kind = "ollama.chat.complete.result",
      chat_id = chat_a,
      output = { text = "A-ans" },
    },
  } })
  local resA = find_call_with_kind("tool.result")
  assert_true(resA ~= nil, "result for A fired")
  assert_eq(resA.body.id, "fA", "result correlated to firing A")
  assert_eq(pending_count(), 0,
    "all pending firings resolved → pending_count == 0")
end

local function test_rga_register_type_dispatches_custom_handler()
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

  reasoners._internals.handlers["my-leaf"] = nil
end

local function test_rga_terminal_handler_emits_tool_result_synchronously()
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
local spawn_calls = {}
local function install_spawn_stub()
  spawn_calls = {}
  nefor.plugins = {
    spawn = function(opts)
      table.insert(spawn_calls, opts)
    end,
  }
end

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
    { name = "p", command = { "/bin/echo" }, env = { FOO = "bar" } },
    "unknown field 'env'", "env field rejected")
  local _, err = pcall(ncp.spawn, {
    name = "p", command = { "/bin/echo" }, env = { FOO = "bar" },
  })
  assert_true(tostring(err):find("command array", 1, true) ~= nil,
    "env hint mentions command array")
  assert_eq(#spawn_calls, 0, "engine spawn must not be invoked on rejection")
end

local function test_spawn_rejects_args_field_with_hint()
  reset()
  install_spawn_stub()
  local err = assert_spawn_errors_with(
    { name = "p", command = { "/bin/echo" }, args = { "--flag" } },
    "unknown field 'args'", "args field rejected")
  assert_true(err:find("command array", 1, true) ~= nil,
    "args hint mentions command array")
end

local function test_spawn_rejects_cwd_field_with_hint()
  reset()
  install_spawn_stub()
  local err = assert_spawn_errors_with(
    { name = "p", command = { "/bin/echo" }, cwd = "/tmp" },
    "unknown field 'cwd'", "cwd field rejected")
  assert_true(err:find("<plugin-dir>/<name>", 1, true) ~= nil,
    "cwd hint mentions plugin-dir/name policy")
end

local function test_spawn_rejects_unknown_field()
  reset()
  install_spawn_stub()
  assert_spawn_errors_with(
    { name = "p", command = { "/bin/echo" }, mystery = 42 },
    "unknown field 'mystery'", "unknown field rejected")
end

local function test_spawn_accepts_the_four_valid_fields_without_error()
  reset()
  install_spawn_stub()
  ncp.spawn({
    name        = "p",
    command     = { "/bin/echo", "hi" },
    from_plugin = function(envs) end,
    to_plugin   = function(envs) end,
  })
  assert_eq(#spawn_calls, 1, "engine spawn invoked once")
  assert_eq(spawn_calls[1].name, "p", "name forwarded")
  assert_true(type(spawn_calls[1].command) == "table",
    "command forwarded as a table")
  assert_eq(spawn_calls[1].command[1], "/bin/echo", "command[1] preserved")
  assert_eq(spawn_calls[1].from_plugin, nil,
    "from_plugin must not leak to engine spawn")
  assert_eq(spawn_calls[1].to_plugin, nil,
    "to_plugin must not leak to engine spawn")
end

-- ------------------------------------------------------------------
-- engine envelopes (origin = engine) → chat.popup
-- ------------------------------------------------------------------

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
  _test.set_plugins({ "nefor-tui" })
  ready("nefor-tui")
  _test.calls_clear()

  local payload = make_engine_plugin_failed(
    "ollama", "spawn", "binary not found", "missing_dir"
  )
  ncp.dispatch({ entry_engine(payload) })

  local popups = {}
  for _, c in ipairs(_test.calls()) do
    local d = json.decode(c.payload)
    if d and d.body and d.body.kind == "chat.popup" then
      popups[#popups + 1] = c
    end
  end
  assert_eq(#popups, 1, "exactly one chat.popup")
  assert_eq(popups[1].target, "nefor-tui", "popup targeted at nefor-tui")
  local decoded = json.decode(popups[1].payload)
  assert_eq(decoded.body.level, "error", "level=error")
  assert_eq(decoded.body.source, "engine", "source=engine")
  assert_true(decoded.body.message:find("ollama", 1, true) ~= nil,
    "message includes plugin name")
  assert_true(decoded.body.message:find("binary not found", 1, true) ~= nil,
    "message includes reason")
  assert_true(decoded.body.message:find("spawn", 1, true) ~= nil,
    "message includes phase")
end

local function test_engine_plugin_failed_drops_when_chat_not_connected()
  reset()
  _test.set_plugins({ "ollama" })
  ncp.dispatch({ entry_engine(make_engine_plugin_failed(
    "nefor-tui", "runtime", "crashed", "crash"
  )) })

  for _, c in ipairs(_test.calls()) do
    local d = json.decode(c.payload)
    assert_true(
      not (d and d.body and d.body.kind == "chat.popup"),
      "no popup when nefor-tui isn't on the bus"
    )
  end
end

local function test_engine_plugin_failed_buffers_until_chat_readies()
  reset()
  _test.set_plugins({ "nefor-tui" })

  ncp.dispatch({ entry_engine(make_engine_plugin_failed(
    "ollama", "spawn", "binary not found", "missing_dir"
  )) })

  -- No popup yet — chat hasn't readied.
  for _, c in ipairs(_test.calls()) do
    local d = json.decode(c.payload)
    assert_true(
      not (d and d.body and d.body.kind == "chat.popup"),
      "no popup while nefor-tui is pre-ready"
    )
  end

  -- Now chat readies — handshake reply + buffered popup must both fire.
  drive_inbound("nefor-tui", make_ready("0.1"))

  local saw_ready_ok = false
  local saw_popup = false
  for _, c in ipairs(_test.calls()) do
    if c.target == "nefor-tui" then
      local d = json.decode(c.payload)
      if d and d.body then
        if d.body.kind == "ready_ok" then saw_ready_ok = true end
        if d.body.kind == "chat.popup" then saw_popup = true end
      end
    end
  end
  assert_true(saw_ready_ok, "ready_ok delivered")
  assert_true(saw_popup, "buffered popup flushed after ready")
end

-- ------------------------------------------------------------------
-- Bug 2 regression — tool.permission_response wire-side delivery
-- ------------------------------------------------------------------
--
-- The chat unit-test (nefor-tui chat_test.rs) verifies that pressing
-- the approve key emits `{kind=tool.permission_response, decision="approve"}`
-- on the egress queue. The Rust unit-test in plugins/tool-gate/src/main.rs
-- verifies that the binary's `handle_permission_response` calls
-- forward_to_source when decision="approve". The wire between them —
-- ncp.lua's dispatch + tool-gate's wrapper to_plugin — has no
-- intermediate transform: the response must round-trip from the chat's
-- bus emission to tool-gate's stdin verbatim, with the `decision` field
-- intact. This test locks that contract.
--
-- Two paths matter independently:
--   * the chat's approve emission lands at tool-gate's stdin with
--     `decision = "approve"`.
--   * the chat's deny emission lands with `decision = "deny"`. (Pinning
--     both stops a future "swap approve/deny somewhere" regression
--     from passing this test by accident.)

local tools_mod = require("tools")

local function spawn_tool_gate_wrapper()
  local spec = tools_mod.gate_spec("tool-gate", { "/bin/true" })
  -- Skip the engine-spawn round-trip — register the transforms
  -- directly so we don't try to fork a real binary.
  ncp._test_set_transforms("tool-gate", {
    from_plugin = spec.from_plugin,
    to_plugin   = spec.to_plugin,
  })
end

local function find_deliver_to(target, kind)
  for _, c in ipairs(_test.calls()) do
    if c.target == target then
      local d = json.decode(c.payload)
      if d and d.body and d.body.kind == kind then
        return d
      end
    end
  end
  return nil
end

local function test_tool_permission_response_approve_round_trips_to_gate()
  reset()
  _test.set_plugins({ "tool-gate", "nefor-tui" })
  ready_each({ "tool-gate", "nefor-tui" })
  spawn_tool_gate_wrapper()
  _test.calls_clear()

  drive_inbound("nefor-tui", make_event({
    kind     = "tool.permission_response",
    id       = "outer-id-7",
    decision = "approve",
  }))

  local delivered = find_deliver_to("tool-gate", "tool.permission_response")
  assert_true(delivered ~= nil,
    "tool.permission_response must reach tool-gate's stdin")
  assert_eq(delivered.body.decision, "approve",
    "decision must round-trip as `approve` — bug 2 regression")
  assert_eq(delivered.body.id, "outer-id-7",
    "id must round-trip unchanged so tool-gate matches awaiting_approval")
end

local function test_tool_permission_response_deny_round_trips_to_gate()
  reset()
  _test.set_plugins({ "tool-gate", "nefor-tui" })
  ready_each({ "tool-gate", "nefor-tui" })
  spawn_tool_gate_wrapper()
  _test.calls_clear()

  drive_inbound("nefor-tui", make_event({
    kind     = "tool.permission_response",
    id       = "outer-id-9",
    decision = "deny",
  }))

  local delivered = find_deliver_to("tool-gate", "tool.permission_response")
  assert_true(delivered ~= nil,
    "tool.permission_response (deny) must reach tool-gate's stdin")
  assert_eq(delivered.body.decision, "deny",
    "deny must round-trip as `deny`, not silently flipped to approve")
end

-- Pin that the wrapper's replay-window guard suppresses delivery while
-- a session is replaying — a stale approve from the recorded log must
-- not double-fire the Rust gate's awaiting_approval lookup. Captures
-- the per-wrapper replay-skip contract introduced by the Phase-4.5
-- callback refactor.
local function test_tool_permission_response_dropped_during_replay()
  reset()
  _test.set_plugins({ "tool-gate", "nefor-tui" })
  ready_each({ "tool-gate", "nefor-tui" })
  spawn_tool_gate_wrapper()

  -- Open the replay window on the bus the way sessions does.
  local replay_window = require("core.history_replay")
  replay_window._set(true)
  _test.calls_clear()

  drive_inbound("nefor-tui", make_event({
    kind     = "tool.permission_response",
    id       = "outer-id-replay",
    decision = "approve",
  }))

  local delivered = find_deliver_to("tool-gate", "tool.permission_response")
  assert_true(delivered == nil,
    "replay window must suppress permission_response delivery to the gate")
  replay_window._set(false)
end

-- Bug 5 regression — the replay-window flag must be active for the
-- per-entry to_plugin loop on every entry between sessions.replay.start
-- and sessions.replay.end. The old setup deferred the flag flip to a
-- nefor.bus.on_event subscriber that fires AFTER the entire batch's
-- to_plugin pass (vm.rs `drain_pending_dispatch`); replayed envelopes
-- riding in the same batch as the framing markers would all see
-- replay_window.active() == false and reach the Rust peers as if
-- fresh. ncp.dispatch now toggles the flag inline based on the framing
-- marker entries.
--
-- The test simulates the production scenario: dispatch sees a batch
-- containing [replay.start, tool-gate.tool.invoke, replay.end] in
-- order. The replayed tool-invoke must NOT reach tool-gate's stdin —
-- if it did, the binary would treat it as fresh, decide Prompt, and
-- emit a duplicate chat.tool.permission_request after the window
-- closes (the actual user-visible Bug 5 symptom on /resume).
local function test_replay_window_suppresses_replayed_tool_invoke_in_same_batch()
  reset()
  _test.set_plugins({ "tool-gate", "nefor-tui", "ollama" })
  ready_each({ "tool-gate", "nefor-tui", "ollama" })
  spawn_tool_gate_wrapper()
  -- Force the flag to a known starting state in case a prior test
  -- left it set.
  local replay_window = require("core.history_replay")
  replay_window.set(false)

  -- Bypass invoke_from_plugin's "must be a ready peer" check by
  -- pushing the entries directly onto the test bus log via
  -- `nefor.engine.send`. Each entry rides as a step-origin emission
  -- the way sessions's send_msg + replay_envelope path produces them.
  local function send_step(from, body)
    nefor.engine.send(json.encode({
      type = "event",
      from = from,
      ts   = "2026-05-04T00:00:00.000Z",
      body = body,
    }))
  end
  send_step("sessions", { kind = "sessions.replay.start", session_id = "s", count = 1 })
  send_step("ollama",   { kind = "tool-gate.tool.invoke",
                          id   = "outer-replay",
                          name = "spawn_graph",
                          args = { graph = { nodes = {}, edges = {} } } })
  send_step("sessions", { kind = "sessions.replay.end",   session_id = "s" })
  _test.calls_clear()

  ncp.dispatch(_test.bus_log())

  local delivered = find_deliver_to("tool-gate", "tool-gate.tool.invoke")
  assert_true(delivered == nil,
    "replayed tool-gate.tool.invoke must NOT reach tool-gate's stdin during the replay window — Bug 5 regression")
  -- And the flag must be released after the end-marker entry runs.
  assert_eq(replay_window.active(), false,
    "replay_window.active() must be false after the replay.end entry's to_plugin pass")
end

-- Bug B regression — when tool-gate emits `tool.result { id, error }`
-- (deny / policy / unknown-tool path) the wrapper's `from_plugin`
-- correlates the tool_id back to the executor pending entry and emits
-- a `chat.tool.end` to nefor-tui. The earlier shape dropped the error
-- string and emitted `output = ""`, leaving the chat surface to
-- render an empty `output:` row under the tool block — visually
-- indistinguishable from "still running" for the user. Mirror the
-- openai-provider `chat_tool_end_body` contract: when the result
-- carries an error, the chat-side `output` field carries the error
-- message so the tool block can label it `error:` and surface what
-- happened.
local function test_tool_gate_wrapper_forwards_error_message_to_chat_tool_end()
  reset()
  reset_loop()
  _test.set_plugins({ "tool-gate", "nefor-tui" })
  ready_each({ "tool-gate", "nefor-tui" })
  spawn_tool_gate_wrapper()

  -- Register a pending tool-executor entry the wrapper can correlate
  -- back to. Mirrors the reasoner's `tool-executor` dispatch shape.
  agentic_loop_mod.track_tool_executor(
    "run-1", "node-1", "firing-1",
    { { id = "call_mock_ls", name = "bash", arguments = { command = "ls -la" } } },
    { "tool-1" }
  )
  _test.calls_clear()

  drive_inbound("tool-gate", make_event({
    kind  = "tool.result",
    id    = "tool-1",
    error = "tool `bash` denied by user",
  }))

  local delivered = find_deliver_to("nefor-tui", "chat.tool.end")
  assert_true(delivered ~= nil,
    "tool-gate wrapper must emit chat.tool.end on tool.result correlation")
  assert_eq(delivered.body.id, "call_mock_ls",
    "chat.tool.end carries the model_call_id from the executor entry")
  assert_eq(delivered.body.error, true,
    "error flag must be true when the result carries an error string")
  assert_eq(delivered.body.output, "tool `bash` denied by user",
    "Bug B: error message must land in the `output` field so the chat tool block can render it under `error:` instead of an empty `output:` line")
end

-- Pin the symmetric guarantee: nefor-tui DOES still receive replayed
-- envelopes during the window, because the TUI surface needs them to
-- repaint the transcript on resume. The chat.lua reducer gates side
-- effects (popup, dag observation) with its own state.replay_mode
-- flag — that's the level where "replay vs live" rendering policy
-- lives, not the wrapper.
local function test_replay_window_does_not_starve_nefor_tui()
  reset()
  _test.set_plugins({ "tool-gate", "nefor-tui", "ollama" })
  ready_each({ "tool-gate", "nefor-tui", "ollama" })
  spawn_tool_gate_wrapper()
  local replay_window = require("core.history_replay")
  replay_window.set(false)

  local function send_step(from, body)
    nefor.engine.send(json.encode({
      type = "event",
      from = from,
      ts   = "2026-05-04T00:00:00.000Z",
      body = body,
    }))
  end
  send_step("sessions", { kind = "sessions.replay.start", session_id = "s", count = 1 })
  send_step("ollama",   { kind = "chat.message.append", role = "user", text = "hi" })
  send_step("sessions", { kind = "sessions.replay.end",   session_id = "s" })
  _test.calls_clear()

  ncp.dispatch(_test.bus_log())

  local delivered = find_deliver_to("nefor-tui", "chat.message.append")
  assert_true(delivered ~= nil,
    "replayed chat.message.append MUST reach nefor-tui's stdin so chat.lua repaints the transcript on resume")
end

-- Batch-dispatch contract — `to_plugin(envs)` fires ONCE per dispatch
-- tick with all envelopes destined for the peer in the new tail. The
-- batch protocol refactor's headline guarantee: N envelopes pumped
-- through the dispatch loop in a single tick reach the wrapper as a
-- single invocation with N elements, not N invocations of one element
-- each. This unblocks Phase B's resume-rendering optimisation
-- (chat.lua coalesces N replayed deltas into one render pass) and
-- amortises wrapper-side translation cost across live bursts.
--
-- Pre-refactor (per-envelope dispatch) the wrapper's to_plugin was
-- called once per Step entry; converting that loop to send a list is
-- the dispatch-loop change. This test pins the new contract: 5
-- envelopes, 1 to_plugin call, 5 elements in `envs`.
local function test_to_plugin_receives_full_batch_in_one_call()
  reset()
  _test.set_plugins({ "src", "peer" })

  -- Record the size of `envs` for every to_plugin invocation.
  local invocation_sizes = {}
  ncp._test_set_transforms("peer", {
    to_plugin = function(envs)
      invocation_sizes[#invocation_sizes + 1] = #envs
    end,
  })

  ready_each({ "src", "peer" })
  _test.calls_clear()

  -- Pump 5 envelopes through src's from_plugin (default callback
  -- republishes via send → bus log gets 5 entries before the broker
  -- drains to dispatch). We bypass `drive_inbound` here so the
  -- accumulated bus log is dispatched as a single tick rather than one
  -- entry per call.
  for i = 1, 5 do
    ncp.invoke_from_plugin("src", make_event({ kind = "test.batch", n = i }))
  end
  -- A single dispatch call walks the whole new tail and fans out per
  -- peer in one to_plugin invocation.
  ncp.dispatch(_test.bus_log())

  assert_eq(#invocation_sizes, 1,
    "to_plugin must fire ONCE for the whole batch — pre-refactor's per-envelope dispatch fired 5 times for 5 envelopes")
  assert_eq(invocation_sizes[1], 5,
    "the single invocation's envs list must contain all 5 envelopes")
end

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
  { name = "from_plugin_callback_can_transform_kind", fn = test_from_plugin_callback_can_transform_kind },
  { name = "from_plugin_callback_can_drop", fn = test_from_plugin_callback_can_drop },
  { name = "to_plugin_callback_per_target_only", fn = test_to_plugin_callback_per_target_only },
  { name = "to_plugin_callback_can_drop", fn = test_to_plugin_callback_can_drop },
  { name = "from_plugin_callback_error_emits_transform_error", fn = test_from_plugin_callback_error_emits_transform_error },
  { name = "replayed_events_pass_through_from_plugin_callback", fn = test_replayed_events_pass_through_from_plugin_callback },
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
  { name = "engine_plugin_failed_buffers_until_chat_readies", fn = test_engine_plugin_failed_buffers_until_chat_readies },
  { name = "tool_permission_response_approve_round_trips_to_gate", fn = test_tool_permission_response_approve_round_trips_to_gate },
  { name = "tool_permission_response_deny_round_trips_to_gate", fn = test_tool_permission_response_deny_round_trips_to_gate },
  { name = "tool_permission_response_dropped_during_replay", fn = test_tool_permission_response_dropped_during_replay },
  { name = "replay_window_suppresses_replayed_tool_invoke_in_same_batch", fn = test_replay_window_suppresses_replayed_tool_invoke_in_same_batch },
  { name = "replay_window_does_not_starve_nefor_tui", fn = test_replay_window_does_not_starve_nefor_tui },
  { name = "tool_gate_wrapper_forwards_error_message_to_chat_tool_end", fn = test_tool_gate_wrapper_forwards_error_message_to_chat_tool_end },
  { name = "to_plugin_receives_full_batch_in_one_call", fn = test_to_plugin_receives_full_batch_in_one_call },
}

for _, t in ipairs(tests) do
  local ok, err = pcall(t.fn)
  if not ok then
    error("test '" .. t.name .. "' FAILED:\n" .. tostring(err), 0)
  end
end
