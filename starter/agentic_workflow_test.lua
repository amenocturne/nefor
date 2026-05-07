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

-- sessions.session_end (delivered via the bus) → emits chat.reset
-- broadcast and graph.cancel for any in-flight runs. Phase 4.5 moved
-- the trigger from receive_msg to a `nefor.bus.on_event` subscriber;
-- the test drives it through `_test.fire_bus` accordingly.
do
  _test.set_plugins({ "ollama", "reasoner-graph", "nefor-tui" })
  _test.calls_clear()

  _test.fire_bus("sessions.session_end", { session_id = "old-id" })

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

-- Replay-window gating now lives in `lib/replay_window`, driven by
-- `sessions.replay.start` / `sessions.replay.end` framing markers.
-- agentic-loop short-circuits inside `receive_msg` based on the
-- module's `active()` getter; the test asserts the flip end-to-end by
-- firing the markers and probing the gate.
do
  local replay_window = require("lib.replay_window")
  _test.set_plugins({ "ollama", "reasoner-graph", "nefor-tui" })

  _test.fire_bus("sessions.replay.start", { session_id = "new-id", count = 0 })
  assert_eq(replay_window.active(), true,
    "after replay.start, replay_window is active")

  _test.fire_bus("sessions.replay.end", { session_id = "new-id" })
  assert_eq(replay_window.active(), false,
    "after replay.end, replay_window lifts")
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
-- orchestrator frees up. We drive the canonical run-close
-- `tool.result { id=run_id, result: { status, results } }` to flush.
do
  fresh_loop()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "alpha" })
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "beta" })

  -- Submission rides on the canonical tool contract:
  -- `tool.invoke { id=run_id, name="spawn_graph", args: { graph, on_node_failure } }`.
  local function find_run_emit(calls)
    for _, c in ipairs(calls) do
      if c.body.kind == "tool.invoke" and c.body.name == "spawn_graph" then
        return c.body.id
      end
    end
    return nil
  end
  local first_run = find_run_emit(decode_calls())
  assert(first_run ~= nil, "first submit dispatched a spawn_graph tool.invoke")
  do
    local count = 0
    for _, c in ipairs(decode_calls()) do
      if c.body.kind == "tool.invoke" and c.body.name == "spawn_graph" then
        count = count + 1
      end
    end
    assert_eq(count, 1, "second submit must NOT dispatch while first is in flight")
  end
  _test.calls_clear()
  -- Drive the canonical run-close for the first run.
  send_to_loop("reasoner-graph", {
    kind   = "tool.result",
    id     = first_run,
    result = { status = "success", results = {} },
  })
  local second_run = find_run_emit(decode_calls())
  assert(second_run ~= nil,
    "queued submit must dispatch on tool.result run-close; sends were " ..
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
      if c.body.kind == "tool.invoke"
          and c.body.name == "spawn_graph"
          and type(c.body.args) == "table"
          and type(c.body.args.graph) == "table" then
        for _, node in ipairs(c.body.args.graph.nodes or {}) do
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
  _test.fire_bus("sessions.session_end", { session_id = "old" })
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "post-swap" })
  for _, c in ipairs(decode_calls()) do
    if c.body.kind == "tool.invoke"
        and c.body.name == "spawn_graph"
        and type(c.body.args) == "table"
        and type(c.body.args.graph) == "table" then
      for _, node in ipairs(c.body.args.graph.nodes or {}) do
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

-- ------------------------------------------------------------------
-- Bug 3 regression — chat.input.submit during replay must not
-- re-spawn an orchestrator graph. Sessions replays the user's
-- original submit envelope when a session resumes; agentic-loop
-- already saw the answer in the prior run, so re-firing the
-- handler would spawn a fresh graph and re-invoke the model on
-- exactly the same prompt. State is rebuilt by pure-Lua actors
-- watching the bus markers; replayed wire envelopes are observation
-- only, not new orchestration triggers.
-- ------------------------------------------------------------------
do
  fresh_loop()
  _test.fire_bus("sessions.replay.start", { session_id = "resumed", count = 0 })
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "first prompt" })
  for _, c in ipairs(decode_calls()) do
    assert(not (c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"),
      "chat.input.submit during replay must NOT dispatch a fresh spawn_graph; got "
      .. json.encode(c.body))
    assert(not (c.body.kind == "chat.message.append" and c.body.role == "user"),
      "chat.input.submit during replay must NOT echo a user message; got "
      .. json.encode(c.body))
  end
  _test.fire_bus("sessions.replay.end", { session_id = "resumed" })

  -- After replay ends, the actor returns to live behaviour: a fresh
  -- submit dispatches normally. Locks in that the gate is window-
  -- scoped, not a hard mute.
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "post-replay" })
  local saw_dispatch = false
  for _, c in ipairs(decode_calls()) do
    if c.body.kind == "tool.invoke" and c.body.name == "spawn_graph" then
      saw_dispatch = true
    end
  end
  assert(saw_dispatch,
    "after replay.end, live chat.input.submit must dispatch a spawn_graph again")
end

-- chat.reset / chat.interrupt_all / chat.model.set during replay are
-- also gated. Same reasoning: they would mutate state that's already
-- being rebuilt from the recorded log.
do
  fresh_loop()
  _test.fire_bus("sessions.replay.start", { session_id = "resumed", count = 0 })
  send_to_loop("nefor-tui", { kind = "chat.reset" })
  send_to_loop("nefor-tui", { kind = "chat.interrupt_all" })
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "ollama", model = "should-not-stick" })
  _test.fire_bus("sessions.replay.end", { session_id = "resumed" })

  -- model.set was gated, so config.model must not have changed to the
  -- replay-injected value.
  local g = agentic_loop.build_template("hi")
  for _, n in ipairs(g.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      assert(n.args.model ~= "should-not-stick",
        "chat.model.set during replay must not mutate config.model; saw "
        .. tostring(n.args.model))
    end
  end
end

-- ------------------------------------------------------------------
-- Bug 4 regression — sub-graph completion emits the literal terminal
-- output as a chat.message.append so the user can see what the
-- sub-graph produced. The deferred relay text (verbose, model-facing)
-- still rides as the next orchestrator-turn user message; the visible
-- emit lands in the transcript before that.
-- ------------------------------------------------------------------
do
  fresh_loop()
  -- Drive the sub-graph dispatch path the way tool-gate's wrapper
  -- does: queue_sub_graph + tool.result keyed by the minted run_id.
  local terminal_text = "octopuses are eight-armed cephalopods; lighthouses are coastal sentinels."
  local run_id = agentic_loop.queue_sub_graph(
    { graph = { nodes = { { id = "terminal", reasoner = "terminal", args = {} } }, edges = {} } },
    "gate-inner-1"
  )
  assert(type(run_id) == "string", "queue_sub_graph must return a run_id")
  _test.calls_clear()

  -- Drive the canonical sub-graph completion: a tool.result with id
  -- == run_id and result.status == success carrying the terminal node
  -- output table.
  send_to_loop("reasoner-graph", {
    kind   = "tool.result",
    id     = run_id,
    result = {
      status  = "success",
      results = { terminal = { output = { text = terminal_text } } },
    },
  })

  local calls = decode_calls()
  local visible = find_call(calls, "chat.message.append", "system", "octopuses are eight-armed")
  assert(visible ~= nil,
    "sub-graph completion must emit a chat.message.append carrying the literal terminal output; got "
    .. json.encode(_test.calls()))
  assert_eq(visible.target, "nefor-tui",
    "visible sub-graph output must target nefor-tui")
end

-- Sub-graph failure surfaces an [spawn_graph errored] visible message.
do
  fresh_loop()
  local run_id = agentic_loop.queue_sub_graph(
    { graph = { nodes = { { id = "terminal", reasoner = "terminal", args = {} } }, edges = {} } },
    "gate-inner-2"
  )
  _test.calls_clear()
  send_to_loop("reasoner-graph", {
    kind   = "tool.result",
    id     = run_id,
    result = { status = "error", results = { terminal = { error = "boom" } } },
  })
  local calls = decode_calls()
  local err_msg = find_call(calls, "chat.message.append", "system", "spawn_graph errored")
  assert(err_msg ~= nil,
    "sub-graph failure must emit a [spawn_graph errored] system message; got "
    .. json.encode(_test.calls()))
end

print("agentic_workflow_test: ok")
