-- tests/lua/mag-kernel/llm_test.lua — unit tests for the llm provider-boundary
-- factory (plugins/mag/lua/mag-kernel/factories/llm.lua).
--
-- Driven from engine/tests/starter_mag_kernel_test.rs (installs the minimal
-- nefor.log surface, points package.path at plugins/mag/lua/mag-kernel/). Tests the
-- factory in isolation: a capturing `emit` stands in for the kernel outbound,
-- so no real provider, bus, or router is needed. Covers the task's factory-
-- level list: capability.invoke on a graph activation; tool-calls reply →
-- ToolCalls + mag.complete; no-tool-calls reply → FinalAnswer + mag.complete;
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

-- Construct an llm instance with a fresh capture. Consumes the ready confirm.
local function make(id, params)
  local msgs, emit = capture()
  local instance = llm.construct(id, params or {}, emit)
  return instance, msgs
end

-- A graph activation carrying one ProviderOut message.
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
  assert_eq(reg:declared_input("llm", "provider_out"), "generic-provider.ProviderOut",
    "declares a single generic-provider.ProviderOut input")

  local outs = {}
  for _, t in ipairs(reg:declaration("llm").outputs) do outs[t] = true end
  assert_true(outs["generic-tool.ToolCalls"], "declares the ToolCalls exit")
  assert_true(outs["generic-provider.FinalAnswer"], "declares the FinalAnswer exit")

  local sigs = {}
  for _, s in ipairs(reg:declaration("llm").signals) do sigs[s] = true end
  assert_true(sigs.kill and sigs.drain, "declares the kill and drain signals")
end

-- ==================================================================
-- construct emits ready; a graph activation emits capability.invoke and defers
-- ==================================================================

do
  local instance, msgs = make("docs-explorer.llm", {
    model = "opus",
    system = "Explore the codebase.",
    tools = { "fs/read", "grep" },
    provider = "chatgpt-provider",
    reasoning_effort = "high",
  })

  local ready = find_kind(msgs, "mag.ready")
  assert_true(ready ~= nil and ready.from == "docs-explorer.llm",
    "construction emits an id-signed ready confirm")

  local completion = instance.deliver(turn({ messages = { "prior turn" } }))
  assert_eq(completion.status, "pending", "a provider turn defers completion")

  local inv = find_kind(msgs, "capability.invoke")
  assert_true(inv ~= nil, "emits a capability.invoke for the provider request")
  assert_eq(inv.from, "docs-explorer.llm", "capability.invoke is id-signed")
  assert_eq(inv.capability, "chatgpt-provider", "invokes the params-selected provider capability")
  assert_eq(inv.request.model, "opus", "request carries params.model")
  assert_eq(inv.request.system, "Explore the codebase.", "request carries params.system")
  assert_eq(inv.request.tools[1], "fs/read", "request carries params.tools")
  assert_eq(inv.request.reasoning_effort, "high", "request carries the resolved reasoning effort")
  assert_eq(inv.request.input.messages[1], "prior turn", "request carries the incoming turn message")
  assert_true(type(inv.request.chat_id) == "string" and #inv.request.chat_id > 0,
    "factory mints a chat_id request handle")
  assert_eq(inv.ref, inv.request.chat_id, "the capability ref traces the chat_id handle")
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
  assert_true(find_kind(msgs, "generic-provider.FinalAnswer") == nil,
    "no FinalAnswer when tool calls are present")
end

-- ==================================================================
-- reply without tool calls → generic-provider.FinalAnswer + mag.complete
-- ==================================================================

do
  local instance, msgs = make("code-writer.llm", { provider = "p" })
  instance.deliver(turn({}))
  local invoke = find_kind(msgs, "capability.invoke")

  instance.deliver({
    kind = "reply",
    ref = invoke.ref,
    result = { text = "done", final_answer = { text = "done" } },
  })

  local final = find_kind(msgs, "generic-provider.FinalAnswer")
  assert_true(final ~= nil, "a reply without tool calls emits generic-provider.FinalAnswer")
  assert_eq(final.text, "done", "FinalAnswer carries the provider text")
  assert_true(find_kind(msgs, "mag.complete") ~= nil, "deferred success signalled with mag.complete")
  assert_true(find_kind(msgs, "generic-tool.ToolCalls") == nil,
    "no ToolCalls when the result has none")

  -- An empty tool_calls array is not "tool calls present" — classifies as final.
  local i2, m2 = make("x.llm", { provider = "p" })
  i2.deliver(turn({}))
  i2.deliver({ kind = "reply", ref = find_kind(m2, "capability.invoke").ref, result = { tool_calls = {}, text = "hi" } })
  assert_true(find_kind(m2, "generic-provider.FinalAnswer") ~= nil,
    "an empty tool_calls array classifies as a FinalAnswer")
end

-- ==================================================================
-- per-round transcript replay: round 2 carries the whole conversation, and the
-- round counter increments by one per activation
-- ==================================================================

do
  local instance, msgs = make("agent.llm", { provider = "p" })
  instance.deliver(turn({ messages = { { role = "user", content = "list the repo" } } }))
  local r1 = find_kind(msgs, "capability.invoke")
  assert_true(r1.request.chat_id:find("@r1", 1, true) ~= nil,
    "the first activation mints @r1, not a continued counter")
  assert_eq(#r1.request.input.messages, 1, "round 1 carries the seed turn alone")

  instance.deliver({
    kind = "reply",
    ref = r1.ref,
    result = {
      text = "",
      finish_reason = "tool_calls",
      tool_calls = { { id = "call-1", name = "list_dir", arguments = { path = "." } } },
    },
  })

  -- The tool result comes back as the next ProviderOut turn (tool-result.lua).
  instance.deliver(turn({ messages = {
    { role = "tool", tool_call_id = "call-1", name = "list_dir", content = "dir-listing" },
  } }))
  local r2
  for _, m in ipairs(msgs) do
    if m.kind == "capability.invoke" and m ~= r1 then r2 = m end
  end
  assert_true(r2 ~= nil, "round 2 emits a second capability.invoke")
  assert_true(r2.request.chat_id:find("@r2", 1, true) ~= nil,
    "round 2 mints @r2 — one increment per activation")

  -- Each round runs on a fresh provider chat, so the request must replay the
  -- WHOLE conversation: [user, assistant(tool_calls), tool]. A request carrying
  -- only the tool result is exactly what a real provider rejects ("tool message
  -- without preceding tool_calls") — the session-27c60892 r4 failure.
  local replay = r2.request.input.messages
  assert_eq(#replay, 3, "round 2 replays the whole transcript")
  assert_eq(replay[1].role, "user", "transcript starts with the seed turn")
  assert_eq(replay[1].content, "list the repo", "the seed turn is verbatim")
  assert_eq(replay[2].role, "assistant", "the model's tool-call turn is recorded")
  assert_eq(replay[2].tool_calls[1].id, "call-1", "the recorded call keeps the model's id")
  assert_eq(replay[2].tool_calls[1]["function"].name, "list_dir",
    "the recorded call is in the provider wire shape")
  assert_eq(replay[2].tool_calls[1]["function"].arguments, "{\"path\":\".\"}",
    "arguments re-encode as the wire's JSON string")
  assert_eq(replay[3].role, "tool", "the tool result follows the call")
  assert_eq(replay[3].tool_call_id, "call-1", "the tool result pairs with the call id")
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
  local instance, msgs = make("turn.llm", {
    provider = "p",
    system = "You are the lead.",
    history = seed,
  })
  instance.deliver(turn({ messages = { { role = "user", content = "do step one" } } }))

  local inv = find_kind(msgs, "capability.invoke")
  local sent = inv.request.input.messages
  assert_eq(#sent, 3, "round 1 replays the seed ahead of the activation turn")
  assert_eq(sent[1].role, "user", "seed message 1 leads the request")
  assert_eq(sent[1].content, "what is the plan?", "seed message 1 is verbatim")
  assert_eq(sent[2].role, "assistant", "seed message 2 follows in order")
  assert_eq(sent[2].content, "step one, then step two.", "seed message 2 is verbatim")
  assert_eq(sent[3].role, "user", "the activation turn comes after the whole seed")
  assert_eq(sent[3].content, "do step one", "the activation turn is verbatim")

  -- System precedence: params.system and params.history are orthogonal
  -- channels. params.system rides the request's system field; a system-role
  -- seed entry is neither lifted into it nor stripped — it replays verbatim
  -- as an ordinary leading transcript message.
  assert_eq(inv.request.system, "You are the lead.", "params.system stays the system channel")
  local i2, m2 = make("sys.llm", {
    provider = "p",
    system = "from params",
    history = { { role = "system", content = "from seed" } },
  })
  i2.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  local inv2 = find_kind(m2, "capability.invoke")
  assert_eq(inv2.request.system, "from params",
    "a system-role seed entry does not displace params.system")
  assert_eq(inv2.request.input.messages[1].role, "system",
    "the system-role seed entry replays verbatim in the transcript")
  assert_eq(inv2.request.input.messages[1].content, "from seed",
    "the seed's system content is untouched")
end

-- ==================================================================
-- transcript seeding + multi-round: the seed prefix and accumulated turns
-- stay in order across a tool round
-- ==================================================================

do
  local instance, msgs = make("turn.llm", {
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
  assert_eq(#r1.request.input.messages, 5, "round 1 carries seed (4) + activation (1)")

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
  local replay = r2.request.input.messages
  assert_eq(#replay, 7, "round 2 replays seed + accumulated turns")
  assert_eq(replay[1].content, "earlier question", "the seed prefix still leads")
  assert_eq(replay[2].tool_calls[1].id, "call-0", "the seeded tool-call turn survives verbatim")
  assert_eq(replay[3].tool_call_id, "call-0", "the seeded tool result stays paired")
  assert_eq(replay[4].content, "earlier answer", "the seeded final answer stays in place")
  assert_eq(replay[5].content, "new question", "the first live turn follows the seed")
  assert_eq(replay[6].role, "assistant", "the accumulated tool-call turn is recorded after it")
  assert_eq(replay[6].tool_calls[1].id, "call-1", "the live call keeps the model's id")
  assert_eq(replay[7].tool_call_id, "call-1", "the live tool result closes the sequence")
end

-- ==================================================================
-- transcript delta: the FinalAnswer carries everything accumulated beyond the
-- seed — the seed itself is never echoed back
-- ==================================================================

do
  local instance, msgs = make("turn.llm", {
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

  local final = find_kind(msgs, "generic-provider.FinalAnswer")
  assert_true(final ~= nil, "the run ends in a FinalAnswer")
  local delta = final.transcript_delta
  assert_true(type(delta) == "table", "the FinalAnswer carries a transcript_delta")
  assert_eq(#delta, 4, "the delta is the live turns alone: user, tool call, tool result, answer")
  assert_eq(delta[1].role, "user", "the delta opens with the activation's user turn")
  assert_eq(delta[1].content, "new question", "the seed is not echoed back")
  assert_eq(delta[2].role, "assistant", "the assistant tool-call turn is recorded")
  assert_eq(delta[2].tool_calls[1].id, "call-1", "the recorded call keeps the model's id")
  assert_eq(delta[3].role, "tool", "the tool result follows the call")
  assert_eq(delta[3].content, "dir-listing", "the tool result is verbatim")
  assert_eq(delta[4].role, "assistant", "the final answer closes the delta")
  assert_eq(delta[4].content, "the answer", "the final answer text is verbatim")

  -- Without a seed the delta covers the whole owned transcript.
  local i2, m2 = make("plain.llm", { provider = "p" })
  i2.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  i2.deliver({ kind = "reply", ref = find_kind(m2, "capability.invoke").ref, result = { text = "done" } })
  local f2 = find_kind(m2, "generic-provider.FinalAnswer")
  assert_eq(#f2.transcript_delta, 2, "no seed: the delta is the whole transcript")
  assert_eq(f2.transcript_delta[1].content, "go", "the user turn leads")
  assert_eq(f2.transcript_delta[2].content, "done", "the answer closes")
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
  local plain, pm = make("plain.llm", { provider = "p", model = "opus" })
  plain.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  local pi = find_kind(pm, "capability.invoke")
  assert_eq(#pi.request.input.messages, 1, "no seed: round 1 carries the activation turn alone")
  assert_eq(pi.request.input.messages[1].content, "go", "the activation turn is verbatim")
  assert_true(pi.request.chat_id:find("@r1", 1, true) ~= nil, "no seed: the round counter starts at @r1")

  local empty, em = make("empty.llm", { provider = "p", model = "opus", history = {} })
  empty.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  local ei = find_kind(em, "capability.invoke")
  assert_eq(#ei.request.input.messages, #pi.request.input.messages,
    "an empty seed sends the same message count as no seed")
  assert_eq(ei.request.input.messages[1].content, pi.request.input.messages[1].content,
    "an empty seed sends the same content as no seed")

  -- The seed is copied, not aliased: mutating the caller's table after
  -- construct does not bleed into the replayed transcript.
  local caller_seed = { { role = "user", content = "seeded" } }
  local aliased, am = make("alias.llm", { provider = "p", history = caller_seed })
  caller_seed[2] = { role = "user", content = "injected later" }
  aliased.deliver(turn({ messages = { { role = "user", content = "go" } } }))
  local ai = find_kind(am, "capability.invoke")
  assert_eq(#ai.request.input.messages, 2,
    "post-construct mutation of the caller's seed table does not reach the transcript")
end

-- ==================================================================
-- a result with finish_reason "error" is a suffered failure, never a FinalAnswer
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
  assert_true(find_kind(msgs, "generic-provider.FinalAnswer") == nil,
    "an errored round must never classify as a FinalAnswer (error masking)")
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
  assert_true(find_kind(msgs, "generic-provider.FinalAnswer") == nil,
    "an errored reply emits no data output")
end

-- ==================================================================
-- kill mid-flight → provider cancel envelope keyed by the chat_id handle
-- ==================================================================

do
  local instance, msgs = make("docs-explorer.llm", { provider = "chatgpt-provider" })
  instance.deliver(turn({}))
  local invoke = find_kind(msgs, "capability.invoke")
  local handle = invoke.request.chat_id

  instance.handle_kill()

  local cancel = find_kind(msgs, "chatgpt-provider.chat.cancel")
  assert_true(cancel ~= nil, "kill mid-flight emits the provider cancel envelope")
  assert_eq(cancel.chat_id, handle, "cancel is keyed by the in-flight chat_id handle")
  assert_eq(cancel.from, "docs-explorer.llm", "the cancel is id-signed")

  -- Idempotent: no in-flight request → kill emits no cancel.
  local i2, m2 = make("idle.llm", { provider = "p" })
  i2.handle_kill()
  assert_true(find_kind(m2, "p.chat.cancel") == nil, "kill while idle emits no cancel")

  -- A late reply after kill is ignored (pending was cleared).
  instance.deliver({ kind = "reply", ref = invoke.ref, result = { text = "late" } })
  assert_true(find_kind(msgs, "generic-provider.FinalAnswer") == nil,
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
  assert_true(find_kind(bm, "generic-provider.FinalAnswer") ~= nil,
    "the pending reply flushes its output during drain")
  assert_true(find_kind(bm, "mag.complete") ~= nil, "and signals deferred completion")
end

print("mag-kernel llm_test: all assertions passed")
