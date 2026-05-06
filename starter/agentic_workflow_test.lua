-- starter/agentic_workflow_test.lua — unit tests for the agentic-loop
-- actor. The Rust harness (`crates/nefor/tests/starter_agentic_workflow_test.rs`)
-- installs a stub `nefor.*` surface (json + engine.* + log.* + bus.on_event)
-- so `require("agentic-loop")` succeeds, then loads this file. Tests
-- drive the actor's behaviour by:
--
--   * calling its public API directly (configure, submit, set_model,
--     cancel_all, build_template) — these short-circuit the bus and
--     test the orchestrator state machine in isolation;
--   * fabricating wire envelopes and feeding them to receive_msg
--     (replacement for the prior for_chat / for_reasoner_graph
--     factory tests).
--
-- The test surface is the same as before — `_test.fire_bus`,
-- `_test.calls`, `_test.set_plugins`, `_test.calls_clear` — so the
-- Rust harness needs no modifications.

local agentic_loop = require("agentic-loop")
local json = nefor.json

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ",
      tostring(expected), tostring(actual)), 2)
  end
end

-- Build a wire-shaped log entry the actor's receive_msg accepts.
-- Mirrors the engine's broker output: { ts, origin, payload } where
-- payload is JSON-encoded { type, body }.
local function make_entry(origin, body)
  return {
    ts      = "2026-05-04T00:00:00.000Z",
    origin  = origin,
    payload = json.encode({ type = "event", from = origin, body = body }),
  }
end

local function send_to_loop(origin, body)
  agentic_loop.receive_msg(make_entry(origin, body))
end

-- Configure with a known model so we can detect a runtime switch.
agentic_loop.configure {
  provider = "ollama",
  model    = "initial-model",
}

-- Sanity: build_template reflects the seeded model.
do
  local g = agentic_loop.build_template("hi")
  local saw = false
  for _, n in ipairs(g.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      saw = true
      assert_eq(n.args.model, "initial-model", "wrap node carries seeded model")
    end
  end
  assert(saw, "wrap node found in template")
end

-- chat.model.set with a non-empty model updates config.model.
do
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "ollama", model = "new-model" })
  local g = agentic_loop.build_template("hi")
  local saw = false
  for _, n in ipairs(g.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      saw = true
      assert_eq(n.args.model, "new-model", "wrap node carries updated model")
    end
  end
  assert(saw, "wrap node found in template after switch")
end

-- chat.model.set with an empty model is a no-op (no crash, no update).
do
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "ollama", model = "" })
  local g = agentic_loop.build_template("hi")
  for _, n in ipairs(g.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      assert_eq(n.args.model, "new-model", "empty-model set did not clobber config.model")
    end
  end
end

-- chat.model.set with the model field absent is also a no-op.
do
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "ollama" })
  local g = agentic_loop.build_template("hi")
  for _, n in ipairs(g.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      assert_eq(n.args.model, "new-model", "missing-model set did not clobber config.model")
    end
  end
end

-- A second switch updates config.model again.
do
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "ollama", model = "another-model" })
  local g = agentic_loop.build_template("hi")
  for _, n in ipairs(g.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      assert_eq(n.args.model, "another-model", "second switch sticks")
    end
  end
end

-- ------------------------------------------------------------------
-- session lifecycle (graph_skips_replay → broadcast chat.reset)
-- ------------------------------------------------------------------

-- session_end → emits chat.reset broadcast and graph.cancel for any
-- in-flight runs.
do
  _test.set_plugins({ "ollama", "reasoner-graph", "nefor-tui" })
  _test.calls_clear()

  send_to_loop("sessions", { kind = "sessions.session_end", session_id = "old-id" })

  local saw_reset = false
  for _, c in ipairs(_test.calls()) do
    local ok, decoded = pcall(json.decode, c.payload)
    if ok and type(decoded) == "table" and type(decoded.body) == "table"
       and decoded.body.kind == "chat.reset" then
      saw_reset = true
    end
  end
  assert(saw_reset, "session_end must broadcast chat.reset to clear provider+TUI state")
end

-- replay_mode flag flips during session_end → session_start window.
-- After resume_done the flag clears.
do
  _test.set_plugins({ "ollama", "reasoner-graph", "nefor-tui" })

  -- session_end (already entered replay-expectation gate above)
  send_to_loop("sessions", { kind = "sessions.session_start", session_id = "new-id" })
  assert_eq(agentic_loop.is_replay_mode(), true,
    "after session_end → session_start, replay_mode is true")

  send_to_loop("sessions", { kind = "sessions.resume_done", session_id = "new-id" })
  assert_eq(agentic_loop.is_replay_mode(), false,
    "after resume_done, replay_mode lifts")
end

-- ------------------------------------------------------------------
-- Issue 1 + Issue 3 — user message echo + busy-submit queue
-- ------------------------------------------------------------------

local function decode_calls()
  local out = {}
  for _, c in ipairs(_test.calls()) do
    local ok, decoded = pcall(json.decode, c.payload)
    if ok and type(decoded) == "table" and type(decoded.body) == "table" then
      out[#out + 1] = { body = decoded.body, target = c.target }
    end
  end
  return out
end

local function find_call(calls, kind, role, text_substr)
  for _, c in ipairs(calls) do
    if c.body.kind == kind
       and (role == nil or c.body.role == role)
       and (text_substr == nil
            or (type(c.body.text) == "string"
                and string.find(c.body.text, text_substr, 1, true) ~= nil)) then
      return c
    end
  end
  return nil
end

local function fresh_loop()
  agentic_loop._internals.reset()
  agentic_loop.configure { provider = "ollama", model = "test-model" }
  _test.set_plugins({ "ollama", "reasoner-graph", "nefor-tui" })
  _test.calls_clear()
end

-- (1.A) chat.input.submit emits chat.message.append role=user.
do
  fresh_loop()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "first prompt" })
  local calls = decode_calls()
  local user_echo = find_call(calls, "chat.message.append", "user", "first prompt")
  assert(user_echo ~= nil,
    "chat.input.submit must emit chat.message.append role=user (Issue 1 echo)")
  assert_eq(user_echo.target, "nefor-tui",
    "user echo must target nefor-tui specifically")
end

-- (3.A) Busy submit is queued, not rejected.
do
  fresh_loop()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "first" })
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "second" })
  local calls = decode_calls()
  local echo = find_call(calls, "chat.message.append", "user", "second")
  assert(echo ~= nil,
    "queued submit must still echo a chat.message.append role=user")
  local queued_note = find_call(calls, "chat.message.append", "system", "queued")
  assert(queued_note ~= nil,
    "queued submit must surface a [queued ...] system message; got " ..
    json.encode(calls))
  local busy_msg = find_call(calls, "chat.message.append", "system", "orchestrator busy")
  assert_eq(busy_msg, nil,
    "the rejected '[orchestrator busy ...]' must no longer appear")
end

-- (3.B) Two messages submitted back-to-back BOTH dispatch as the
-- orchestrator frees up. We drive graph.run_complete to flush.
do
  fresh_loop()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "alpha" })
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "beta" })

  local function find_run_emit(calls)
    for _, c in ipairs(calls) do
      if c.body.kind == "reasoner-graph.run" then
        return c.body.run_id
      end
    end
    return nil
  end
  local first_run = find_run_emit(decode_calls())
  assert(first_run ~= nil, "first submit dispatched a reasoner-graph.run")
  do
    local count = 0
    for _, c in ipairs(decode_calls()) do
      if c.body.kind == "reasoner-graph.run" then count = count + 1 end
    end
    assert_eq(count, 1, "second submit must NOT dispatch while first is in flight")
  end
  _test.calls_clear()
  -- Drive graph.run_complete success for the first run.
  send_to_loop("reasoner-graph", {
    kind   = "graph.run_complete",
    run_id = first_run,
    status = "success",
    results = {},
  })
  local second_run = find_run_emit(decode_calls())
  assert(second_run ~= nil,
    "queued submit must dispatch on graph.run_complete; sends were " ..
    json.encode(_test.calls()))
  assert(second_run ~= first_run,
    "second run must be a fresh run_id, got " .. tostring(second_run))
end

-- (3.C) chat.reset clears the pending-input queue.
do
  fresh_loop()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "first" })
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "queued-2" })
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "queued-3" })
  send_to_loop("nefor-tui", { kind = "chat.reset" })
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "fresh" })
  local function find_run_user_text(calls)
    for _, c in ipairs(calls) do
      if c.body.kind == "reasoner-graph.run" and type(c.body.graph) == "table" then
        for _, node in ipairs(c.body.graph.nodes or {}) do
          if node.id == "wrap" and type(node.args) == "table" then
            return node.args.prompt
          end
        end
      end
    end
    return nil
  end
  local user_text = find_run_user_text(decode_calls())
  assert_eq(user_text, "fresh",
    "after chat.reset, queue must be empty: dispatched user_text was '"
    .. tostring(user_text) .. "', expected 'fresh'")
end

-- (3.D) session_end teardown clears the queue.
do
  fresh_loop()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "first" })
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "stranded" })
  send_to_loop("sessions", { kind = "sessions.session_end", session_id = "old" })
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "post-swap" })
  for _, c in ipairs(decode_calls()) do
    if c.body.kind == "reasoner-graph.run" and type(c.body.graph) == "table" then
      for _, node in ipairs(c.body.graph.nodes or {}) do
        if node.id == "wrap" and type(node.args) == "table" then
          local txt = node.args.prompt
          assert(txt ~= "stranded",
            "session_end must clear pending_user_inputs; saw stranded dispatch")
        end
      end
    end
  end
end

-- ------------------------------------------------------------------
-- Issue 2 — cancel_all no longer emits the [interrupted: chat=...] line
-- ------------------------------------------------------------------
do
  agentic_loop._internals.reset()
  agentic_loop.configure { provider = "ollama", model = "test-model" }
  _test.set_plugins({ "ollama", "reasoner-graph", "nefor-tui" })
  _test.calls_clear()
  agentic_loop.cancel_all()
  for _, c in ipairs(decode_calls()) do
    if c.body.kind == "chat.message.append" and type(c.body.text) == "string" then
      assert(string.find(c.body.text, "interrupted: chat=", 1, true) == nil,
        "cancel_all must not emit '[interrupted: chat=...]' to the user; saw "
        .. c.body.text)
      assert(string.find(c.body.text, "sub-graphs=", 1, true) == nil,
        "cancel_all must not emit 'sub-graphs=' counter; saw " .. c.body.text)
    end
  end
end

print("agentic_workflow_test: ok")
