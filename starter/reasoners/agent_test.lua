-- starter/reasoners/agent_test.lua — unit tests for the `agent`
-- reasoner.
--
-- The Rust harness (`crates/nefor/tests/starter_agent_reasoner_test.rs`)
-- installs a stub `nefor.*` surface (json + engine.* + log.* +
-- bus.on_event) so `require("reasoners")` succeeds, then loads this
-- file. Tests drive the reasoner by:
--
--   * dispatching a `tool.invoke{name="agent"}` envelope through
--     reasoners' receive_msg (matches the live wire path)
--   * synthesising the provider's `chat.complete.result` reply and
--     feeding it through receive_msg (the openai-provider wrapper
--     ordinarily emits this on the bus; for a unit test we drive it
--     directly)
--   * synthesising tool-gate's `tool.result` and feeding it through
--     receive_msg (same — tool-gate ordinarily emits these)
--
-- Assertions read `_test.calls()` (engine.send capture) and the
-- agent reasoner's internal state via `_internals`.

local agentic_loop = require("agentic-loop")
local reasoners    = require("reasoners")
local agent        = require("reasoners.agent")
local json         = nefor.json

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ",
      tostring(expected), tostring(actual)), 2)
  end
end

local function decode_calls()
  local out = {}
  for _, c in ipairs(_test.calls()) do
    local ok, decoded = pcall(json.decode, c.payload)
    if ok and type(decoded) == "table" and type(decoded.body) == "table" then
      out[#out + 1] = { body = decoded.body, target = c.target, from = decoded.from }
    end
  end
  return out
end

local function find_call(calls, predicate)
  for _, c in ipairs(calls) do
    if predicate(c) then return c end
  end
  return nil
end

local function find_calls(calls, predicate)
  local out = {}
  for _, c in ipairs(calls) do
    if predicate(c) then out[#out + 1] = c end
  end
  return out
end

local function make_entry(origin, body)
  return {
    ts      = "2026-05-08T00:00:00.000Z",
    origin  = origin,
    payload = json.encode({ type = "event", from = origin, body = body }),
  }
end

local function feed(origin, body)
  reasoners.receive_msg(make_entry(origin, body))
end

-- Build the wire shape reasoner-graph dispatches: a tool.invoke whose
-- args carry { run_id, node_id, args, inputs, prev_state }.
local function dispatch_agent(firing_id, args)
  feed("reasoner-graph", {
    kind = "tool.invoke",
    id   = firing_id,
    name = "agent",
    args = {
      run_id     = "run-test",
      node_id    = "node-test",
      args       = args,
      inputs     = {},
      prev_state = nil,
    },
  })
end

local function fresh()
  agentic_loop._internals.reset()
  agent._internals.reset()
  reasoners._internals.reset()
  agentic_loop.configure { provider = "mock-prov", model = "test-model" }
  _test.set_plugins({ "mock-prov", "tool-gate", "reasoner-graph" })
  _test.calls_clear()
end

-- ------------------------------------------------------------------
-- Scenario 1: single-turn agent (no tool calls)
-- ------------------------------------------------------------------
do
  fresh()
  dispatch_agent("firing-1", {
    system_prompt  = "You are a builder.",
    model          = "test-model",
    tool_allowlist = { "read", "write" },
    prompt         = "What is 2+2?",
  })

  local calls = decode_calls()
  -- Expect: chat.create + chat.append(system) + chat.append(user) + chat.complete
  local create = find_call(calls, function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  assert(create ~= nil, "agent must emit <provider>.chat.create on dispatch; got " .. json.encode(_test.calls()))
  assert_eq(create.body.model, "test-model", "chat.create carries args.model")
  assert_eq(create.target, "mock-prov", "chat.create targets the provider")

  local appends = find_calls(calls, function(c)
    return c.body.kind == "mock-prov.chat.append"
  end)
  assert_eq(#appends, 2, "agent must emit chat.append for system + user")
  assert_eq(appends[1].body.message.role, "system", "first append is system")
  assert_eq(appends[1].body.message.content, "You are a builder.",
    "system content matches args.system_prompt")
  assert_eq(appends[2].body.message.role, "user",   "second append is user")
  assert_eq(appends[2].body.message.content, "What is 2+2?",
    "user content matches args.prompt")

  local complete = find_call(calls, function(c)
    return c.body.kind == "mock-prov.chat.complete"
  end)
  assert(complete ~= nil, "agent must emit chat.complete to kick off first turn")

  -- Feed the provider's reply: text-only, no tool calls.
  _test.calls_clear()
  local chat_id = create.body.chat_id
  assert(type(chat_id) == "string", "chat_id minted on chat.create")
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = {
      text          = "The answer is 4.",
      finish_reason = "stop",
    },
  })

  -- Assert: terminal tool.result emitted.
  local terminal_calls = decode_calls()
  local terminal = find_call(terminal_calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-1"
  end)
  assert(terminal ~= nil,
    "agent must emit terminal tool.result on text-only provider reply; got "
    .. json.encode(_test.calls()))
  assert(terminal.body.error == nil, "terminal tool.result must NOT carry error")
  assert_eq(terminal.body.result.text, "The answer is 4.",
    "terminal result.text matches provider output.text")
  assert(type(terminal.body.result.next_state) == "table",
    "terminal result.next_state present")
  assert_eq(terminal.body.result.next_state.chat_id, chat_id,
    "terminal next_state.chat_id matches the firing's chat_id")

  -- Sanity: per-firing state cleared after terminal.
  assert(agent._internals.agents["firing-1"] == nil,
    "agent state must clear on terminal")
end

-- ------------------------------------------------------------------
-- Scenario 2: tool-call → response cycle
-- ------------------------------------------------------------------
do
  fresh()
  dispatch_agent("firing-2", {
    system_prompt  = "You are a builder.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Read README.md",
  })

  local create = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  local chat_id = create.body.chat_id

  -- Provider replies with a single tool call.
  _test.calls_clear()
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = {
      text          = "",
      finish_reason = "tool_calls",
      tool_calls    = {
        { id = "model-call-1", name = "read_file", arguments = { path = "README.md" } },
      },
    },
  })

  -- Assert: agent dispatched a tool-gate.tool.invoke for read_file.
  local invoke = find_call(decode_calls(), function(c)
    return c.body.kind == "tool-gate.tool.invoke" and c.body.name == "read_file"
  end)
  assert(invoke ~= nil,
    "agent must dispatch tool-gate.tool.invoke for an allowed tool_call; got "
    .. json.encode(_test.calls()))
  assert(type(invoke.body.id) == "string", "tool-gate.tool.invoke carries an id")
  assert_eq(invoke.body.args.path, "README.md", "args carry through")
  assert_eq(invoke.target, "tool-gate", "tool-gate.tool.invoke targets tool-gate")

  -- Tool-gate emits tool.result when the tool finishes.
  _test.calls_clear()
  feed("tool-gate", {
    kind   = "tool.result",
    id     = invoke.body.id,
    output = "# Hello\nThis is a README.",
  })

  -- Assert: agent appended the tool result to chat history AND fired
  -- the next chat.complete.
  local calls_after_tool = decode_calls()
  local tool_append = find_call(calls_after_tool, function(c)
    return c.body.kind == "mock-prov.chat.append"
       and type(c.body.message) == "table"
       and c.body.message.role == "tool"
  end)
  assert(tool_append ~= nil,
    "agent must append the tool result back to chat history; got "
    .. json.encode(_test.calls()))
  assert_eq(tool_append.body.message.content, "# Hello\nThis is a README.",
    "tool message content matches the tool.result.output")
  assert_eq(tool_append.body.message.tool_call_id, "model-call-1",
    "tool_call_id echoes the model-side call.id")

  local complete2 = find_call(calls_after_tool, function(c)
    return c.body.kind == "mock-prov.chat.complete"
  end)
  assert(complete2 ~= nil,
    "agent must re-fire chat.complete after the tool result lands")

  -- Provider's next reply: terminal text.
  _test.calls_clear()
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = {
      text          = "The README starts with 'Hello'.",
      finish_reason = "stop",
    },
  })

  local terminal = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-2"
  end)
  assert(terminal ~= nil,
    "agent must emit terminal tool.result on second turn's text-only reply")
  assert_eq(terminal.body.result.text, "The README starts with 'Hello'.",
    "terminal text matches second-turn provider output")
  assert_eq(terminal.body.result.next_state.chat_id, chat_id,
    "terminal next_state.chat_id stable across turns")
end

-- ------------------------------------------------------------------
-- Scenario 3: allowlist enforcement
-- ------------------------------------------------------------------
do
  fresh()
  dispatch_agent("firing-3", {
    system_prompt  = "You are an explorer.",
    model          = "test-model",
    tool_allowlist = { "read_file" },   -- bash NOT allowed
    prompt         = "Investigate.",
  })

  local create = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  local chat_id = create.body.chat_id

  -- Provider tries to call a disallowed tool (bash).
  _test.calls_clear()
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = {
      text          = "",
      finish_reason = "tool_calls",
      tool_calls    = {
        { id = "model-call-2", name = "bash", arguments = { command = "rm -rf /" } },
      },
    },
  })

  -- Assert: NO tool-gate.tool.invoke for the disallowed tool.
  local calls_after = decode_calls()
  local leaked = find_call(calls_after, function(c)
    return c.body.kind == "tool-gate.tool.invoke" and c.body.name == "bash"
  end)
  assert(leaked == nil,
    "disallowed tool MUST NOT reach tool-gate; saw " .. json.encode(_test.calls()))

  -- Assert: synthetic tool.result content lands in chat history with
  -- the allowlist-rejection error.
  local tool_append = find_call(calls_after, function(c)
    return c.body.kind == "mock-prov.chat.append"
       and type(c.body.message) == "table"
       and c.body.message.role == "tool"
  end)
  assert(tool_append ~= nil,
    "agent must synthesize a local tool result for disallowed calls; got "
    .. json.encode(_test.calls()))
  assert(string.find(tool_append.body.message.content, "not in allowlist", 1, true),
    "synthesized tool message must mention the allowlist; got: " ..
    tostring(tool_append.body.message.content))
  assert(string.find(tool_append.body.message.content, "bash", 1, true),
    "synthesized tool message must name the rejected tool")
  assert_eq(tool_append.body.message.tool_call_id, "model-call-2",
    "synthesized tool message echoes the model-side call.id")

  -- Assert: a fresh chat.complete fires (next turn — model sees the
  -- rejection in history and adapts).
  local complete = find_call(calls_after, function(c)
    return c.body.kind == "mock-prov.chat.complete"
  end)
  assert(complete ~= nil,
    "agent must re-fire chat.complete after synthesizing the rejection")
end

-- ------------------------------------------------------------------
-- Scenario 4: chat_id stable across multiple turns
-- ------------------------------------------------------------------
do
  fresh()
  dispatch_agent("firing-4", {
    system_prompt  = "You are a builder.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Read x.txt then y.txt.",
  })

  local create = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  local chat_id = create.body.chat_id
  assert(type(chat_id) == "string" and #chat_id > 0, "chat_id minted")

  -- Turn 1 reply: tool call.
  _test.calls_clear()
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = {
      text          = "",
      finish_reason = "tool_calls",
      tool_calls    = {
        { id = "tc-1", name = "read_file", arguments = { path = "x.txt" } },
      },
    },
  })
  local invoke1 = find_call(decode_calls(), function(c)
    return c.body.kind == "tool-gate.tool.invoke" and c.body.name == "read_file"
  end)

  -- Verify: every chat.append + chat.complete observed so far has
  -- ridden the same chat_id.
  for _, c in ipairs(decode_calls()) do
    if c.body.kind == "mock-prov.chat.append"
        or c.body.kind == "mock-prov.chat.complete" then
      assert_eq(c.body.chat_id, chat_id,
        "every provider envelope must ride the firing's chat_id; saw "
        .. tostring(c.body.chat_id))
    end
  end

  -- Tool result back.
  _test.calls_clear()
  feed("tool-gate", {
    kind   = "tool.result",
    id     = invoke1.body.id,
    output = "x contents",
  })
  -- Verify chat_id stable on the second turn's chat.append + chat.complete.
  for _, c in ipairs(decode_calls()) do
    if c.body.kind == "mock-prov.chat.append"
        or c.body.kind == "mock-prov.chat.complete" then
      assert_eq(c.body.chat_id, chat_id,
        "post-tool-result chat envelopes must reuse the same chat_id; saw "
        .. tostring(c.body.chat_id))
    end
  end

  -- Turn 2 reply: another tool call.
  _test.calls_clear()
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = {
      text          = "",
      finish_reason = "tool_calls",
      tool_calls    = {
        { id = "tc-2", name = "read_file", arguments = { path = "y.txt" } },
      },
    },
  })
  -- Verify chat_id still stable on the third turn.
  for _, c in ipairs(decode_calls()) do
    if c.body.kind == "mock-prov.chat.append"
        or c.body.kind == "mock-prov.chat.complete"
        or c.body.kind == "tool-gate.tool.invoke" then
      if c.body.chat_id ~= nil then
        assert_eq(c.body.chat_id, chat_id,
          "third-turn chat envelopes must reuse the same chat_id")
      end
    end
  end

  -- Final terminal reply.
  _test.calls_clear()
  local invoke2 = find_call(decode_calls(), function(_) return false end)  -- (drained)
  -- Capture the second tool_id from earlier reply chain.
  -- (We re-dispatch tool result then a terminal reply.)
  -- Simulated: bypass through next firing chain by consuming pending tool.
  feed("tool-gate", {
    kind   = "tool.result",
    id     = (function()
      -- look up the most recent tool-id minted: pull from agent state
      for tid, fid in pairs(agent._internals.tool_to_firing) do
        if fid == "firing-4" then return tid end
      end
      error("expected an outstanding tool_id for firing-4")
    end)(),
    output = "y contents",
  })
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = {
      text          = "Both files read.",
      finish_reason = "stop",
    },
  })
  local terminal = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-4"
  end)
  assert(terminal ~= nil, "terminal tool.result fires after last reply")
  assert_eq(terminal.body.result.next_state.chat_id, chat_id,
    "terminal next_state.chat_id matches the originally-minted chat_id (stable across re-fires)")
end

-- ------------------------------------------------------------------
-- Scenario 5: provider error surfaces as terminal error
-- ------------------------------------------------------------------
do
  fresh()
  dispatch_agent("firing-err", {
    system_prompt  = "You are a builder.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Read.",
  })
  local create = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  local chat_id = create.body.chat_id

  _test.calls_clear()
  feed("mock-prov", {
    kind    = "mock-prov.chat.error",
    chat_id = chat_id,
    message = "rate limited",
  })
  local terminal = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-err"
  end)
  assert(terminal ~= nil, "agent must emit terminal tool.result on chat.error")
  assert(terminal.body.error ~= nil, "terminal tool.result must carry an error")
  assert(string.find(tostring(terminal.body.error), "rate limited", 1, true),
    "error string includes the provider message")
end

print("agent_test: ok")
