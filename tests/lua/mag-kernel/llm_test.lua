-- tests/lua/mag-kernel/llm_test.lua — unit tests for the llm provider-boundary
-- factory (plugins/mag/lua/mag-kernel/factories/llm.lua).
--
-- Driven from engine/tests/starter_mag_kernel_test.rs (installs the minimal
-- nefor.log surface, points package.path at plugins/mag/lua/mag-kernel/). Tests the
-- factory in isolation: a capturing `emit` stands in for the kernel outbound,
-- so no real provider, bus, or router is needed. Covers the task's factory-
-- level list: capability.invoke on a graph activation; tool-calls reply →
-- ToolCalls + mag.complete; no-tool-calls reply → TextAnswer + mag.complete;
-- provider-error reply → mag.failed; kill mid-flight → provider cancel
-- envelope; drain idle vs in-flight. Plus transcript seeding (params.history):
-- seed replays ahead of the round-1 activation, seed + accumulated turns stay
-- ordered across a tool round, malformed seeds fail construction with the
-- detail, and an absent seed is today's behavior.

local Registry = require("registry")
local llm = require("factories.llm")

-- ------------------------------------------------------------------
-- helpers
-- ------------------------------------------------------------------

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ", tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

-- Capturing emit sink standing in for the kernel's outbound.
local function capture()
  local out = {}
  return out, function(message) out[#out + 1] = message end
end

local function find_kind(msgs, kind)
  for _, m in ipairs(msgs) do
    if m.kind == kind then return m end
  end
  return nil
end

local function find_last_kind(msgs, kind)
  for i = #msgs, 1, -1 do
    if msgs[i].kind == kind then return msgs[i] end
  end
  return nil
end

local function conversation(id, facts)
  facts = facts or {}
  return {
    id = id .. ":conversation",
    turn_id = id .. ":turn",
    provenance = { actor_id = id, run_id = id .. ":run" },
    emit = function(fact) facts[#facts + 1] = fact end,
  }, facts
end

local function conversation_messages(facts)
  local messages, by_id, exchanges = {}, {}, {}
  for _, fact in ipairs(facts) do
    if fact.kind == "message_started" then
      local message = {
        role = fact.role,
        content = nil,
        text = "",
        structured = {},
        tool_call_id = fact.tool_call_id,
        name = fact.name,
        tool_calls = {},
        completed = false,
      }
      messages[#messages + 1] = message
      by_id[fact.message_id] = message
    elseif fact.kind == "content_chunk_appended" then
      local message = by_id[fact.message_id]
      if message and fact.chunk.kind == "text" then
        message.text = message.text .. fact.chunk.data
      elseif message and fact.chunk.kind == "structured" then
        message.structured[#message.structured + 1] = fact.chunk.data
      end
    elseif fact.kind == "tool_exchange_started" then
      exchanges[fact.exchange_id] = {
        message = by_id[fact.message_id],
        tool_call_id = fact.tool_call_id,
        name = fact.tool_name,
      }
    elseif fact.kind == "tool_call_completed" then
      local exchange = exchanges[fact.exchange_id]
      if exchange and exchange.message then
        exchange.message.tool_calls[#exchange.message.tool_calls + 1] = {
          id = exchange.tool_call_id,
          name = fact.call.name or exchange.name,
          arguments = fact.call.arguments,
        }
      end
    elseif fact.kind == "message_completed" then
      local message = by_id[fact.message_id]
      if message then message.completed = true end
    end
  end
  local completed = {}
  for _, message in ipairs(messages) do
    if message.completed then
      if message.text ~= "" then
        message.content = message.text
      elseif #message.structured == 1 then
        message.content = message.structured[1]
      elseif #message.structured > 1 then
        message.content = message.structured
      end
      message.text = nil
      message.structured = nil
      message.completed = nil
      completed[#completed + 1] = message
    end
  end
  return completed
end

-- Construct an llm instance with a fresh capture. Consumes the ready confirm.
local function make(id, params)
  local msgs, emit = capture()
  local dependency, facts = conversation(id)
  local instance = llm.construct(id, params or {}, emit, { conversation = dependency })
  return instance, msgs, facts
end

-- A graph activation carrying one ProviderInput message.
local function turn(message)
  return {
    shape = "single",
    messages = { { from = "upstream", tag = "generic-provider.ProviderOut", message = message } },
  }
end

-- ==================================================================
-- declaration registers and exposes the typed contract
-- ==================================================================

do
  local reg = Registry.new()
  local decl, err = reg:register({ declaration = llm.declaration, construct = llm.construct })
  assert_true(decl ~= nil and err == nil, "llm factory registers cleanly")
  assert_eq(reg:declared_input("llm", "provider_input"), "generic-provider.ProviderOut",
    "declares a single generic-provider.ProviderOut input")

  local outs = {}
  for _, t in ipairs(reg:declaration("llm").outputs) do outs[t] = true end
  assert_true(outs["generic-tool.ToolCalls"], "declares the ToolCalls exit")
  assert_true(outs["generic-provider.TextAnswer"], "declares the TextAnswer exit")

  local sigs = {}
  for _, s in ipairs(reg:declaration("llm").signals) do sigs[s] = true end
  assert_true(sigs.kill and sigs.drain and sigs.steer,
    "declares the kill, drain, and steer signals")
end

-- ==================================================================
-- steering lands after the current exchange and before the next round
-- ==================================================================

do
  local instance, msgs, facts = make("lead.llm", { provider = "p" })
  instance.deliver(turn({ messages = { { role = "user", content = "first" } } }))
  local first = find_last_kind(msgs, "capability.invoke")
  assert_true(instance.handle_steer({ role = "user", content = "queued" }),
    "llm accepts a user steering message")
  instance.deliver({
    kind = "reply", ref = first.ref,
    result = { text = "current answer", finish_reason = "stop" },
  })

  assert_true(find_kind(msgs, "generic-provider.TextAnswer") == nil,
    "a queued steer keeps the run open instead of finishing after the current answer")
  local second = find_last_kind(msgs, "capability.invoke")
  local history = conversation_messages(facts)
  assert_eq(history[1].content, "first", "original user message stays first")
  assert_eq(history[2].content, "current answer", "current assistant turn finishes before steer")
  assert_eq(history[3].content, "queued", "steer is the next user turn")
  assert_true(second.request.input == nil,
    "the next provider invocation does not duplicate canonical history")
end

do
  local instance, msgs, facts = make("lead-tools.llm", { provider = "p" })
  instance.deliver(turn({ messages = { { role = "user", content = "first" } } }))
  local first = find_last_kind(msgs, "capability.invoke")
  instance.handle_steer({ role = "user", content = "queued" })
  instance.deliver({
    kind = "reply", ref = first.ref,
    result = {
      text = "", finish_reason = "tool_calls",
      tool_calls = { { id = "call-1", name = "read", args = {} } },
    },
  })
  instance.deliver(turn({ messages = {
    { role = "tool", tool_call_id = "call-1", name = "read", content = "result" },
  } }))

  local second = find_last_kind(msgs, "capability.invoke")
  local history = conversation_messages(facts)
  assert_eq(history[#history - 1].role, "tool", "tool result precedes steering")
  assert_eq(history[#history].content, "queued", "steering is appended after tool results")
  assert_true(second.request.input == nil,
    "the continuation invocation stays thin")
end

-- ==================================================================
-- construct emits ready; a graph activation emits capability.invoke and defers
-- ==================================================================

do
  local instance, msgs, facts = make("docs-explorer.llm", {
    model = "opus",
    system = "Explore the codebase.",
    tools = { "fs/read", "grep" },
    provider = "chatgpt-provider",
    reasoning_effort = "high",
  })

  local ready = find_kind(msgs, "mag.ready")
  assert_true(ready ~= nil and ready.from == "docs-explorer.llm",
    "construction emits an id-signed ready confirm")

  local completion = instance.deliver(turn({ messages = {
    { role = "user", content = "prior turn" },
  } }))
  assert_eq(completion.status, "pending", "a provider turn defers completion")

  local inv = find_kind(msgs, "capability.invoke")
  assert_true(inv ~= nil, "emits a capability.invoke for the provider request")
  assert_eq(inv.from, "docs-explorer.llm", "capability.invoke is id-signed")
  assert_eq(inv.capability, "chatgpt-provider", "invokes the params-selected provider capability")
  assert_eq(inv.request.model, "opus", "request carries params.model")
  assert_eq(inv.request.tools[1], "fs/read", "request carries params.tools")
  assert_eq(inv.request.reasoning_effort, "high", "request carries the resolved reasoning effort")
  assert_true(inv.request.system == nil and inv.request.input == nil,
    "provider invocation contains metadata but no canonical conversation payload")
  local history = conversation_messages(facts)
  assert_eq(history[1].role, "system", "params.system becomes canonical context")
  assert_eq(history[1].content, "Explore the codebase.", "canonical system content is preserved")
  assert_eq(history[2].content, "prior turn", "the incoming turn becomes canonical context")
  assert_true(type(inv.ref) == "string" and inv.ref:find("@r1", 1, true) ~= nil,
    "factory mints a request-scoped correlation handle")
  assert_eq(inv.request.chat_id, nil, "provider requests carry no legacy chat_id")
end

-- ==================================================================
-- tool-calls reply → generic-tool.ToolCalls + mag.complete
-- ==================================================================

do
  local instance, msgs = make("code-writer.llm", { provider = "p" })
  instance.deliver(turn({}))
  local invoke = find_kind(msgs, "capability.invoke")

  local completion = instance.deliver({
    kind = "reply",
    ref = invoke.ref,
    result = { tool_calls = { { name = "fs/read", arguments = { path = "x" } } } },
  })
  assert_eq(completion, nil, "the reply delivery returns nil (deferred success via emit)")

  local calls = find_kind(msgs, "generic-tool.ToolCalls")
  assert_true(calls ~= nil, "a reply with tool calls emits generic-tool.ToolCalls")
  assert_eq(calls.from, "code-writer.llm", "ToolCalls is id-signed")
  -- Canonical ToolCalls payload: { kind, from, calls = { { id, name, args } } }.
  -- The provider boundary normalizes the native shape (name/arguments) here.
  assert_eq(calls.calls[1].name, "fs/read", "ToolCalls carries a canonical call name")
  assert_eq(calls.calls[1].args.path, "x", "the provider arguments are normalized to .args")
  assert_true(calls.tool_calls == nil, "the raw provider tool_calls field is not surfaced")
  assert_true(find_kind(msgs, "mag.complete") ~= nil, "deferred success signalled with mag.complete")
  assert_true(find_kind(msgs, "generic-provider.TextAnswer") == nil,
    "no TextAnswer when tool calls are present")
end

-- ==================================================================
-- malformed tool arguments are quarantined and corrected in-model
-- ==================================================================

local function count_kind(messages, kind)
  local count = 0
  for _, message in ipairs(messages) do
    if message.kind == kind then count = count + 1 end
  end
  return count
end

for _, case in ipairs({
  { label = "empty", arguments = "", diagnostic = "was empty" },
  { label = "malformed", arguments = "{\"path\":", diagnostic = "malformed JSON" },
  { label = "string", arguments = '"path"', diagnostic = "JSON string" },
  { label = "number", arguments = "42", diagnostic = "JSON number" },
  { label = "null", arguments = "null", diagnostic = "JSON null" },
  { label = "array", arguments = "[]", diagnostic = "JSON array" },
}) do
  local instance, msgs, facts = make("invalid-" .. case.label .. ".llm", {
    provider = "p", max_tool_call_corrections = 1,
  })
  instance.deliver(turn({ messages = { { role = "user", content = "inspect" } } }))
  local first = find_last_kind(msgs, "capability.invoke")
  instance.deliver({ kind = "reply", ref = first.ref, result = {
    finish_reason = "tool_calls",
    tool_calls = { { id = "call-bad", name = "read_file", arguments = case.arguments } },
  } })
  assert_true(find_kind(msgs, "generic-tool.ToolCalls") == nil,
    case.label .. " arguments never leave the provider boundary as executable calls")
  local second = find_last_kind(msgs, "capability.invoke")
  assert_true(second ~= first, case.label .. " arguments trigger another provider completion")
  local history = conversation_messages(facts)
  local feedback = history[#history]
  assert_eq(feedback.role, "user", "correction feedback is provider-valid user context")
  assert_true(feedback.content:find("not executed", 1, true) ~= nil,
    "correction states that the call was not executed")
  assert_true(feedback.content:find(case.diagnostic, 1, true) ~= nil,
    case.label .. " correction includes diagnostic " .. case.diagnostic .. ": " .. feedback.content)
  assert_true(feedback.content:find("call-bad", 1, true) ~= nil
      and feedback.content:find("read_file", 1, true) ~= nil,
    "correction identifies the attempted call")
  for _, message in ipairs(history) do
    assert_true(#message.tool_calls == 0,
      "quarantined malformed calls never enter canonical assistant history")
  end
end

do
  local instance, msgs, facts = make("valid-object.llm", { provider = "p" })
  instance.deliver(turn({ messages = { { role = "user", content = "inspect" } } }))
  instance.deliver({ kind = "reply", ref = find_last_kind(msgs, "capability.invoke").ref,
    result = { tool_calls = {
      { id = "empty-object", name = "zero", arguments = "{}" },
      { id = "required-object", name = "read", arguments = '{"path":"x"}' },
    } } })
  local calls = find_kind(msgs, "generic-tool.ToolCalls")
  assert_true(calls ~= nil and #calls.calls == 2, "JSON objects proceed to normal tool handling")
  assert_eq(calls.calls[2].args.path, "x", "object strings decode before tool execution")
  local history = conversation_messages(facts)
  assert_eq(history[#history].tool_calls[1].arguments ~= nil, true,
    "valid calls remain canonical and replayable")
end

do
  local instance, msgs, facts = make("mixed-invalid.llm", { provider = "p" })
  instance.deliver(turn({ messages = { { role = "user", content = "inspect" } } }))
  instance.deliver({ kind = "reply", ref = find_last_kind(msgs, "capability.invoke").ref,
    result = { tool_calls = {
      { id = "valid", name = "read", arguments = { path = "x" } },
      { id = "invalid", name = "write", arguments = "" },
    } } })
  assert_true(find_kind(msgs, "generic-tool.ToolCalls") == nil,
    "a mixed batch is atomic and executes no calls when one call is invalid")
  for _, message in ipairs(conversation_messages(facts)) do
    assert_true(#message.tool_calls == 0, "the entire malformed batch is quarantined from history")
  end
end

do
  local instance, msgs = make("bounded-invalid.llm", {
    provider = "p", max_tool_call_corrections = 1,
  })
  instance.deliver(turn({ messages = { { role = "user", content = "inspect" } } }))
  local first = find_last_kind(msgs, "capability.invoke")
  instance.deliver({ kind = "reply", ref = first.ref,
    result = { tool_calls = { { id = "bad-1", name = "read", arguments = "" } } } })
  local second = find_last_kind(msgs, "capability.invoke")
  instance.deliver({ kind = "reply", ref = second.ref,
    result = { tool_calls = { { id = "bad-2", name = "read", arguments = "[]" } } } })
  assert_eq(count_kind(msgs, "capability.invoke"), 2,
    "repeated malformed calls stop at the configured correction bound")
  assert_true(find_kind(msgs, "mag.failed") ~= nil,
    "exhausting correction feedback settles the actor instead of looping")
end

-- ==================================================================
-- reply without tool calls → generic-provider.TextAnswer + mag.complete
-- ==================================================================

do
  local instance, msgs = make("code-writer.llm", { provider = "p" })
  instance.deliver(turn({}))
  local invoke = find_kind(msgs, "capability.invoke")

  instance.deliver({
    kind = "reply",
    ref = invoke.ref,
    result = { text = "done", text_answer = { text = "done" } },
  })

  local final = find_kind(msgs, "generic-provider.TextAnswer")
  assert_true(final ~= nil, "a reply without tool calls emits generic-provider.TextAnswer")
  assert_eq(final.text, "done", "TextAnswer carries the provider text")
  assert_true(find_kind(msgs, "mag.complete") ~= nil, "deferred success signalled with mag.complete")
  assert_true(find_kind(msgs, "generic-tool.ToolCalls") == nil,
    "no ToolCalls when the result has none")

  -- An empty tool_calls array is not "tool calls present" — classifies as final.
  local i2, m2 = make("x.llm", { provider = "p" })
  i2.deliver(turn({}))
  i2.deliver({ kind = "reply", ref = find_kind(m2, "capability.invoke").ref, result = { tool_calls = {}, text = "hi" } })
  assert_true(find_kind(m2, "generic-provider.TextAnswer") ~= nil,
    "an empty tool_calls array classifies as a TextAnswer")
end

-- ==================================================================
-- per-round transcript replay: round 2 carries the whole conversation, and the
-- round counter increments by one per activation
-- ==================================================================

do
  local instance, msgs, facts = make("agent.llm", { provider = "p" })
  instance.deliver(turn({ messages = { { role = "user", content = "list the repo" } } }))
  local r1 = find_kind(msgs, "capability.invoke")
  assert_true(r1.ref:find("@r1", 1, true) ~= nil,
    "the first activation mints @r1, not a continued counter")
  assert_eq(#conversation_messages(facts), 1, "round 1 records the seed turn once")

  instance.deliver({
    kind = "reply",
    ref = r1.ref,
    result = {
      text = "",
      finish_reason = "tool_calls",
      tool_calls = { { id = "call-1", name = "list_dir", arguments = { path = "." } } },
    },
  })

  -- The tool result comes back as the next ProviderInput turn (tool-result.lua).
  instance.deliver(turn({ messages = {
    { role = "tool", tool_call_id = "call-1", name = "list_dir", content = "dir-listing" },
  } }))
  local r2
  for _, m in ipairs(msgs) do
    if m.kind == "capability.invoke" and m ~= r1 then r2 = m end
  end
  assert_true(r2 ~= nil, "round 2 emits a second capability.invoke")
  assert_true(r2.ref:find("@r2", 1, true) ~= nil,
    "round 2 mints @r2 — one increment per activation")

  -- Each round runs on a fresh provider chat, so the request must replay the
  -- WHOLE conversation: [user, assistant(tool_calls), tool]. A request carrying
  -- only the tool result is exactly what a real provider rejects ("tool message
  -- without preceding tool_calls") — the session-27c60892 r4 failure.
  local replay = conversation_messages(facts)
  assert_eq(#replay, 3, "round 2 reads the whole canonical transcript")
  assert_eq(replay[1].role, "user", "transcript starts with the seed turn")
  assert_eq(replay[1].content, "list the repo", "the seed turn is verbatim")
  assert_eq(replay[2].role, "assistant", "the model's tool-call turn is recorded")
  assert_eq(replay[2].tool_calls[1].id, "call-1", "the recorded call keeps the model's id")
  assert_eq(replay[2].tool_calls[1].name, "list_dir",
    "the recorded call keeps its universal tool name")
  assert_eq(replay[2].tool_calls[1].arguments.path, ".",
    "canonical arguments stay structured")
  assert_eq(replay[3].role, "tool", "the tool result follows the call")
  assert_eq(replay[3].tool_call_id, "call-1", "the tool result pairs with the call id")
  assert_eq(replay[3].content, "dir-listing", "one logical tool message reaches canonical provider context")
  assert_eq(#replay, 3, "the provider-context boundary does not duplicate value.content")
  assert_true(r2.request.input == nil,
    "round 2 does not replay the transcript on the bus")
end

-- ==================================================================
-- transcript seeding (params.history, turn-as-function): the seed replays
-- ahead of the current activation on round 1
-- ==================================================================

do
  local seed = {
    { role = "user", content = "what is the plan?" },
    { role = "assistant", content = "step one, then step two." },
  }
  local instance, msgs, facts = make("turn.llm", {
    provider = "p",
    system = "You are the lead.",
    history = seed,
  })
  instance.deliver(turn({ messages = { { role = "user", content = "do step one" } } }))

  local inv = find_kind(msgs, "capability.invoke")
  local sent = conversation_messages(facts)
  assert_eq(#sent, 4, "round 1 records system, seed, and activation once")
  assert_eq(sent[1].role, "system", "the authored system message leads canonical context")
  assert_eq(sent[1].content, "You are the lead.", "the system message is verbatim")
  assert_eq(sent[2].role, "user", "seed message 1 follows the system message")
  assert_eq(sent[2].content, "what is the plan?", "seed message 1 is verbatim")
  assert_eq(sent[3].role, "assistant", "seed message 2 follows in order")
  assert_eq(sent[3].content, "step one, then step two.", "seed message 2 is verbatim")
  assert_eq(sent[4].role, "user", "the activation turn comes after the whole seed")
  assert_eq(sent[4].content, "do step one", "the activation turn is verbatim")

  -- System precedence: params.system and params.history are orthogonal
  -- authored inputs. Both become canonical messages in their original order.
  assert_true(inv.request.system == nil and inv.request.input == nil,
    "authored context stays out of the provider invocation")
  local i2, _, f2 = make("sys.llm", {
    provider = "p",
    system = "from params",
    history = { { role = "system", content = "from seed" } },
  })
  i2.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  local authored = conversation_messages(f2)
  assert_eq(authored[1].content, "from params",
    "params.system remains the first authored system message")
  assert_eq(authored[2].role, "system",
    "the system-role seed entry remains a system message")
  assert_eq(authored[2].content, "from seed",
    "the seed's system content is untouched")
end

-- ==================================================================
-- transcript seeding + multi-round: the seed prefix and accumulated turns
-- stay in order across a tool round
-- ==================================================================

do
  local instance, msgs, facts = make("turn.llm", {
    provider = "p",
    history = {
      { role = "user", content = "earlier question" },
      {
        role = "assistant",
        content = "",
        tool_calls = { { id = "call-0", type = "function", ["function"] = { name = "grep", arguments = "{\"q\":\"x\"}" } } },
      },
      { role = "tool", tool_call_id = "call-0", name = "grep", content = "earlier result" },
      { role = "assistant", content = "earlier answer" },
    },
  })
  instance.deliver(turn({ messages = { { role = "user", content = "new question" } } }))
  local r1 = find_kind(msgs, "capability.invoke")
  assert_eq(#conversation_messages(facts), 5,
    "round 1 records seed (4) + activation (1) canonically")

  instance.deliver({
    kind = "reply",
    ref = r1.ref,
    result = {
      text = "",
      finish_reason = "tool_calls",
      tool_calls = { { id = "call-1", name = "list_dir", arguments = { path = "." } } },
    },
  })
  instance.deliver(turn({ messages = {
    { role = "tool", tool_call_id = "call-1", name = "list_dir", content = "dir-listing" },
  } }))

  local r2
  for _, m in ipairs(msgs) do
    if m.kind == "capability.invoke" and m ~= r1 then r2 = m end
  end
  local replay = conversation_messages(facts)
  assert_eq(#replay, 7, "round 2 sees seed + accumulated canonical turns")
  assert_eq(replay[1].content, "earlier question", "the seed prefix still leads")
  assert_eq(replay[2].tool_calls[1].id, "call-0", "the seeded tool-call turn survives verbatim")
  assert_eq(replay[3].tool_call_id, "call-0", "the seeded tool result stays paired")
  assert_eq(replay[4].content, "earlier answer", "the seeded final answer stays in place")
  assert_eq(replay[5].content, "new question", "the first live turn follows the seed")
  assert_eq(replay[6].role, "assistant", "the accumulated tool-call turn is recorded after it")
  assert_eq(replay[6].tool_calls[1].id, "call-1", "the live call keeps the model's id")
  assert_eq(replay[7].tool_call_id, "call-1", "the live tool result closes the sequence")
  assert_true(r2.request.input == nil,
    "the accumulated transcript remains outside the provider invocation")
end

-- ==================================================================
-- canonical conversation facts replace transcript reconstruction metadata.
-- ==================================================================

do
  local instance, msgs, facts = make("turn.llm", {
    provider = "p",
    history = {
      { role = "user", content = "earlier question" },
      { role = "assistant", content = "earlier answer" },
    },
  })
  instance.deliver(turn({ messages = { { role = "user", content = "new question" } } }))
  local r1 = find_kind(msgs, "capability.invoke")
  instance.deliver({
    kind = "reply",
    ref = r1.ref,
    result = {
      text = "",
      finish_reason = "tool_calls",
      tool_calls = { { id = "call-1", name = "list_dir", arguments = { path = "." } } },
    },
  })
  instance.deliver(turn({ messages = {
    { role = "tool", tool_call_id = "call-1", name = "list_dir", content = "dir-listing" },
  } }))
  local r2
  for _, m in ipairs(msgs) do
    if m.kind == "capability.invoke" and m ~= r1 then r2 = m end
  end
  instance.deliver({ kind = "reply", ref = r2.ref, result = { text = "the answer" } })

  local final = find_kind(msgs, "generic-provider.TextAnswer")
  assert_true(final ~= nil, "the run ends in a TextAnswer")
  assert_eq(final.transcript_delta, nil, "TextAnswer carries no transcript reconstruction data")
  assert_eq(facts[1].kind, "created", "the logical actor conversation is explicit")
  assert_eq(facts[#facts].kind, "turn_completed", "the turn closes at final output")
  local started, exchanges, results = 0, 0, 0
  for _, fact in ipairs(facts) do
    if fact.kind == "message_started" then started = started + 1 end
    if fact.kind == "tool_exchange_started" then exchanges = exchanges + 1 end
    if fact.kind == "tool_result_recorded" then results = results + 1 end
  end
  assert_eq(started, 6,
    "seed and live user, assistant-call, tool, and answer messages are canonical facts")
  assert_eq(exchanges, 1, "the provider tool call becomes one canonical exchange")
  assert_eq(results, 1, "the matching tool result settles that exchange")

  local i2, m2, f2facts = make("plain.llm", { provider = "p" })
  i2.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  i2.deliver({ kind = "reply", ref = find_kind(m2, "capability.invoke").ref, result = { text = "done" } })
  local f2 = find_kind(m2, "generic-provider.TextAnswer")
  assert_eq(f2.transcript_delta, nil, "unseeded turns also rely on canonical facts")
  assert_eq(f2facts[#f2facts].kind, "turn_completed", "unseeded turn closes")
end

-- ==================================================================
-- live provider observations become canonical chunks before terminal settle.
-- ==================================================================

do
  local instance, msgs, facts = make("stream.llm", { provider = "p", model = "opus" })
  instance.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  instance.handle_observation({ binding = "transcript", value = {
    kind = "reasoning", text = "thinking",
  } })
  instance.handle_observation({ binding = "transcript", value = {
    kind = "assistant", text = "answer",
  } })
  instance.deliver({
    kind = "reply",
    ref = find_kind(msgs, "capability.invoke").ref,
    result = { text = "answer", model = "opus", duration_ms = 42, usage = { output_tokens = 2 } },
  })

  local chunks, assistant_starts = {}, 0
  for _, fact in ipairs(facts) do
    if fact.kind == "message_started" and fact.role == "assistant" then
      assistant_starts = assistant_starts + 1
    elseif fact.kind == "content_chunk_appended" then
      chunks[#chunks + 1] = fact.chunk
    end
  end
  assert_eq(assistant_starts, 1, "stream and terminal result share one assistant message")
  assert_eq(chunks[#chunks - 1].kind, "reasoning", "reasoning is canonical before terminal")
  assert_eq(chunks[#chunks].kind, "text", "visible text is canonical before terminal")
  local terminal = facts[#facts]
  assert_eq(terminal.kind, "turn_completed", "streaming turn has one terminal fact")
  assert_eq(terminal.detail.result.duration_ms, 42, "terminal timing metadata is preserved")
  assert_eq(terminal.detail.result.model, "opus", "terminal model metadata is preserved")
end

do
  local instance, msgs, facts = make("observed-usage.llm", { provider = "p" })
  instance.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  instance.handle_observation({ binding = "conversation", value = {
    kind = "usage", prompt_tokens = 290, completion_tokens = 22,
    context_input_tokens = 105,
    model = "observed-model", duration_ms = 50,
  } })
  instance.deliver({
    kind = "reply",
    ref = find_kind(msgs, "capability.invoke").ref,
    result = { text = "done" },
  })

  local terminal = facts[#facts]
  assert_eq(terminal.kind, "turn_completed", "observed usage reaches the terminal fact")
  assert_eq(terminal.detail.usage.input_tokens, 290,
    "aggregate provider prompt usage is preserved for operation statistics")
  assert_eq(terminal.detail.usage.output_tokens, 22,
    "aggregate provider completion usage is preserved with the input count")
  assert_eq(terminal.detail.usage.context_input_tokens, 105,
    "final-request context usage crosses the MAG provider boundary unchanged")
  assert_eq(terminal.detail.model, "observed-model", "usage observation preserves the model")
end

do
  local instance, msgs, facts = make("structured-tool.llm", { provider = "p" })
  instance.deliver(turn({ messages = { { role = "user", content = "inspect" } } }))
  instance.deliver({ kind = "reply", ref = find_kind(msgs, "capability.invoke").ref,
    result = { tool_calls = { { id = "call-1", name = "inspect", args = {} } } } })
  instance.deliver(turn({ messages = { {
    role = "tool", tool_call_id = "call-1", name = "inspect",
    content = { files = { "a.lua", "b.lua" }, count = 2 },
  } } }))
  local structured, exchange, completed_call, tool_message, recorded_result
  for _, fact in ipairs(facts) do
    if fact.kind == "content_chunk_appended" and fact.chunk.kind == "structured" then
      structured = fact.chunk.data
    elseif fact.kind == "tool_exchange_started" then
      exchange = fact
    elseif fact.kind == "tool_call_completed" then
      completed_call = fact.call
    elseif fact.kind == "message_started" and fact.role == "tool" then
      tool_message = fact
    elseif fact.kind == "tool_result_recorded" then
      recorded_result = fact.result
    end
  end
  assert_eq(exchange.tool_call_id, "call-1", "the provider call id remains external correlation data")
  assert_true(exchange.exchange_id ~= exchange.tool_call_id,
    "the manager-owned exchange id is distinct from the provider call id")
  assert_eq(completed_call.tool_call_id, "call-1", "the completed call keeps raw correlation")
  assert_eq(tool_message.tool_call_id, "call-1", "the tool message names the call it answers")
  assert_eq(tool_message.name, "inspect", "the tool message keeps its universal tool name")
  assert_eq(structured.count, 2, "structured universal content is preserved losslessly")
  assert_eq(structured.files[2], "b.lua", "structured content keeps nested values")
  assert_eq(recorded_result.count, 2, "the exchange result stays structured")
end

do
  local instance, msgs, facts = make("reused.llm", {
    provider = "provider-with-an-internal-id",
    conversation_context = {
      messages = { { role = "user", content = "manager history" } },
      history_length = 1,
      watermark = "manager-watermark",
      compaction = { opaque = true },
    },
    context_artifact = { legacy = true },
  })

  instance.deliver(turn({ messages = { { role = "user", content = "first firing" } } }))
  local first_request = find_last_kind(msgs, "capability.invoke")
  assert_true(first_request.request.conversation_context == nil,
    "MAG never forwards a duplicate manager conversation projection")
  assert_true(first_request.request.context_artifact == nil and first_request.request.input == nil,
    "removed provider context compatibility fields are never forwarded")
  instance.deliver({ kind = "reply", ref = first_request.ref, result = { text = "first answer" } })

  instance.deliver(turn({ messages = { { role = "user", content = "second firing" } } }))
  local second_request = find_last_kind(msgs, "capability.invoke")
  instance.deliver({ kind = "reply", ref = second_request.ref, result = { text = "second answer" } })

  local turns, turn_ids, event_ids, message_ids = 0, {}, {}, {}
  for _, fact in ipairs(facts) do
    assert_true(not fact.event_id:find("provider%-with%-an%-internal%-id"),
      "provider identity never enters canonical event identity")
    assert_true(not event_ids[fact.event_id], "event ids remain unique across firings")
    event_ids[fact.event_id] = true
    if fact.kind == "turn_started" then
      turns = turns + 1
      turn_ids[fact.turn_id] = true
    elseif fact.kind == "message_started" then
      assert_true(not message_ids[fact.message_id], "message ids remain unique across firings")
      message_ids[fact.message_id] = true
    end
  end
  local distinct_turns = 0
  for _ in pairs(turn_ids) do distinct_turns = distinct_turns + 1 end
  assert_eq(turns, 2, "a long-lived actor records every independent firing")
  assert_eq(distinct_turns, 2, "each firing receives a distinct stable turn identity")
  assert_eq(facts[#facts].kind, "turn_completed", "the second firing is not dropped after the first terminal")
end

do
  local msgs, emit = capture()
  local facts = {}
  local instance = assert(llm.construct("lead.llm", { provider = "p" }, emit, {
    conversation = {
      id = "root-conversation",
      root_id = "root-conversation",
      is_root = true,
      turn_id = "lead-run-1",
      provenance = { run_id = "lead-run-1", actor_id = "lead.llm" },
      emit = function(fact) facts[#facts + 1] = fact end,
    },
  }))
  instance.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  instance.deliver({ kind = "reply", ref = find_kind(msgs, "capability.invoke").ref,
    result = { text = "done" } })
  assert_eq(facts[1].kind, "turn_started", "manager-created root starts directly with its turn")
  for _, fact in ipairs(facts) do
    assert_eq(fact.conversation_id, "root-conversation", "root identity is explicit")
    assert_eq(fact.turn_id, "lead-run-1", "root turn is run-correlated")
    assert_eq(fact.run_id, "lead-run-1", "every root fact carries run correlation")
  end
end

-- ==================================================================
-- transcript seeding: a malformed seed fails construction with the detail
-- ==================================================================

do
  local _, emit = capture()

  local inst, err = llm.construct("bad.llm", { provider = "p", history = "not a list" }, emit)
  assert_true(inst == nil, "a non-table params.history does not construct")
  assert_true(err:find("params.history", 1, true) ~= nil and err:find("string", 1, true) ~= nil,
    "the error names params.history and the offending type")

  local i2, e2 = llm.construct("bad.llm", { provider = "p", history = { role = "user", content = "x" } }, emit)
  assert_true(i2 == nil, "a single message passed instead of a list does not construct")
  assert_true(e2:find("not a map", 1, true) ~= nil, "the error explains the array requirement")

  local i3, e3 = llm.construct("bad.llm", { provider = "p", history = { { role = "user" }, "loose string" } }, emit)
  assert_true(i3 == nil, "a non-table entry does not construct")
  assert_true(e3:find("params.history[2]", 1, true) ~= nil, "the error points at the offending index")

  local i4, e4 = llm.construct("bad.llm", { provider = "p", history = { { content = "no role" } } }, emit)
  assert_true(i4 == nil, "an entry without a role does not construct")
  assert_true(e4:find("missing a role", 1, true) ~= nil, "the error names the missing role")

  local i5, e5 = llm.construct("bad.llm",
    { provider = "p", history = { { role = "assistant", tool_calls = "call-1" } } }, emit)
  assert_true(i5 == nil, "a non-array tool_calls does not construct")
  assert_true(e5:find("tool_calls", 1, true) ~= nil, "the error names the malformed tool_calls")
end

-- ==================================================================
-- transcript seeding: absent params.history is today's behavior, and an empty
-- seed is indistinguishable from no seed
-- ==================================================================

do
  local plain, pm, pfacts = make("plain.llm", { provider = "p", model = "opus" })
  plain.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  local pi = find_kind(pm, "capability.invoke")
  local plain_history = conversation_messages(pfacts)
  assert_eq(#plain_history, 1, "no seed: round 1 records the activation turn alone")
  assert_eq(plain_history[1].content, "go", "the activation turn is verbatim")
  assert_true(pi.ref:find("@r1", 1, true) ~= nil, "no seed: the round counter starts at @r1")

  local empty, _, efacts = make("empty.llm", { provider = "p", model = "opus", history = {} })
  empty.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  local empty_history = conversation_messages(efacts)
  assert_eq(#empty_history, #plain_history,
    "an empty seed records the same message count as no seed")
  assert_eq(empty_history[1].content, plain_history[1].content,
    "an empty seed records the same content as no seed")

  -- The seed is copied, not aliased: mutating the caller's table after
  -- construct does not bleed into the replayed transcript.
  local caller_seed = { { role = "user", content = "seeded" } }
  local aliased, _, afacts = make("alias.llm", { provider = "p", history = caller_seed })
  caller_seed[2] = { role = "user", content = "injected later" }
  aliased.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  assert_eq(#conversation_messages(afacts), 2,
    "post-construct mutation of the caller's seed table does not reach the transcript")
end

-- ==================================================================
-- a result with finish_reason "error" is a suffered failure, never a TextAnswer
-- ==================================================================

do
  local instance, msgs = make("agent.llm", { provider = "p" })
  instance.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  local invoke = find_kind(msgs, "capability.invoke")

  instance.deliver({
    kind = "reply",
    ref = invoke.ref,
    result = { text = "", finish_reason = "error", error = "HTTP 400: boom" },
  })

  local failed = find_kind(msgs, "mag.failed")
  assert_true(failed ~= nil, "a finish_reason error emits mag.failed")
  assert_eq(failed.failure, "mag.Failed", "mag.failed names the reserved suffered-failure tag")
  assert_eq(failed.value.error, "HTTP 400: boom", "the failure threads the provider's detail")
  assert_true(find_kind(msgs, "generic-provider.TextAnswer") == nil,
    "an errored round must never classify as a TextAnswer (error masking)")
  assert_true(find_kind(msgs, "mag.complete") == nil, "an errored round does not complete-ok")

  -- A detail-less provider error still carries a readable failure.
  local i2, m2 = make("x.llm", { provider = "p" })
  i2.deliver(turn({}))
  i2.deliver({
    kind = "reply",
    ref = find_kind(m2, "capability.invoke").ref,
    result = { text = "", finish_reason = "error" },
  })
  local f2 = find_kind(m2, "mag.failed")
  assert_true(f2 ~= nil and type(f2.value.error) == "string" and #f2.value.error > 0,
    "a detail-less provider error still names the failure")
end

-- ==================================================================
-- provider selection is required: no params.provider fails construction
-- ==================================================================

do
  local _, emit = capture()
  local inst, err = llm.construct("naked.llm", {}, emit)
  assert_true(inst == nil, "an llm with no params.provider does not construct")
  assert_true(type(err) == "string" and err:find("provider", 1, true) ~= nil,
    "the construction error names the missing provider requirement")

  -- An empty-string provider is likewise rejected (not a valid capability name).
  local i2, e2 = llm.construct("naked2.llm", { provider = "" }, emit)
  assert_true(i2 == nil and type(e2) == "string", "an empty provider string also fails construction")

  -- Registry construction propagates the same nil + error (init.lua's
  -- set_construct then logs it and never binds, so the actor never readies).
  local reg = Registry.new()
  reg:register({ declaration = llm.declaration, construct = llm.construct })
  local rinst, rerr = reg:construct("llm", "r.llm", {}, emit, {})
  assert_true(rinst == nil and type(rerr) == "string" and rerr:find("provider", 1, true) ~= nil,
    "registry:construct forwards the missing-provider error")
end

-- ==================================================================
-- provider-error reply → mag.failed (suffered failure)
-- ==================================================================

do
  local instance, msgs = make("code-writer.llm", { provider = "p" })
  instance.deliver(turn({}))
  local invoke = find_kind(msgs, "capability.invoke")

  instance.deliver({ kind = "reply", ref = invoke.ref, error = "provider timed out" })

  local failed = find_kind(msgs, "mag.failed")
  assert_true(failed ~= nil, "a provider error in the reply emits mag.failed")
  assert_eq(failed.failure, "mag.Failed", "mag.failed names the reserved suffered-failure tag")
  assert_eq(failed.value.error, "provider timed out", "the failure carries the provider error")
  assert_true(find_kind(msgs, "mag.complete") == nil, "an errored reply does not also complete-ok")
  assert_true(find_kind(msgs, "generic-provider.TextAnswer") == nil,
    "an errored reply emits no data output")
end

-- ==================================================================
-- kill clears local pending state; routing owns external cancellation
-- ==================================================================

do
  local instance, msgs = make("docs-explorer.llm", { provider = "chatgpt-provider" })
  instance.deliver(turn({}))
  local invoke = find_kind(msgs, "capability.invoke")

  instance.handle_kill()

  assert_true(find_kind(msgs, "tool.cancel") == nil,
    "the provider boundary never emits generated-id cancellation")

  local i2, m2 = make("idle.llm", { provider = "p" })
  i2.handle_kill()
  assert_true(find_kind(m2, "tool.cancel") == nil, "kill while idle emits no cancel")

  -- A late reply after kill is ignored (pending was cleared).
  instance.deliver({ kind = "reply", ref = invoke.ref, result = { text = "late" } })
  assert_true(find_kind(msgs, "generic-provider.TextAnswer") == nil,
    "a reply arriving after kill produces no output")
end

-- ==================================================================
-- drain: idle completes immediately; in-flight defers, then the reply flushes
-- ==================================================================

do
  -- Idle drain → mag.complete now (the unified completion ack).
  local idle, im = make("idle.llm", { provider = "p" })
  idle.handle_drain()
  assert_true(find_kind(im, "mag.complete") ~= nil, "drain while idle completes immediately")

  -- In-flight drain → no immediate completion; a new turn is refused; the
  -- pending reply still flushes its output and completes.
  local busy, bm = make("busy.llm", { provider = "p" })
  busy.deliver(turn({}))
  local invoke = find_kind(bm, "capability.invoke")
  busy.handle_drain()
  assert_true(find_kind(bm, "mag.complete") == nil, "drain while in flight does not complete yet")

  local refused = busy.deliver(turn({ another = true }))
  assert_eq(refused, nil, "a new turn arriving mid-drain is refused (no completion)")
  local invokes = 0
  for _, m in ipairs(bm) do if m.kind == "capability.invoke" then invokes = invokes + 1 end end
  assert_eq(invokes, 1, "no second provider request is started while draining")

  busy.deliver({ kind = "reply", ref = invoke.ref, result = { text = "flush" } })
  assert_true(find_kind(bm, "generic-provider.TextAnswer") ~= nil,
    "the pending reply flushes its output during drain")
  assert_true(find_kind(bm, "mag.complete") ~= nil, "and signals deferred completion")
end

print("mag-kernel llm_test: all assertions passed")
