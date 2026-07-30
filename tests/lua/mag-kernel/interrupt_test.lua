-- tests/lua/mag-kernel/interrupt_test.lua — the graceful-interrupt core (W1).
--
-- Driven from `engine/tests/starter_mag_kernel_test.rs` (same bare-VM harness
-- as routing_test.lua). Exercises routing.lua's `interrupt` over a real
-- run-tool → tool-result → sink constellation with the actual factories bound
-- through the construct hook:
--
--   * an interrupt settles the in-flight capability correlation as a FAILED
--     reply "interrupted by user", routes it through run-tool → tool-result
--     (so the model sees a `[tool error] interrupted by user` turn), and emits
--     a `tool.cancel` for the open correlation so the real work stops. The run
--     context stays alive and its actors are untouched (no kills).
--   * a run with NOTHING in flight interrupts cleanly (settles 0, emits no
--     tool.cancel).

local inventory = require("inventory")
local Registry  = require("registry")
local routing   = require("routing")
local run_tool  = require("factories.run-tool")
local tool_res  = require("factories.tool-result")

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

local function noop_logger()
  local function sink() return function() end end
  return { info = sink(), warn = sink(), error = sink() }
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

-- A run-scoped router with the REAL factories, wired the way init.lua wires a
-- run context: registry:construct as the lazy-construction hook, kill through
-- the fold (dispatch_kill → forget), and capturing bus/events sinks. `sink` is
-- a tiny capture factory standing in for the terminal node so ProviderOut is
-- observable.
local function harness()
  local log = noop_logger()
  local bus = {}
  local events = {}
  local captured_provider_out = {}

  local reg = Registry.new({ require_preview = false })
  reg:register({ declaration = run_tool.declaration, construct = run_tool.construct })
  reg:register({ declaration = tool_res.declaration, construct = tool_res.construct })
  reg:register({
    declaration = {
      name = "capture-sink",
      inputs = { input = "generic-provider.ProviderOut" },
      outputs = {},
      signals = {},
    },
    construct = function(id, _params, emit)
      emit({ kind = "mag.ready", from = id })
      return {
        id = id,
        deliver = function(activation)
          local m = ((activation.messages or {})[1] or {}).message
          captured_provider_out[#captured_provider_out + 1] = m
          return { status = "ok" }
        end,
      }
    end,
  })

  local inv = inventory.new({ log = log, registry = reg })
  local seq = 0
  local router = routing.new({
    inventory = inv,
    registry = reg,
    log = log,
    bus_emit = function(env) bus[#bus + 1] = env end,
    events = function(e) events[#events + 1] = e end,
    gen_id = function() seq = seq + 1; return "r1/cap-" .. seq end,
  })
  inv.set_on_kill(function(id)
    router:dispatch_kill(id)
    router:forget(id)
  end)
  router:set_construct(function(record)
    return reg:construct(record.factory, record.id, record.params, router:emitter(record.id), {})
  end)

  -- run-tool --ToolHandle--> tool-result --ProviderOut--> capture-sink.
  local res = inv.apply({
    actors = {
      { id = "rt", factory = "run-tool", params = {},
        evidence={version=2,identity="nefor.factory.run-tool",arguments={},input={kind="named",name="nefor.contracts.ToolCalls",arguments={}},output={kind="named",name="nefor.contracts.ToolHandle",arguments={}}},
        input={type={kind="named",name="nefor.contracts.ToolCalls",arguments={}},wire="generic-tool.ToolCalls"},outputs={{type={kind="named",name="nefor.contracts.ToolHandle",arguments={}},wire="generic-tool.ToolHandle"}},
        routes = { ["generic-tool.ToolHandle"] = { { actor = "tr", wire = "generic-tool.ToolHandle" } } } },
      { id = "tr", factory = "tool-result", params = {},
        evidence={version=2,identity="nefor.factory.tool-result",arguments={},input={kind="named",name="nefor.contracts.ToolHandle",arguments={}},output={kind="named",name="nefor.contracts.ProviderInput",arguments={}}},
        input={type={kind="named",name="nefor.contracts.ToolHandle",arguments={}},wire="generic-tool.ToolHandle"},outputs={{type={kind="named",name="nefor.contracts.ProviderInput",arguments={}},wire="generic-provider.ProviderOut"}},
        routes = { ["generic-provider.ProviderOut"] = { { actor = "cap", wire = "generic-provider.ProviderOut" } } } },
      { id = "cap", factory = "capture-sink", params = {}, routes = {},
        input={type={kind="named",name="nefor.contracts.ProviderInput",arguments={}},wire="generic-provider.ProviderOut"},outputs={} },
    },
  })
  assert_true(res.ok, "constellation applies: " .. tostring(res.error))

  return {
    inv = inv, router = router, bus = bus, events = events,
    provider_out = captured_provider_out,
  }
end

-- ==================================================================
-- interrupt settles the in-flight tool call as a failed reply, routing it
-- through run-tool → tool-result → sink, and cancels the real work
-- ==================================================================

do
  local h = harness()

  -- Fire a ToolCalls into run-tool: it constructs, emits one capability.invoke
  -- (correlation opened, minted id on the bus), and defers.
  h.router:fire("rt", "llm", "generic-tool.ToolCalls", {
    calls = { { id = "call-1", name = "bash", args = { command = "sleep 10" } } },
  })
  local invoke = find_kind(h.bus, "tool.invoke")
  assert_true(invoke ~= nil, "run-tool put a tool.invoke on the bus")
  local req_id = invoke.id
  assert_eq(req_id, "r1/cap-1", "the correlation id is the minted, scoped request id")

  -- Interrupt the run.
  local settled = h.router:interrupt("interrupted by user")
  assert_eq(settled, 1, "exactly the one in-flight correlation was settled")

  -- 1. real termination: a tool.cancel for the open correlation went on the bus.
  local cancel = find_kind(h.bus, "tool.cancel")
  assert_true(cancel ~= nil, "interrupt emits a tool.cancel for the open correlation")
  assert_eq(cancel.id, req_id, "the cancel targets the in-flight correlation id")

  -- 2. synthetic settle routed through run-tool → tool-result → sink as a
  --    failed tool result the model can read.
  assert_eq(#h.provider_out, 1, "the interrupted result reached the sink as one ProviderOut turn")
  local turn = h.provider_out[1]
  assert_eq(#turn.messages, 1, "one tool-role message for the interrupted call")
  assert_eq(turn.messages[1].role, "tool", "the interrupted result is a tool-role turn")
  assert_eq(turn.messages[1].content, "[tool error] interrupted by user",
    "the model sees the interruption as a readable tool error")
  assert_eq(turn.messages[1].error, "interrupted by user", "the raw failure is preserved")

  -- 3. the run stays alive: its actors were never killed.
  assert_eq(h.inv.state_of("rt"), "alive", "run-tool is not killed by the interrupt")
  assert_eq(h.inv.state_of("tr"), "alive", "tool-result is not killed by the interrupt")

  -- 4. the correlation is now closed: a second interrupt settles nothing.
  assert_eq(h.router:interrupt("interrupted by user"), 0,
    "the settled correlation is closed; a re-interrupt is a clean no-op")
end

-- ==================================================================
-- a run with nothing in flight interrupts cleanly (settles 0, no cancel)
-- ==================================================================

do
  local h = harness()
  local settled = h.router:interrupt("interrupted by user")
  assert_eq(settled, 0, "no open correlations → nothing to settle")
  assert_true(find_kind(h.bus, "tool.cancel") == nil, "no tool.cancel when nothing is in flight")
  assert_eq(#h.provider_out, 0, "no tool result routed when nothing was interrupted")
end

-- ==================================================================
-- TERMINATING primitive (cancel_inflight): tool.cancel fires but NO reply is
-- delivered — the actor does NOT re-fire. Contrast the graceful path above,
-- where the settle routes a tool result to the sink (the re-fire). This is what
-- lets the host end a dispatched run FAILED without its llm answering.
-- ==================================================================

do
  local h = harness()

  h.router:fire("rt", "llm", "generic-tool.ToolCalls", {
    calls = { { id = "call-1", name = "bash", args = { command = "sleep 10" } } },
  })
  local invoke = find_kind(h.bus, "tool.invoke")
  assert_true(invoke ~= nil, "run-tool put a tool.invoke on the bus")
  local req_id = invoke.id

  -- Terminate primitive: cancel the in-flight work, deliver NOTHING.
  local cancelled = h.router:cancel_inflight()
  assert_eq(cancelled, 1, "the one in-flight correlation was cancelled")

  -- 1. real termination: a tool.cancel for the open correlation went on the bus.
  local cancel = find_kind(h.bus, "tool.cancel")
  assert_true(cancel ~= nil, "cancel_inflight emits a tool.cancel for the open correlation")
  assert_eq(cancel.id, req_id, "the cancel targets the in-flight correlation id")
  assert_eq(count_kind(h.bus, "tool.cancel"), 1, "exactly one cancel for the one correlation")

  -- 2. NO re-fire: no tool result reached the sink — the llm gets no reply to
  --    absorb and answer on. This is the difference from the graceful interrupt.
  assert_eq(#h.provider_out, 0,
    "cancel_inflight delivers no reply — the actor does not re-fire")

  -- 3. the actors are untouched by the primitive itself (the HOST reaps the run
  --    afterward via end_run — not this call).
  assert_eq(h.inv.state_of("rt"), "alive", "cancel_inflight does not kill actors")
end

print("mag-kernel interrupt_test: all assertions passed")
