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

-- ------------------------------------------------------------------
-- Scenario 6: finalize tool — answer-only terminates with structured
-- ------------------------------------------------------------------
--
-- The agent reasoner intercepts `finalize` BEFORE allowlist
-- enforcement / tool-gate dispatch. A `finalize` call MUST:
--   * never reach tool-gate (no tool-gate.tool.invoke envelope)
--   * close the firing with result.text = args.answer and
--     result.structured = the args object verbatim
do
  fresh()
  dispatch_agent("firing-fin-1", {
    system_prompt  = "You are an explorer.",
    model          = "test-model",
    tool_allowlist = { "read_file" },   -- finalize NOT listed here
    prompt         = "Investigate.",
  })

  local create = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  local chat_id = create.body.chat_id

  _test.calls_clear()
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = {
      text          = "",
      finish_reason = "tool_calls",
      tool_calls    = {
        { id = "tc-fin-1", name = "finalize", arguments = { answer = "done" } },
      },
    },
  })

  local calls = decode_calls()

  -- finalize must NEVER reach tool-gate.
  local leaked = find_call(calls, function(c)
    return c.body.kind == "tool-gate.tool.invoke" and c.body.name == "finalize"
  end)
  assert(leaked == nil,
    "finalize MUST NOT reach tool-gate; saw " .. json.encode(_test.calls()))

  -- The terminal envelope carries text + structured.
  local terminal = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-fin-1"
  end)
  assert(terminal ~= nil,
    "agent must emit terminal tool.result on finalize; got " .. json.encode(_test.calls()))
  assert(terminal.body.error == nil, "terminal tool.result must NOT carry error")
  assert_eq(terminal.body.result.text, "done", "result.text = args.answer")
  assert(type(terminal.body.result.structured) == "table",
    "result.structured present")
  assert_eq(terminal.body.result.structured.answer, "done",
    "result.structured carries the answer field verbatim")
  assert_eq(terminal.body.result.next_state.chat_id, chat_id,
    "terminal next_state.chat_id matches the firing's chat_id")

  -- Per-firing state cleared.
  assert(agent._internals.agents["firing-fin-1"] == nil,
    "agent state must clear on finalize-terminal")
end

-- ------------------------------------------------------------------
-- Scenario 7: finalize with extra structured fields
-- ------------------------------------------------------------------
--
-- Arbitrary fields on `finalize`'s args land verbatim in
-- result.structured. These are what downstream reasoner combinators
-- read to compose the next agent's prompt (spec §2 hand-off
-- mechanics).
do
  fresh()
  dispatch_agent("firing-fin-2", {
    system_prompt  = "You are an explorer.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Investigate.",
  })
  local create = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  local chat_id = create.body.chat_id

  _test.calls_clear()
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = {
      text          = "",
      finish_reason = "tool_calls",
      tool_calls    = {
        { id = "tc-fin-2", name = "finalize", arguments = {
            answer     = "done",
            findings   = { "x", "y" },
            confidence = 0.8,
          },
        },
      },
    },
  })

  local terminal = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-fin-2"
  end)
  assert(terminal ~= nil, "finalize emits terminal tool.result")
  assert_eq(terminal.body.result.text, "done", "result.text = args.answer")
  local s = terminal.body.result.structured
  assert(type(s) == "table", "result.structured present")
  assert_eq(s.answer, "done", "structured.answer = args.answer")
  assert(type(s.findings) == "table", "structured.findings present (list)")
  assert_eq(#s.findings, 2, "structured.findings preserves length")
  assert_eq(s.findings[1], "x", "structured.findings[1] preserved")
  assert_eq(s.findings[2], "y", "structured.findings[2] preserved")
  assert_eq(s.confidence, 0.8, "structured.confidence preserved verbatim")
end

-- ------------------------------------------------------------------
-- Scenario 8: finalize alongside other tool calls — finalize wins
-- ------------------------------------------------------------------
--
-- When the provider returns multiple tool_calls in a single response
-- and one of them is `finalize`, the agent terminates immediately on
-- the finalize. Sibling calls (here: bash) are dropped — they MUST
-- NOT reach tool-gate. This pins the simpler semantic over running
-- siblings before terminating.
do
  fresh()
  dispatch_agent("firing-fin-3", {
    system_prompt  = "You are a builder.",
    model          = "test-model",
    tool_allowlist = { "bash" },   -- bash is allowed, but finalize wins
    prompt         = "Build.",
  })
  local create = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  local chat_id = create.body.chat_id

  _test.calls_clear()
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = {
      text          = "",
      finish_reason = "tool_calls",
      tool_calls    = {
        { id = "tc-bash", name = "bash", arguments = { command = "ls" } },
        { id = "tc-fin",  name = "finalize", arguments = { answer = "built" } },
      },
    },
  })

  local calls = decode_calls()

  -- The sibling bash call MUST NOT have been dispatched.
  local bash_leak = find_call(calls, function(c)
    return c.body.kind == "tool-gate.tool.invoke" and c.body.name == "bash"
  end)
  assert(bash_leak == nil,
    "sibling tool_calls dropped when finalize is in the same response; saw "
    .. json.encode(_test.calls()))

  -- Terminal lands.
  local terminal = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-fin-3"
  end)
  assert(terminal ~= nil, "finalize-among-siblings emits terminal tool.result")
  assert_eq(terminal.body.result.text, "built", "result.text from finalize")
  assert_eq(terminal.body.result.structured.answer, "built",
    "result.structured.answer from finalize")
end

-- ------------------------------------------------------------------
-- Scenario 9: chat.create advertises finalize regardless of allowlist
-- ------------------------------------------------------------------
--
-- The agent reasoner injects `finalize` into the advertised tool set
-- on every firing — the caller's `tool_allowlist` doesn't need to
-- mention it. This ensures sub-agents can always terminate
-- structurally even with a minimal allowlist.
do
  fresh()
  dispatch_agent("firing-fin-4", {
    system_prompt  = "You are an explorer.",
    model          = "test-model",
    tool_allowlist = { "read_file" },   -- finalize NOT mentioned
    prompt         = "Investigate.",
  })

  local create = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  assert(create ~= nil, "chat.create emitted")
  assert(type(create.body.tools) == "table",
    "chat.create.tools present (list of advertised tool names)")

  local saw_finalize, saw_read = false, false
  for _, n in ipairs(create.body.tools) do
    if n == "finalize"  then saw_finalize = true end
    if n == "read_file" then saw_read     = true end
  end
  assert(saw_read,
    "user-supplied allowlist entries still advertised (read_file)")
  assert(saw_finalize,
    "finalize MUST be auto-included in chat.create.tools regardless of allowlist")
end

-- ------------------------------------------------------------------
-- Scenario 10: finalize with empty/missing answer synthesises text
-- ------------------------------------------------------------------
--
-- A degenerate finalize call (no answer / empty answer) terminates
-- with a placeholder text rather than erroring. structured still
-- carries whatever the model passed.
do
  fresh()
  dispatch_agent("firing-fin-5", {
    system_prompt  = "You are an explorer.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Investigate.",
  })
  local create = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  local chat_id = create.body.chat_id

  _test.calls_clear()
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = {
      text          = "",
      finish_reason = "tool_calls",
      tool_calls    = {
        { id = "tc-fin-empty", name = "finalize", arguments = {} },
      },
    },
  })

  local terminal = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-fin-5"
  end)
  assert(terminal ~= nil, "empty finalize still emits terminal tool.result")
  assert(terminal.body.error == nil,
    "missing answer must NOT surface as a terminal error")
  assert(type(terminal.body.result.text) == "string"
       and #terminal.body.result.text > 0,
    "result.text is a non-empty placeholder when answer is missing; got: "
    .. tostring(terminal.body.result.text))
  assert(string.find(terminal.body.result.text, "finalize", 1, true) ~= nil,
    "placeholder text mentions finalize for diagnostics")
end

-- ------------------------------------------------------------------
-- Scenario 11: graph.cancel propagates to in-flight sub-agent firing
-- ------------------------------------------------------------------
--
-- Mirrors the chat-side cancel_all → <provider>.interrupt fix from
-- ef260cd, applied to sub-graph firings (#53). When lead-workflow
-- broadcasts `graph.cancel { run_id }` (session_end / user /quit
-- mid-run), every agent reasoner with a firing under that run_id MUST:
--   * emit `<provider>.interrupt { chat_id }` so the provider binary
--     tears down the streaming HTTP call,
--   * emit a terminal `tool.result { error }` so reasoner-graph
--     de-registers the firing.
--
-- Verified-fail-pre-fix: with the on_graph_cancel handler removed (or
-- the receive_msg branch elided), neither envelope fires.
do
  fresh()
  dispatch_agent("firing-cancel-1", {
    system_prompt  = "You are an explorer.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Investigate.",
  })

  local create = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  local chat_id = create.body.chat_id

  -- Mid-firing: lead-workflow broadcasts graph.cancel for our run_id.
  -- The dispatch_agent helper sets run_id = "run-test".
  _test.calls_clear()
  feed("lead-workflow", { kind = "graph.cancel", run_id = "run-test" })

  local calls = decode_calls()

  -- Provider interrupt MUST fire, carrying our chat_id.
  local interrupt = find_call(calls, function(c)
    return c.body.kind == "mock-prov.interrupt"
       and c.body.chat_id == chat_id
       and c.target == "mock-prov"
  end)
  assert(interrupt ~= nil,
    "graph.cancel for the firing's run_id must emit <provider>.interrupt { chat_id }; got "
    .. json.encode(_test.calls()))

  -- Terminal tool.result MUST close the firing with an error.
  local terminal = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-cancel-1"
  end)
  assert(terminal ~= nil,
    "graph.cancel must close the firing with a terminal tool.result; got "
    .. json.encode(_test.calls()))
  assert(terminal.body.error ~= nil,
    "terminal tool.result on graph.cancel must carry an error (not a normal result)")
  assert(string.find(tostring(terminal.body.error), "cancelled", 1, true) ~= nil
      or string.find(tostring(terminal.body.error), "Cancel", 1, true) ~= nil,
    "terminal error mentions cancellation; got: " .. tostring(terminal.body.error))

  -- Per-firing state cleared.
  assert(agent._internals.agents["firing-cancel-1"] == nil,
    "agent state cleared after graph.cancel teardown")
end

-- ------------------------------------------------------------------
-- Scenario 12: graph.cancel fans out to every firing under the run
-- ------------------------------------------------------------------
--
-- Two sub-agent firings under the same run_id (e.g. an explore →
-- explore parallel pair). One graph.cancel must interrupt BOTH chat_ids
-- and close BOTH firings. Verifies the fanout walks the agents map and
-- doesn't stop at the first match.
do
  fresh()
  dispatch_agent("firing-multi-A", {
    system_prompt  = "You are an explorer.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Investigate A.",
  })
  dispatch_agent("firing-multi-B", {
    system_prompt  = "You are an explorer.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Investigate B.",
  })

  -- Capture each firing's chat_id from its chat.create.
  local creates = find_calls(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  assert_eq(#creates, 2, "two firings → two chat.create envelopes")
  local chat_ids = {}
  for _, c in ipairs(creates) do chat_ids[#chat_ids + 1] = c.body.chat_id end
  assert(chat_ids[1] ~= chat_ids[2], "each firing mints a distinct chat_id")

  _test.calls_clear()
  feed("lead-workflow", { kind = "graph.cancel", run_id = "run-test" })

  local calls = decode_calls()

  -- Both chat_ids interrupted.
  local interrupts = find_calls(calls, function(c)
    return c.body.kind == "mock-prov.interrupt"
  end)
  assert_eq(#interrupts, 2,
    "graph.cancel must interrupt every firing under the run; got "
    .. json.encode(_test.calls()))
  local hit = { [chat_ids[1]] = false, [chat_ids[2]] = false }
  for _, c in ipairs(interrupts) do
    if hit[c.body.chat_id] ~= nil then hit[c.body.chat_id] = true end
  end
  assert(hit[chat_ids[1]] and hit[chat_ids[2]],
    "every firing's chat_id appears on an interrupt envelope")

  -- Both firings closed.
  for _, fid in ipairs({ "firing-multi-A", "firing-multi-B" }) do
    local terminal = find_call(calls, function(c)
      return c.body.kind == "tool.result" and c.body.id == fid
    end)
    assert(terminal ~= nil,
      "graph.cancel closes firing " .. fid .. "; got " .. json.encode(_test.calls()))
    assert(terminal.body.error ~= nil,
      "firing " .. fid .. " closed with an error on cancel")
    assert(agent._internals.agents[fid] == nil,
      "firing " .. fid .. " state cleared after cancel")
  end
end

-- ------------------------------------------------------------------
-- Scenario 13: graph.cancel for an unrelated run_id is a no-op
-- ------------------------------------------------------------------
--
-- A firing under run_id A must NOT be interrupted when graph.cancel
-- carries run_id B. Guards against accidental cancel-fanout to siblings
-- (e.g. lead's own chat firing under a different run, or a parallel
-- graph the user didn't /quit).
do
  fresh()
  dispatch_agent("firing-unrelated", {
    system_prompt  = "You are an explorer.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Investigate.",
  })

  -- The firing is under run_id "run-test" (dispatch_agent default).
  -- Cancel a DIFFERENT run.
  _test.calls_clear()
  feed("lead-workflow", { kind = "graph.cancel", run_id = "run-some-other" })

  local calls = decode_calls()

  -- No interrupt envelope.
  local interrupt = find_call(calls, function(c)
    return c.body.kind == "mock-prov.interrupt"
  end)
  assert(interrupt == nil,
    "graph.cancel for a non-matching run_id must NOT emit interrupt; got "
    .. json.encode(_test.calls()))

  -- No terminal tool.result for our firing.
  local terminal = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-unrelated"
  end)
  assert(terminal == nil,
    "graph.cancel for an unrelated run must NOT close our firing")

  -- State still alive.
  assert(agent._internals.agents["firing-unrelated"] ~= nil,
    "agent state for the unrelated firing must still be live")
end

-- ------------------------------------------------------------------
-- Scenario 14: graph.cancel is idempotent against an already-closed firing
-- ------------------------------------------------------------------
--
-- Race: the firing terminates normally (text-only reply lands) and
-- THEN graph.cancel arrives for the same run_id. The cancel handler
-- must no-op (no double interrupt, no double terminal tool.result) —
-- otherwise we'd emit a stale interrupt against a chat_id whose stream
-- is already gone, and reasoner-graph would see a phantom tool.result
-- it can't correlate.
do
  fresh()
  dispatch_agent("firing-closed", {
    system_prompt  = "You are an explorer.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Investigate.",
  })

  local create = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  local chat_id = create.body.chat_id

  -- Close normally with a text-only reply.
  _test.calls_clear()
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = { text = "Done.", finish_reason = "stop" },
  })
  local terminal = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-closed"
  end)
  assert(terminal ~= nil, "firing closed normally")
  assert(agent._internals.agents["firing-closed"] == nil,
    "state cleared after normal close")

  -- Now graph.cancel arrives for the same run_id. Should be a no-op.
  _test.calls_clear()
  feed("lead-workflow", { kind = "graph.cancel", run_id = "run-test" })

  local calls = decode_calls()

  -- No spurious interrupt.
  local late_interrupt = find_call(calls, function(c)
    return c.body.kind == "mock-prov.interrupt"
  end)
  assert(late_interrupt == nil,
    "post-close graph.cancel must NOT emit a late interrupt; got "
    .. json.encode(_test.calls()))

  -- No spurious terminal tool.result.
  local late_terminal = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-closed"
  end)
  assert(late_terminal == nil,
    "post-close graph.cancel must NOT emit a second terminal tool.result")
end

-- ------------------------------------------------------------------
-- Scenario 15 (Fix #1): chat.complete carries finalize schema in
-- extra_tools so the provider binary appends it to the upstream
-- tools array
-- ------------------------------------------------------------------
--
-- Pre-fix the agent reasoner emitted `<provider>.chat.complete {
-- chat_id }` only — the binary built its tools array purely from the
-- global ToolCatalog and the model never saw `finalize` as an option.
-- Fix #1 adds `extra_tools = [FINALIZE_SCHEMA]` to the chat.complete
-- envelope; the binary appends them to the catalog tools before
-- assembling the upstream HTTP request.
--
-- Verified-fail-pre-fix: removing the `extra_tools = { FINALIZE_SCHEMA }`
-- line in agent.lua's emit_chat_complete trips the schema-shape
-- assertion below.
do
  fresh()
  dispatch_agent("firing-extra-1", {
    system_prompt  = "You are a builder.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Build.",
  })

  local complete = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.complete"
  end)
  assert(complete ~= nil, "chat.complete emitted on dispatch")
  assert(type(complete.body.extra_tools) == "table",
    "chat.complete must carry extra_tools as a list")
  assert_eq(#complete.body.extra_tools, 1,
    "extra_tools holds exactly one entry (finalize)")

  local entry = complete.body.extra_tools[1]
  assert(type(entry) == "table",
    "extra_tools[1] is a tool-spec object")
  assert_eq(entry.type, "function",
    "extra_tools[1].type is 'function' (OpenAI tool wire shape)")
  assert(type(entry["function"]) == "table",
    "extra_tools[1].function is a table")
  assert_eq(entry["function"].name, "finalize",
    "extra_tools[1].function.name is 'finalize'")

  -- Also assert subsequent turns (tool-call → result → next chat.complete)
  -- still carry extra_tools so the model can call finalize on iteration 2.
  local create = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.create"
  end)
  local chat_id = create.body.chat_id

  _test.calls_clear()
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = {
      text          = "",
      finish_reason = "tool_calls",
      tool_calls    = {
        { id = "tc-extra", name = "read_file", arguments = { path = "x.txt" } },
      },
    },
  })
  local invoke = find_call(decode_calls(), function(c)
    return c.body.kind == "tool-gate.tool.invoke"
  end)
  _test.calls_clear()
  feed("tool-gate", {
    kind   = "tool.result",
    id     = invoke.body.id,
    output = "x contents",
  })

  local complete2 = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.complete"
  end)
  assert(complete2 ~= nil, "chat.complete fires on next turn")
  assert(type(complete2.body.extra_tools) == "table",
    "next-turn chat.complete also carries extra_tools (finalize available every iteration)")
  assert_eq(complete2.body.extra_tools[1]["function"].name, "finalize",
    "next-turn extra_tools[1] is finalize")
end

-- ------------------------------------------------------------------
-- Scenario 16 (Fix #2): chat.message.append { role=system } folds
-- into <provider>.chat.append for active firings
-- ------------------------------------------------------------------
--
-- tool-gate's smart AGENTS.md loader (b600850) emits
-- chat.message.append { role=system, text=<marker>+<body> } envelopes
-- onto the bus when an inner tool call touches a path. Pre-fix the
-- envelope was TUI/persistence-only; the model never saw the AGENTS.md
-- content because nothing folded it into provider chat history.
-- Fix #2 has the agent reasoner watch its bus subscription for
-- chat.message.append envelopes with role=system and translate them
-- into <provider>.chat.append for each active firing.
--
-- Verified-fail-pre-fix: removing the chat.message.append branch in
-- agent.lua's receive_msg trips the "agent must fold system message
-- into <provider>.chat.append" assertion.
do
  fresh()
  dispatch_agent("firing-fold-1", {
    system_prompt  = "You are a builder.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Read README.",
  })

  -- Drain dispatch envelopes; we only care about envelopes after the
  -- system-message fold is triggered.
  _test.calls_clear()

  -- tool-gate's smart loader emits this envelope onto the bus. The
  -- text shape mirrors the actual marker tool-gate uses (the prefix
  -- is load-bearing per spec §5).
  local agents_md_text =
    "[Loaded /home/skril/code/foo/AGENTS.md because tool call touched a file in /home/skril/code/foo/. " ..
    "This is project guidance for that directory, not a user request.]\n\n" ..
    "Project rule: prefer functional style. Avoid global state."
  feed("tool-gate", {
    kind = "chat.message.append",
    role = "system",
    text = agents_md_text,
  })

  local calls = decode_calls()
  local fold = find_call(calls, function(c)
    return c.body.kind == "mock-prov.chat.append"
       and type(c.body.message) == "table"
       and c.body.message.role == "system"
       and c.body.message.content == agents_md_text
  end)
  assert(fold ~= nil,
    "agent must fold system chat.message.append into <provider>.chat.append for the active firing; got "
    .. json.encode(_test.calls()))
  assert_eq(fold.target, "mock-prov",
    "folded chat.append targets the firing's provider")

  -- The chat_id MUST match the firing's chat_id (so the provider
  -- binary appends to the same chat history the agent reasoner is
  -- driving).
  local entry
  for _, e in pairs(agent._internals.agents) do entry = e; break end
  assert(entry ~= nil, "active firing entry exists")
  assert_eq(fold.body.chat_id, entry.chat_id,
    "folded chat.append rides the firing's chat_id")
end

-- ------------------------------------------------------------------
-- Scenario 17 (Fix #2 negative): role=user does NOT translate
-- ------------------------------------------------------------------
--
-- The user-message path is owned by the chat-runner / agentic-loop —
-- folding role=user from chat.message.append would double-emit user
-- turns into provider chat history. Only role=system folds.
do
  fresh()
  dispatch_agent("firing-fold-2", {
    system_prompt  = "You are a builder.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Build.",
  })

  _test.calls_clear()
  feed("nefor-tui", {
    kind = "chat.message.append",
    role = "user",
    text = "extra user input",
  })

  local calls = decode_calls()
  local leak = find_call(calls, function(c)
    return c.body.kind == "mock-prov.chat.append"
       and type(c.body.message) == "table"
       and c.body.message.role == "user"
  end)
  assert(leak == nil,
    "role=user chat.message.append MUST NOT fold into <provider>.chat.append; got "
    .. json.encode(_test.calls()))

  -- Also assert: an assistant role doesn't fold either.
  _test.calls_clear()
  feed("nefor-tui", {
    kind = "chat.message.append",
    role = "assistant",
    text = "extra assistant text",
  })
  local leak2 = find_call(decode_calls(), function(c)
    return c.body.kind == "mock-prov.chat.append"
       and type(c.body.message) == "table"
       and c.body.message.role == "assistant"
  end)
  assert(leak2 == nil,
    "role=assistant chat.message.append MUST NOT fold either")
end

-- ------------------------------------------------------------------
-- Scenario 18 (Fix #2 negative): no fold when no firing is active
-- ------------------------------------------------------------------
--
-- AGENTS.md envelopes flowing on the bus when no agent firing is
-- active (e.g. the user's lead-workflow chat hits a tool itself)
-- MUST NOT produce spurious <provider>.chat.append envelopes — there's
-- no agent to fold them into.
do
  fresh()
  -- No dispatch_agent call — agents table is empty.
  _test.calls_clear()
  feed("tool-gate", {
    kind = "chat.message.append",
    role = "system",
    text = "[Loaded /tmp/AGENTS.md ...]\n\nfoo",
  })

  local calls = decode_calls()
  local leak = find_call(calls, function(c)
    return string.sub(c.body.kind or "", -#"chat.append") == "chat.append"
  end)
  assert(leak == nil,
    "system chat.message.append with no active firing MUST NOT fold; got "
    .. json.encode(_test.calls()))
end

-- ------------------------------------------------------------------
-- Scenario 19 (Fix #3): agent firing's chat_id is registered as
-- stream-hidden in agentic-loop's stream-suppression map
-- ------------------------------------------------------------------
--
-- Pre-fix sub-agent firings emitted `<provider>.stream.delta` envelopes
-- that the openai-provider wrapper translated into `chat.stream.delta`
-- and the chat surface rendered. The user saw a noisy stream of
-- sub-agent reasoning interleaved with the lead's response. Fix #3
-- registers each agent firing's chat_id as stream-hidden in the
-- agentic-loop's chat_id_stream_explicitly_hidden table so the
-- wrapper's `stream_suppressed` gate drops the sub-agent's stream
-- events.
--
-- Verified-fail-pre-fix: removing the `register_chat_stream_hidden`
-- call in agent.lua's handle() trips this assertion.
do
  fresh()
  dispatch_agent("firing-hide-1", {
    system_prompt  = "You are a builder.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Build.",
  })

  -- The agent's chat_id must be registered as stream-hidden in
  -- agentic-loop's state.
  local entry
  for _, e in pairs(agent._internals.agents) do entry = e; break end
  assert(entry ~= nil, "active firing entry exists")
  assert(agentic_loop.stream_suppressed(entry.chat_id) == true,
    "agent firing's chat_id MUST be stream-suppressed (so the wrapper drops sub-agent stream events)")

  -- A chat_id we never minted MUST NOT be suppressed (default false).
  assert(agentic_loop.stream_suppressed("never-seen-chat-id") == false,
    "unknown chat_id is not suppressed (lead's chat_id, etc., still streams)")
end

-- ------------------------------------------------------------------
-- Scenario 20 (Fix #3): stream-hidden registration is unwound when
-- the firing terminates
-- ------------------------------------------------------------------
--
-- Without unregister-on-close the chat_id_stream_explicitly_hidden
-- table grows monotonically across firings, leaking memory and
-- (worse) suppressing future chat.stream.delta envelopes for any
-- chat that happens to reuse a recycled chat_id.
do
  fresh()
  dispatch_agent("firing-hide-2", {
    system_prompt  = "You are a builder.",
    model          = "test-model",
    tool_allowlist = { "read_file" },
    prompt         = "Build.",
  })

  local entry
  for _, e in pairs(agent._internals.agents) do entry = e; break end
  assert(entry ~= nil, "active firing entry exists")
  local chat_id = entry.chat_id
  assert(agentic_loop.stream_suppressed(chat_id) == true,
    "registered as stream-hidden during firing")

  -- Terminate the firing via a text-only provider reply.
  _test.calls_clear()
  feed("mock-prov", {
    kind    = "mock-prov.chat.complete.result",
    chat_id = chat_id,
    output  = { text = "done.", finish_reason = "stop" },
  })

  -- Firing closed.
  assert(agent._internals.agents["firing-hide-2"] == nil,
    "agent state cleared on terminal")
  -- And the stream-hidden registration is gone.
  assert(agentic_loop.stream_suppressed(chat_id) == false,
    "stream-hidden registration MUST be released when the firing closes")
end

print("agent_test: ok")
