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

print("agentic_workflow_test: ok")
