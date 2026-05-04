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

print("agentic_workflow_test: ok")
