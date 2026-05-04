-- starter/agentic_workflow_test.lua — unit tests for the for_chat
-- adapter's runtime model-switch handling.
--
-- Loaded by `crates/nefor/tests/starter_agentic_workflow_test.rs`. The
-- Rust harness installs a stub `nefor` surface (json + engine.* +
-- log.*) so `require("agentic_workflow")` succeeds; this file then
-- drives `for_chat().from_plugin` directly without spawning anything
-- on the bus.

local agentic_workflow = require("agentic_workflow")

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ",
      tostring(expected), tostring(actual)), 2)
  end
end

-- Configure the orchestrator with a known model so we can detect a
-- runtime switch.
agentic_workflow.setup {
  provider = "ollama",
  model    = "initial-model",
}

local for_chat = agentic_workflow.for_chat()
assert(type(for_chat) == "table", "for_chat returns table")
assert(type(for_chat.from_plugin) == "function", "for_chat.from_plugin is a function")

-- Sanity: build_template reflects the seeded model.
do
  local g = agentic_workflow.build_template("hi")
  -- The orchestrator graph is opaque, but we can poke at the wrap node's
  -- args (where the orchestrator stamps the model). Walk the nodes list
  -- looking for the wrap node and confirm.
  local saw = false
  for _, n in ipairs(g.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      saw = true
      assert_eq(n.args.model, "initial-model", "wrap node carries seeded model")
    end
  end
  assert(saw, "wrap node found in template")
end

-- chat.model.set with a non-empty model updates config.model. The
-- envelope is passed through (return env) so the egress transform on
-- the provider can still translate it into <prefix>.model.set.
do
  local env = {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.model.set", provider = "ollama", model = "new-model" },
  }
  local out = for_chat.from_plugin(env)
  assert(out == env, "chat.model.set is passed through unchanged")

  -- Verify config.model was updated by re-building the template.
  local g = agentic_workflow.build_template("hi")
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
  local env = {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.model.set", provider = "ollama", model = "" },
  }
  local out = for_chat.from_plugin(env)
  assert(out == env, "empty-model chat.model.set still passes through")
  -- config.model should still be "new-model" from the previous case.
  local g = agentic_workflow.build_template("hi")
  for _, n in ipairs(g.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      assert_eq(n.args.model, "new-model", "empty-model set did not clobber config.model")
    end
  end
end

-- chat.model.set with the model field absent is also a no-op.
do
  local env = {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.model.set", provider = "ollama" },
  }
  local out = for_chat.from_plugin(env)
  assert(out == env, "model-absent chat.model.set still passes through")
  local g = agentic_workflow.build_template("hi")
  for _, n in ipairs(g.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      assert_eq(n.args.model, "new-model", "missing-model set did not clobber config.model")
    end
  end
end

-- A second switch updates config.model again.
do
  local env = {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.model.set", provider = "ollama", model = "another-model" },
  }
  for_chat.from_plugin(env)
  local g = agentic_workflow.build_template("hi")
  for _, n in ipairs(g.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      assert_eq(n.args.model, "another-model", "second switch sticks")
    end
  end
end

-- ------------------------------------------------------------------
-- session lifecycle handlers (graph_skips_replay, provider_rebuilds_*,
-- session_end clears state)
-- ------------------------------------------------------------------
--
-- The bus subscriptions registered in agentic_workflow.setup() are
-- driven via `_test.fire_bus(kind, body)`. After firing the handlers
-- we can observe (a) the recorded engine.send calls (via `_test.calls()`),
-- (b) for_reasoner_graph's behaviour while replay_mode is set.

-- Session_end → emits chat.reset broadcast and graph.cancel for any
-- in-flight runs. We seed plugin list so the broadcast fans out.
do
  _test.set_plugins({ "ollama", "reasoner-graph", "nefor-tui" })
  _test.calls_clear()

  _test.fire_bus("sessions.session_end", { session_id = "old-id" })

  local saw_reset = false
  for _, c in ipairs(_test.calls()) do
    local ok, decoded = pcall(nefor.json.decode, c.payload)
    if ok and type(decoded) == "table" and type(decoded.body) == "table"
       and decoded.body.kind == "chat.reset" then
      saw_reset = true
    end
  end
  assert(saw_reset, "session_end must broadcast chat.reset to clear provider+TUI state")
end

-- graph_skips_replay: between session_end and resume_done, the
-- for_reasoner_graph from_plugin transform drops every envelope so the
-- graph plugin's replayed run_node emissions can't re-trigger handlers.
-- Firing session_start (preceded by session_end) enters replay_mode;
-- resume_done lifts it.
do
  local rg = agentic_workflow.for_reasoner_graph()
  -- Sanity: a normal envelope passes through pre-replay (replay_mode is
  -- still false from the boot session_start that didn't follow an end).
  local pre_env = {
    type = "event",
    body = { kind = "irrelevant.kind" },
    from = "reasoner-graph",
  }
  local pre_out = rg.from_plugin(pre_env)
  assert(pre_out ~= nil, "pre-replay: pass-through")

  -- Enter replay (session_end already fired above; now session_start).
  _test.fire_bus("sessions.session_start", { session_id = "new-id" })

  local replay_env = {
    type = "event",
    body = { kind = "responder.run_node", run_id = "r1", node_id = "n1", firing_id = "f1" },
    from = "reasoner-graph",
  }
  local replay_out = rg.from_plugin(replay_env)
  assert_eq(replay_out, nil,
    "graph_skips_replay: in replay_mode, reasoner-graph from_plugin drops envelopes")

  -- Lift replay_mode.
  _test.fire_bus("sessions.resume_done", { session_id = "new-id" })

  local post_env = {
    type = "event",
    body = { kind = "irrelevant.kind" },
    from = "reasoner-graph",
  }
  local post_out = rg.from_plugin(post_env)
  assert(post_out ~= nil, "after resume_done: pass-through restored")
end

-- provider_rebuilds_chat_history: replay paints provider chat.* events
-- (e.g. ollama.chat.create) onto the wire — they reach the openai-provider
-- process directly and are processed by its native state machine. The
-- agentic_workflow per-provider transform's job is to NOT eat them
-- during replay (the inner adapter only intercepts chat.complete.result
-- envelopes for chats it owns). With no such ownership, replayed
-- chat.create/append entries pass through to the outer adapter.
do
  local provider_chain = agentic_workflow.for_provider("ollama")
  local create_env = {
    type = "event",
    body = { kind = "ollama.chat.create", chat_id = "old-chat-1" },
    from = "ollama",
  }
  local out = provider_chain.from_plugin(create_env)
  -- The outer adapter doesn't rewrite chat.create — it stays as-is and
  -- passes through to broadcast. The inner adapter only intercepts
  -- chat.complete.result for chats it owns; old-chat-1 isn't in
  -- chat_id_to_key (cleared by session_end), so the replay flows through.
  assert(out ~= nil,
    "provider_rebuilds_chat_history: replayed chat.create passes through")
  assert_eq(out.body.kind, "ollama.chat.create",
    "provider replay preserves the provider-prefixed kind")
end

-- ------------------------------------------------------------------
-- Issue 1 + Issue 3 — user message echo + busy-submit queue
-- ------------------------------------------------------------------
--
-- Two intertwined behaviours share the chat.input.submit handler:
--   * Issue 1: chat.input.submit must emit `chat.message.append`
--     `{ role=user, text }` so the user message is persisted (and
--     thus replayable) as a step-origin entry. Without this the
--     resume replay path can't repaint user turns.
--   * Issue 3: when busy, queue subsequent submits; flush on
--     graph.run_complete; clear queue on chat.reset.

-- Helpers ---------------------------------------------------------
local function decode_calls()
  local out = {}
  for _, c in ipairs(_test.calls()) do
    local ok, decoded = pcall(nefor.json.decode, c.payload)
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

local function fresh_for_chat()
  agentic_workflow._reset()
  agentic_workflow.setup { provider = "ollama", model = "test-model" }
  _test.set_plugins({ "ollama", "reasoner-graph", "nefor-tui" })
  _test.calls_clear()
  return agentic_workflow.for_chat()
end

-- (1.A) chat.input.submit emits chat.message.append role=user. -----
do
  local for_chat = fresh_for_chat()
  for_chat.from_plugin {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.input.submit", text = "first prompt" },
  }
  local calls = decode_calls()
  local user_echo = find_call(calls, "chat.message.append", "user", "first prompt")
  assert(user_echo ~= nil,
    "chat.input.submit must emit chat.message.append role=user (Issue 1 echo)")
  assert_eq(user_echo.target, "nefor-tui",
    "user echo must target nefor-tui specifically")
end

-- (3.A) Busy submit is queued, not rejected. --------------------------
do
  local for_chat = fresh_for_chat()
  -- First submit — orchestrator becomes busy.
  for_chat.from_plugin {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.input.submit", text = "first" },
  }
  _test.calls_clear()
  -- Second submit while busy. With the old code this emitted
  -- `[orchestrator busy — wait …]` and dropped the input. With the
  -- new code the input should queue, AND the user message should
  -- still echo (so the user sees they were heard).
  for_chat.from_plugin {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.input.submit", text = "second" },
  }
  local calls = decode_calls()
  local echo = find_call(calls, "chat.message.append", "user", "second")
  assert(echo ~= nil,
    "queued submit must still echo a chat.message.append role=user")
  local queued_note = find_call(calls, "chat.message.append", "system", "queued")
  assert(queued_note ~= nil,
    "queued submit must surface a [queued ...] system message; got " ..
    nefor.json.encode(calls))
  -- Old-text rejection MUST be gone.
  local busy_msg = find_call(calls, "chat.message.append", "system", "orchestrator busy")
  assert_eq(busy_msg, nil,
    "the rejected '[orchestrator busy ...]' must no longer appear")
end

-- (3.B) Two messages submitted back-to-back BOTH dispatch as the
-- orchestrator frees up. We drive graph.run_complete to flush.
do
  local for_chat = fresh_for_chat()
  local rg = agentic_workflow.for_reasoner_graph()
  -- Submit twice in a row.
  for_chat.from_plugin {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.input.submit", text = "alpha" },
  }
  for_chat.from_plugin {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.input.submit", text = "beta" },
  }
  -- Capture run_id by walking emitted reasoner-graph.run envelopes.
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
  -- Sanity: only ONE run dispatched so far (second is queued).
  do
    local count = 0
    for _, c in ipairs(decode_calls()) do
      if c.body.kind == "reasoner-graph.run" then count = count + 1 end
    end
    assert_eq(count, 1, "second submit must NOT dispatch while first is in flight")
  end
  _test.calls_clear()
  -- Drive graph.run_complete success for the first run.
  rg.from_plugin {
    type = "event",
    from = "reasoner-graph",
    body = {
      kind   = "graph.run_complete",
      run_id = first_run,
      status = "success",
      results = {},
    },
  }
  -- Now the queued "beta" should have dispatched.
  local second_run = find_run_emit(decode_calls())
  assert(second_run ~= nil,
    "queued submit must dispatch on graph.run_complete; sends were " ..
    nefor.json.encode(_test.calls()))
  assert(second_run ~= first_run,
    "second run must be a fresh run_id, got " .. tostring(second_run))
end

-- (3.C) chat.reset clears the pending-input queue (no surprise dispatch).
do
  local for_chat = fresh_for_chat()
  for_chat.from_plugin {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.input.submit", text = "first" },
  }
  for_chat.from_plugin {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.input.submit", text = "queued-2" },
  }
  for_chat.from_plugin {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.input.submit", text = "queued-3" },
  }
  -- Reset clears queue + current_run_id.
  for_chat.from_plugin {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.reset" },
  }
  _test.calls_clear()
  -- Now drive graph.run_complete (orchestrator's old run_id was
  -- cleared by chat.reset, so this matches nothing — but we want to
  -- confirm the queue is empty independently of run_complete). Fire
  -- a fresh submit and assert the dispatched run carries "fresh",
  -- NOT "queued-2" (which would mean the queue leaked across reset).
  for_chat.from_plugin {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.input.submit", text = "fresh" },
  }
  -- Inspect the reasoner-graph.run envelope's user_text. The graph
  -- builder embeds the prompt inside the wrap node's args.
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

-- (3.D) session_end teardown clears the queue (resume implies discard).
do
  local for_chat = fresh_for_chat()
  for_chat.from_plugin {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.input.submit", text = "first" },
  }
  for_chat.from_plugin {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.input.submit", text = "stranded" },
  }
  -- Session swap.
  _test.fire_bus("sessions.session_end", { session_id = "old" })
  _test.calls_clear()
  -- Fresh submit after session swap. The queue should be empty —
  -- the previously queued "stranded" must NOT dispatch.
  for_chat.from_plugin {
    type = "event",
    from = "nefor-tui",
    body = { kind = "chat.input.submit", text = "post-swap" },
  }
  -- Walk reasoner-graph.run envelopes; assert none carries "stranded".
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
  agentic_workflow._reset()
  agentic_workflow.setup { provider = "ollama", model = "test-model" }
  _test.set_plugins({ "ollama", "reasoner-graph", "nefor-tui" })
  _test.calls_clear()
  -- Trigger cancel_all; ensure the developer-telemetry summary
  -- (`[interrupted: chat=N sub-graphs=N deferred=N]`) does NOT appear
  -- as a chat.message.append on the bus.
  agentic_workflow.cancel_all()
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
