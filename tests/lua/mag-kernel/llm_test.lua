-- tests/lua/mag-kernel/llm_test.lua — unit tests for the llm provider-boundary
-- factory (starter/mag-kernel/factories/llm.lua).
--
-- Driven from engine/tests/starter_mag_kernel_test.rs (installs the minimal
-- nefor.log surface, points package.path at starter/mag-kernel/). Tests the
-- factory in isolation: a capturing `emit` stands in for the kernel outbound,
-- so no real provider, bus, or router is needed. Covers the task's factory-
-- level list: capability.invoke on a graph activation; tool-calls reply →
-- ToolCalls + mag.complete; no-tool-calls reply → FinalAnswer + mag.complete;
-- provider-error reply → mag.failed; kill mid-flight → provider cancel
-- envelope; drain idle vs in-flight.

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

-- Construct an llm instance with a fresh capture. Consumes the ready barrier.
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
    profile = { temperature = 0 },
  })

  local ready = find_kind(msgs, "mag.ready")
  assert_true(ready ~= nil and ready.from == "docs-explorer.llm",
    "construction emits an id-signed ready barrier")

  local completion = instance.deliver(turn({ messages = { "prior turn" } }))
  assert_eq(completion.status, "pending", "a provider turn defers completion")

  local inv = find_kind(msgs, "capability.invoke")
  assert_true(inv ~= nil, "emits a capability.invoke for the provider request")
  assert_eq(inv.from, "docs-explorer.llm", "capability.invoke is id-signed")
  assert_eq(inv.capability, "chatgpt-provider", "invokes the params-selected provider capability")
  assert_eq(inv.request.model, "opus", "request carries params.model")
  assert_eq(inv.request.system, "Explore the codebase.", "request carries params.system")
  assert_eq(inv.request.tools[1], "fs/read", "request carries params.tools")
  assert_eq(inv.request.profile.temperature, 0, "request passes the profile through")
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
