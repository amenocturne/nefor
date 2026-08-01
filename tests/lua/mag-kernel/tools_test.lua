-- tests/lua/mag-kernel/tools_test.lua — factory-level unit tests for the
-- tool-boundary primitives: run-tool, tool-result.
--
-- Driven from `engine/tests/starter_mag_kernel_test.rs` (same harness as
-- flow_test.lua): a bare Lua VM with a stub `nefor.log` and package.path
-- pointed at `plugins/mag/lua/mag-kernel/`.
--
-- These exercise the factories directly — construct an instance, feed it
-- activation messages (graph deliveries and correlated replies), assert what
-- it emits. Emit is stubbed; there is no real tool-gate: capability.invoke
-- envelopes are captured and their refs replayed as reply activations, exactly
-- as routing.lua's bus_response would.

local Registry    = require("registry")
local run_tool    = require("factories.run-tool")
local tool_result = require("factories.tool-result")
local llm         = require("factories.llm")

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

local function count_kind(msgs, kind)
  local n = 0
  for _, m in ipairs(msgs) do
    if m.kind == kind then n = n + 1 end
  end
  return n
end

local function collect_kind(msgs, kind)
  local out = {}
  for _, m in ipairs(msgs) do
    if m.kind == kind then out[#out + 1] = m end
  end
  return out
end

-- Single-input graph activation (routing.lua, the kernel⇄factory contract).
local function single(from, tag, message)
  return { shape = "single", messages = { { from = from, tag = tag, message = message } } }
end

-- A correlated reply activation, the way routing.lua's bus_response builds it
-- from a captured capability.invoke's ref.
local function reply(ref, result, err)
  return { kind = "reply", ref = ref, result = result, error = err }
end

-- ==================================================================
-- both factories declare well-formed contracts and register
-- ==================================================================

do
  local reg = Registry.new({ require_preview = false })
  for _, mod in ipairs({ run_tool, tool_result }) do
    local decl, err = reg:register({ declaration = mod.declaration, construct = mod.construct })
    assert_true(decl ~= nil and err == nil,
      "factory " .. tostring(mod.declaration.name) .. " registers cleanly: " .. tostring(err))
  end

  -- run-tool: ToolCalls in, the aggregated ToolHandle out (fixture-reconciled).
  local rt = reg:declaration("run-tool")
  assert_eq(rt.inputs.calls, "generic-tool.ToolCalls", "run-tool input is ToolCalls")
  assert_eq(rt.outputs[1], "generic-tool.ToolHandle", "run-tool output is ToolHandle")
  assert_eq(rt.signals[1], "kill", "run-tool declares the kill signal")

  -- tool-result: ToolHandle in, ProviderInput out.
  local tr = reg:declaration("tool-result")
  assert_eq(tr.inputs.handle, "generic-tool.ToolHandle", "tool-result input is ToolHandle")
  assert_eq(tr.outputs[1], "generic-provider.ProviderInput", "tool-result output is ProviderInput")
  assert_eq(#tr.signals, 0, "tool-result declares no signals (synchronous)")
end

-- ==================================================================
-- run-tool: ready on construct, id-signed
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = run_tool.construct("docs-explorer.run-tool", {}, emit)
  assert_true(inst ~= nil, "run-tool constructs")
  local ready = find_kind(msgs, "mag.ready")
  assert_true(ready ~= nil, "run-tool emits ready")
  assert_eq(ready.from, "docs-explorer.run-tool", "ready is id-signed")
end

-- ==================================================================
-- run-tool: two calls → two capability.invokes each carrying the da-policy
-- (and allowlist) from params; pending; replies aggregate into one ToolHandle
-- + mag.complete
-- ==================================================================

do
  local msgs, emit = capture()
  local policy = { allow = { "ls", "grep" }, deny = { "rm", "sudo" } }
  local allowlist = { "fs/read", "grep" }
  local inst = run_tool.construct("rt", { ["da-policy"] = policy, allowlist = allowlist }, emit)

  local pending = inst.deliver(single("llm", "generic-tool.ToolCalls", {
    calls = {
      { id = "call-1", name = "grep", args = { pattern = "foo" } },
      { id = "call-2", name = "fs/read", args = { path = "a.txt" } },
    },
  }))
  assert_eq(pending.status, "pending", "run-tool defers completion while calls are in flight")

  local invokes = collect_kind(msgs, "capability.invoke")
  assert_eq(#invokes, 2, "two calls fan out to two parallel capability.invokes")

  -- each invoke is id-signed, names its tool, and carries the per-node policy
  for _, inv in ipairs(invokes) do
    assert_eq(inv.from, "rt", "invoke is id-signed")
    assert_true(inv.request ~= nil, "invoke carries a request payload")
    assert_eq(inv.request["da-policy"], policy, "invoke carries the node da-policy from params")
    assert_eq(inv.request.allowlist, allowlist, "invoke carries the node allowlist from params")
  end
  assert_eq(invokes[1].capability, "grep", "first invoke targets the first tool")
  assert_eq(invokes[1].request.args.pattern, "foo", "first invoke carries the tool args")
  assert_eq(invokes[2].capability, "fs/read", "second invoke targets the second tool")

  -- no output yet — the batch is incomplete
  assert_true(find_kind(msgs, "generic-tool.ToolHandle") == nil, "no handle before both replies")

  -- reply to the SECOND call first: batch still incomplete, no output, no complete
  local before = #msgs
  local r2 = inst.deliver(reply(invokes[2].ref, { output = "file body" }))
  assert_true(r2 == nil, "an intermediate reply returns nil (still pending)")
  assert_true(find_kind(msgs, "generic-tool.ToolHandle") == nil, "still no handle after one reply")
  assert_true(find_kind(msgs, "mag.complete") == nil, "no completion after one reply")
  assert_true(#msgs == before, "an incomplete batch emits nothing on an intermediate reply")

  -- reply to the FIRST call: batch completes → one aggregated ToolHandle + complete
  local r1 = inst.deliver(reply(invokes[1].ref, { output = "match at line 3" }))
  assert_true(r1 == nil, "the completing reply returns nil — completion arrives via mag.complete")

  assert_eq(count_kind(msgs, "generic-tool.ToolHandle"), 1, "exactly one aggregated handle for the batch")
  local handle = find_kind(msgs, "generic-tool.ToolHandle")
  assert_eq(handle.from, "rt", "handle is id-signed")
  assert_eq(#handle.results, 2, "handle aggregates one result per call")

  -- results are index-ordered to the incoming calls, not reply-arrival order
  assert_eq(handle.results[1].id, "call-1", "result 1 keeps the first call's tool_call_id")
  assert_eq(handle.results[1].name, "grep", "result 1 keeps the first tool name")
  assert_eq(handle.results[1].output.output, "match at line 3", "result 1 carries the first call's output")
  assert_eq(handle.results[2].id, "call-2", "result 2 keeps the second call's tool_call_id")
  assert_eq(handle.results[2].output.output, "file body", "result 2 carries the second call's output")

  local complete = find_kind(msgs, "mag.complete")
  assert_true(complete ~= nil and complete.from == "rt",
    "the completed batch signals async success with an id-signed mag.complete")
end

-- ==================================================================
-- run-tool: a tool-error reply is aggregated (error carried, not dropped)
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = run_tool.construct("rt", {}, emit)
  inst.deliver(single("llm", "generic-tool.ToolCalls", {
    calls = { { id = "c1", name = "bash", args = { cmd = "false" } } },
  }))
  local inv = find_kind(msgs, "capability.invoke")

  -- an errored answer (routing.lua bus_response delivers error, not result)
  inst.deliver(reply(inv.ref, nil, "command failed: exit 1"))

  local handle = find_kind(msgs, "generic-tool.ToolHandle")
  assert_true(handle ~= nil, "an errored call still completes the batch")
  assert_eq(handle.results[1].error, "command failed: exit 1", "the tool error is aggregated on the result")
  assert_true(find_kind(msgs, "mag.complete") ~= nil, "an errored batch still signals completion")
end

-- ==================================================================
-- run-tool: an empty ToolCalls completes synchronously (nothing to await)
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = run_tool.construct("rt", {}, emit)
  local c = inst.deliver(single("llm", "generic-tool.ToolCalls", { calls = {} }))
  assert_eq(c.status, "ok", "an empty batch completes synchronously, not pending")
  local handle = find_kind(msgs, "generic-tool.ToolHandle")
  assert_true(handle ~= nil and #handle.results == 0, "an empty batch emits an empty handle")
  assert_true(count_kind(msgs, "capability.invoke") == 0, "an empty batch makes no invocations")
end

-- ==================================================================
-- run-tool: kill drops in-flight batch state; a voided reply produces nothing
-- (bash finishes server-side, the answer is voided — signal choice)
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = run_tool.construct("rt", {}, emit)
  inst.deliver(single("llm", "generic-tool.ToolCalls", {
    calls = { { id = "c1", name = "bash", args = { cmd = "sleep 1" } } },
  }))
  local inv = find_kind(msgs, "capability.invoke")

  inst.handle_kill()

  -- a late answer to the killed batch resolves nothing (result voided)
  inst.deliver(reply(inv.ref, { output = "too late" }))
  assert_true(find_kind(msgs, "generic-tool.ToolHandle") == nil,
    "a reply after kill produces no aggregated handle")
  assert_true(find_kind(msgs, "mag.complete") == nil,
    "a reply after kill signals no completion")
end

-- ==================================================================
-- tool-result: adapts an aggregated ToolHandle into a ProviderInput turn
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = tool_result.construct("tr", {}, emit)
  local ready = find_kind(msgs, "mag.ready")
  assert_true(ready ~= nil and ready.from == "tr", "tool-result emits an id-signed ready")

  local c = inst.deliver(single("run-tool", "generic-tool.ToolHandle", {
    results = {
      { id = "call-1", name = "grep", output = "match at line 3" },
      { id = "call-2", name = "fs/read", output = { path = "a.txt", bytes = 12 } },
      { id = "call-3", name = "bash", error = "exit 1" },
    },
  }))
  assert_eq(c.status, "ok", "synchronous adaptation returns a successful completion")

  local out = find_kind(msgs, "generic-provider.ProviderInput")
  assert_true(out ~= nil, "tool-result emits a ProviderInput turn")
  assert_eq(out.from, "tr", "ProviderInput is id-signed")
  assert_eq(#out.messages, 3, "one tool message per aggregated result")

  -- string output passes through as content, keyed by the model tool_call_id
  assert_eq(out.messages[1].role, "tool", "adapted messages are tool-role")
  assert_eq(out.messages[1].tool_call_id, "call-1", "message keeps the tool_call_id for provider pairing")
  assert_eq(out.messages[1].content, "match at line 3", "string output passes through as content")

  -- structured output passes through verbatim (no json binding in the kernel VM)
  assert_eq(out.messages[2].content.bytes, 12, "structured output passes through verbatim")

  -- an errored result becomes readable content plus the raw error
  assert_eq(out.messages[3].content, "[tool error] exit 1", "an error becomes readable content")
  assert_eq(out.messages[3].error, "exit 1", "the raw error is preserved on the message")
end

-- ==================================================================
-- pinned ToolCalls payload round-trips llm → run-tool
-- (llm emits the canonical { calls = { { id, name, args } } }; run-tool reads
-- it with no alias fallbacks and fans out one invoke per call)
-- ==================================================================

do
  -- Drive an llm to a tool-calls reply so it emits a real ToolCalls message.
  local lm, lemit = capture()
  local linst = llm.construct("dx.llm", { provider = "p" }, lemit)
  linst.deliver({
    shape = "single",
    messages = { { from = "up", tag = "generic-provider.ProviderInput", message = { messages = {} } } },
  })
  local invoke = find_kind(lm, "capability.invoke")
  linst.deliver({
    kind = "reply",
    ref = invoke.ref,
    result = { tool_calls = {
      { id = "tc-1", name = "grep", arguments = { pattern = "foo" } },
      { id = "tc-2", name = "fs/read", arguments = { path = "a.txt" } },
    } },
  })

  local toolcalls = find_kind(lm, "generic-tool.ToolCalls")
  assert_true(toolcalls ~= nil, "llm emits a generic-tool.ToolCalls")
  assert_eq(#toolcalls.calls, 2, "the canonical payload carries one entry per provider call")
  assert_eq(toolcalls.calls[1].id, "tc-1", "canonical call keeps the model's id")
  assert_eq(toolcalls.calls[1].name, "grep", "canonical call carries the tool name")
  assert_eq(toolcalls.calls[1].args.pattern, "foo", "canonical call normalizes arguments to .args")

  -- Feed exactly that payload into run-tool: it must fan out per call, reading
  -- id/name/args directly (no name/tool or args/arguments fallbacks).
  local rm, remit = capture()
  local rinst = run_tool.construct("dx.run-tool", {}, remit)
  local pending = rinst.deliver(single("dx.llm", "generic-tool.ToolCalls", toolcalls))
  assert_eq(pending.status, "pending", "run-tool defers on the round-tripped batch")

  local invokes = collect_kind(rm, "capability.invoke")
  assert_eq(#invokes, 2, "run-tool fans out one invoke per canonical call")
  assert_eq(invokes[1].capability, "grep", "invoke 1 targets the canonical call name")
  assert_eq(invokes[1].request.args.pattern, "foo", "invoke 1 carries the canonical call args")
  assert_eq(invokes[1].ref.call_id, "tc-1", "invoke 1 correlation keeps the canonical call id")
  assert_eq(invokes[2].capability, "fs/read", "invoke 2 targets the second canonical call name")
  assert_eq(invokes[2].request.args.path, "a.txt", "invoke 2 carries the second call args")
end

print("mag-kernel tools_test: all assertions passed")
