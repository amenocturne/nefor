-- tests/lua/mag-kernel/human_test.lua — the human gate's reply routing,
-- end to end through the kernel (plugins/mag/docs/actor-model.md, The
-- approval boundary).
--
-- Driven from engine/tests/starter_mag_kernel_test.rs: bare Lua VM, stub
-- nefor.log, package.path at plugins/mag/lua/mag-kernel/. The harness wires
-- inventory + registry + router + observer exactly the way init.lua does,
-- INCLUDING the construction probe (set_is_constructed) that guards
-- control-plane reply injection at apply time.
--
-- Pinned here:
--   * request out — a subject firing the gate surfaces the run's
--     `mag.approval_request` control-plane event (intercepted emit, never a
--     routed/persisted node output);
--   * reply in — the control plane injects `mag.ApprovalReply` as a
--     modification message; the kernel delivers it past declared ports to the
--     constructed gate, and the gate's typed exit routes the graph onward;
--   * the full gate-template flow — produce → approve → reject → rework
--     (adapter lifts the reason) → produce re-fires → approve → approve →
--     sink completes the run;
--   * reply-before-construction REJECTS the modification (loud, at apply —
--     a reply answers an outstanding request; nothing is parked or lost);
--   * reply at a dead gate stays a race-artifact drop;
--   * drain surfaces `mag.approval_cancel`;
--   * apply-time validation accepts the gate template's wiring against the
--     real shipped factories (llm / human / adapter / sink).

local inventory = require("inventory")
local Registry = require("registry")
local routing = require("routing")
local observer = require("observer")
local human = require("factories.human")
local adapter = require("factories.adapter")
local sink = require("factories.sink")
local llm = require("factories.llm")

-- ------------------------------------------------------------------
-- assert helpers
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

local function assert_contains(haystack, needle, msg)
  if type(haystack) ~= "string" or not haystack:find(needle, 1, true) then
    error(string.format(
      "assertion failed: %s\n  expected to contain: %s\n  actual: %s",
      msg or "substring missing", tostring(needle), tostring(haystack)), 2)
  end
end

local function new_logger()
  local rec = { info = {}, warn = {}, error = {} }
  local function sink_at(bucket)
    return function(m) bucket[#bucket + 1] = m end
  end
  return { info = sink_at(rec.info), warn = sink_at(rec.warn), error = sink_at(rec.error) }, rec
end

-- ------------------------------------------------------------------
-- harness — wired the way init.lua wires a run context: registry-backed
-- validation, lazy construction, deps.writer persistence, the kill hook,
-- and the construction probe for reply injection.
-- ------------------------------------------------------------------

-- A deterministic stand-in for the produce llm: same declared boundary
-- (ProviderOut in, FinalAnswer out) but synchronous — each firing emits
-- "draft <n>" instead of a provider round-trip.
local function producer_factory()
  return {
    declaration = {
      name = "producer",
      params = {},
      inputs = { provider_out = "generic-provider.ProviderOut" },
      outputs = { "generic-provider.FinalAnswer" },
      semantic = {
        input={kind="named",name="nefor.contracts.ProviderInput",arguments={}},
        output={kind="named",name="nefor.contracts.FinalAnswer",arguments={}},
        inputs={{wire="generic-provider.ProviderOut",type={kind="named",name="nefor.contracts.ProviderInput",arguments={}}}},
        outputs={{wire="generic-provider.FinalAnswer",type={kind="named",name="nefor.contracts.FinalAnswer",arguments={}}}},
      },
    },
    construct = function(id, params, emit, deps)
      local n = 0
      local inst = { id = id }
      function inst.deliver(activation)
        n = n + 1
        emit({ kind = "generic-provider.FinalAnswer", from = id, text = "draft " .. n })
        return { status = "ok" }
      end
      emit({ kind = "mag.ready", from = id })
      return inst
    end,
  }
end

local function harness()
  local log, rec = new_logger()
  local events = {}
  local bus = {}
  local persisted = {} -- { { id = <sender>, output = <message> }, ... }
  local reg = Registry.new()
  for _, f in ipairs({
    { declaration = human.declaration, construct = human.construct },
    { declaration = adapter.declaration, construct = adapter.construct },
    { declaration = sink.declaration, construct = sink.construct },
    producer_factory(),
  }) do
    local _, err = reg:register(f)
    assert_true(err == nil, "factory registers: " .. tostring(err))
  end

  local inv = inventory.new({ log = log, registry = reg })
  local router = routing.new({
    inventory = inv,
    registry = reg,
    log = log,
    bus_emit = function(e) bus[#bus + 1] = e end,
    events = function(e) events[#events + 1] = e end,
    persist_output = function(id, output)
      persisted[#persisted + 1] = { id = id, output = output }
    end,
  })
  inv.set_on_kill(function(id)
    router:dispatch_kill(id)
    router:forget(id)
  end)
  inv.set_is_constructed(function(id)
    return router:is_constructed(id)
  end)
  router:set_construct(function(record)
    return reg:construct(record.factory, record.id, record.params,
      router:emitter(record.id), { writer = function() end })
  end)
  inv.set_deliver(function(to, from, content)
    content = content or {}
    router:deliver(to, from, content.kind, content)
  end)
  local obs = observer.new({ inventory = inv, emit_event = function(e) events[#events + 1] = e end })
  return {
    inv = inv, reg = reg, router = router, obs = obs,
    log = rec, events = events, bus = bus, persisted = persisted,
  }
end

local function events_of_kind(h, kind)
  local out = {}
  for _, e in ipairs(h.events) do
    if e.kind == kind then out[#out + 1] = e end
  end
  return out
end

-- The gate constellation used by the flow tests: the gate template's lowered
-- shape (produce → approve → rework → produce; approve → sink) with the
-- synchronous producer standing in for the llm.
local function gate_actors()
  return {
    {
      id = "produce", factory = "producer", params = {},
      evidence={version=2,identity="nefor.factory.producer",arguments={},input={kind="named",name="nefor.contracts.ProviderInput",arguments={}},output={kind="named",name="nefor.contracts.FinalAnswer",arguments={}}},
      input={type={kind="named",name="nefor.contracts.ProviderInput",arguments={}},wire="generic-provider.ProviderOut"},outputs={{type={kind="named",name="nefor.contracts.FinalAnswer",arguments={}},wire="generic-provider.FinalAnswer"}},
      routes = { ["generic-provider.FinalAnswer"] = { "approve" } },
    },
    {
      id = "approve", factory = "human", params = { prompt = "Approve the draft?" },
      evidence={version=2,identity="nefor.factory.human",arguments={},input={kind="named",name="nefor.contracts.FinalAnswer",arguments={}},output={kind="union",items={{kind="named",name="nefor.contracts.Approved",arguments={}},{kind="named",name="nefor.contracts.Rejected",arguments={}}}}},
      input={type={kind="named",name="nefor.contracts.FinalAnswer",arguments={}},wire="generic-provider.FinalAnswer"},outputs={{type={kind="named",name="test.Approved",arguments={}},wire="human.Approved"},{type={kind="named",name="test.Rejected",arguments={}},wire="human.Rejected"}},
      routes = {
        ["human.Approved"] = { "out" },
        ["human.Rejected"] = { "rework" },
      },
    },
    {
      id = "rework", factory = "adapter", params = { seed = "provider-in" },
      evidence={version=2,identity="nefor.factory.adapter",arguments={{kind="named",name="test.Rejected",arguments={}}},input={kind="named",name="test.Rejected",arguments={}},output={kind="named",name="nefor.contracts.ProviderInput",arguments={}}},
      input={type={kind="named",name="test.Rejected",arguments={}},wire="human.Rejected"},outputs={{type={kind="named",name="nefor.contracts.ProviderInput",arguments={}},wire="generic-provider.ProviderOut"}},
      routes = { ["generic-provider.ProviderOut"] = { "produce" } },
    },
    { id = "out", factory = "sink", params = {}, routes = {},
      evidence={version=2,identity="nefor.factory.sink",arguments={{kind="named",name="test.Approved",arguments={}}},input={kind="named",name="test.Approved",arguments={}},output={kind="primitive",name="Unit"}},
      input={type={kind="named",name="test.Approved",arguments={}},wire="human.Approved"},outputs={{type={kind="primitive",name="Unit"},wire="mag.Unit"}} },
  }
end

local function seed_message()
  return {
    to = "produce",
    content = {
      kind = "generic-provider.ProviderOut",
      messages = { { role = "user", content = "write the plan" } },
    },
  }
end

local function reply_message(to, fields)
  local content = { kind = "mag.ApprovalReply" }
  for k, v in pairs(fields) do content[k] = v end
  return { to = to, content = content }
end

-- ==================================================================
-- (1) full gate flow: subject → approval request event; reject → rework →
-- produce re-fires → second request; approve → sink completes the run
-- ==================================================================

do
  local h = harness()
  local result = h.obs:apply({ actors = gate_actors(), messages = { seed_message() } })
  assert_eq(result.ok, true, "the gate constellation applies: " .. tostring(result.error))

  -- Draft 1 reached the gate: the request surfaced as the control-plane event.
  local requests = events_of_kind(h, "mag.approval_request")
  assert_eq(#requests, 1, "the subject raised one approval request")
  assert_eq(requests[1].from, "approve", "the request names the gate actor")
  assert_eq(requests[1].correlation, "approve", "the request carries the correlation handle")
  assert_eq(requests[1].prompt, "Approve the draft?", "the request carries the configured prompt")
  assert_eq(requests[1].subject.text, "draft 1", "the request carries the subject")
  assert_eq(#events_of_kind(h, "mag.run_complete"), 0, "the run waits on the human")

  -- The human rejects: the control plane injects the reply as a modification
  -- message. The gate resolves to human.Rejected → rework lifts the reason →
  -- produce re-fires → a second request with draft 2.
  local rejected = h.obs:apply({
    messages = { reply_message("approve", { approved = false, reason = "tighten the wording" }) },
  })
  assert_eq(rejected.ok, true, "the rejecting reply applies: " .. tostring(rejected.error))
  requests = events_of_kind(h, "mag.approval_request")
  assert_eq(#requests, 2, "the revise loop raised a second approval request")
  assert_eq(requests[2].subject.text, "draft 2", "the second request carries the revised draft")
  assert_eq(#events_of_kind(h, "mag.run_complete"), 0, "still waiting on the human")
  assert_eq(#events_of_kind(h, "mag.run_failed"), 0, "no failure escalated in the revise loop")

  -- The rework adapter lifted exactly the reason into the next provider turn.
  local lifted
  for _, p in ipairs(h.persisted) do
    if p.id == "rework" then lifted = p.output end
  end
  assert_true(lifted ~= nil, "the rework adapter emitted a routed provider turn")
  assert_eq(lifted.messages[1].content, "tighten the wording",
    "the rejection reason is the next turn's content")

  -- The human approves: the gate exits human.Approved into the sink.
  local approved = h.obs:apply({
    messages = { reply_message("approve", { approved = true, content = "ship it" }) },
  })
  assert_eq(approved.ok, true, "the approving reply applies: " .. tostring(approved.error))
  local complete = events_of_kind(h, "mag.run_complete")
  assert_eq(#complete, 1, "the approval completed the run")
  assert_eq(complete[1].result.content, "ship it", "the result carries the human's content")
  assert_eq(complete[1].result.subject.text, "draft 2", "the result carries the approved subject")

  -- Activity honesty: strict busy/idle alternation for the gate. The reject
  -- cascade re-fires the gate (rework → produce → draft 2) while its first
  -- window is still open — the reply's typed exit routes BEFORE its
  -- mag.complete ack — so the overlap extends the one open window instead of
  -- nesting a second busy (routing.lua, mark_busy): one window, one settle.
  local busy, idle = 0, 0
  for _, e in ipairs(h.events) do
    if e.kind == "mag.actor_busy" and e.id == "approve" then busy = busy + 1 end
    if e.kind == "mag.actor_idle" and e.id == "approve" then idle = idle + 1 end
  end
  assert_eq(busy, 1, "the overlapping revise loop extends one busy window (no nested busy)")
  assert_eq(idle, 1, "the window settles once, when a reply's completion resolves the gate")

  -- The approval request is control-plane traffic, never a node output: the
  -- gate's persisted outputs are exactly its typed exits.
  for _, p in ipairs(h.persisted) do
    if p.id == "approve" then
      assert_true(p.output.kind == "human.Approved" or p.output.kind == "human.Rejected",
        "the gate persists only its typed exits, not the request; got " .. tostring(p.output.kind))
    end
  end
end

-- ==================================================================
-- (2) reply-before-construction REJECTS the modification at apply — a reply
-- answers an outstanding request, and a request implies a constructed gate
-- ==================================================================

do
  local h = harness()
  -- The gate registers but never fires (no seed): lazy construction leaves it
  -- unconstructed.
  assert_eq(h.obs:apply({ actors = gate_actors() }).ok, true, "the constellation registers")
  assert_eq(h.router:is_constructed("approve"), false, "the gate never constructed")

  local result = h.obs:apply({
    messages = { reply_message("approve", { approved = true, content = "premature" }) },
  })
  assert_eq(result.ok, false, "a reply at an unconstructed gate rejects the modification")
  assert_contains(result.error, "no outstanding approval request", "the error names the contract")
  assert_contains(result.error, "approve", "the error names the target")

  local rejected = events_of_kind(h, "mag.modification_rejected")
  assert_eq(#rejected, 1, "the rejection surfaced as mag.modification_rejected")
  assert_eq(h.router:is_constructed("approve"), false,
    "the rejected reply constructed nothing (no false 'began work')")
  assert_eq(#events_of_kind(h, "mag.run_failed"), 0, "a rejected injection never fails the run")
end

-- ==================================================================
-- (3) spawn + reply in ONE modification is the same protocol error — the
-- reply structurally precedes any possible request
-- ==================================================================

do
  local h = harness()
  local result = h.obs:apply({
    actors = gate_actors(),
    messages = { reply_message("approve", { approved = true }) },
  })
  assert_eq(result.ok, false, "a same-modification spawn+reply rejects")
  assert_contains(result.error, "no outstanding approval request", "the error names the contract")
  assert_eq(h.inv.state_of("approve"), "never-existed", "the rejected modification spawned nothing")
end

-- ==================================================================
-- (4) a reply at a DEAD gate is a race artifact: passes validation, drops at
-- delivery as a logged no-op, never escalates
-- ==================================================================

do
  local h = harness()
  assert_eq(h.obs:apply({ actors = gate_actors(), messages = { seed_message() } }).ok, true,
    "the constellation applies and the gate constructs")
  assert_eq(h.obs:apply({ kills = { "approve" } }).ok, true, "the kill applies")

  local result = h.obs:apply({
    messages = { reply_message("approve", { approved = true, content = "late" }) },
  })
  assert_eq(result.ok, true, "a reply at a dead gate passes validation (settled race semantics)")
  local logged = false
  for _, line in ipairs(h.log.info) do
    if line:find("dead", 1, true) then logged = true end
  end
  assert_true(logged, "the dead-target drop is logged")
  assert_eq(#events_of_kind(h, "mag.run_failed"), 0, "the race artifact never escalates")
end

-- ==================================================================
-- (5) drain retracts an outstanding request via the run_id-stamped
-- mag.approval_cancel control-plane event
-- ==================================================================

do
  local h = harness()
  assert_eq(h.obs:apply({ actors = gate_actors(), messages = { seed_message() } }).ok, true,
    "the constellation applies")
  assert_eq(#events_of_kind(h, "mag.approval_request"), 1, "a request is outstanding")

  assert_eq(h.router:drain("approve"), true, "the gate's drain handler ran")
  local cancels = events_of_kind(h, "mag.approval_cancel")
  assert_eq(#cancels, 1, "drain surfaced the cancel as a control-plane event")
  assert_eq(cancels[1].from, "approve", "the cancel names the gate actor")
  assert_eq(cancels[1].correlation, "approve", "the cancel carries the correlation handle")
end

-- ==================================================================
-- (6) apply-time validation accepts the gate template's lowered wiring
-- against the REAL shipped factories (llm / human / adapter / sink)
-- ==================================================================

do
  local log = new_logger()
  local reg = Registry.new()
  for _, mod in ipairs({ llm, human, adapter, sink }) do
    local _, err = reg:register({ declaration = mod.declaration, construct = mod.construct })
    assert_true(err == nil, "shipped factory registers: " .. tostring(err))
  end
  local inv = inventory.new({ log = log, registry = reg })

  -- The lowered gate program (crates/nefor-mag/tests/templates.rs): an entry
  -- adapter, the namespaced review.* constellation, the sink terminal.
  local result = inv.apply({
    actors = {
      {
        id = "entry", factory = "adapter", params = { seed = "provider-in" },
        evidence={version=2,identity="nefor.factory.adapter",arguments={{kind="named",name="test.Task",arguments={}}},input={kind="named",name="test.Task",arguments={}},output={kind="named",name="nefor.contracts.ProviderInput",arguments={}}},
        input={type={kind="named",name="test.Task",arguments={}},wire="task"},outputs={{type={kind="named",name="nefor.contracts.ProviderInput",arguments={}},wire="generic-provider.ProviderOut"}},
        routes = { ["generic-provider.ProviderOut"] = { "review.produce" } },
      },
      {
        id = "review.produce", factory = "llm",
        params = { provider = "chatgpt-provider", model = "opus" },
        evidence={version=2,identity="nefor.factory.llm",arguments={},input={kind="named",name="nefor.contracts.ProviderInput",arguments={}},output={kind="union",items={{kind="named",name="nefor.contracts.ToolCalls",arguments={}},{kind="named",name="nefor.contracts.FinalAnswer",arguments={}}}}},
        input={type={kind="named",name="nefor.contracts.ProviderInput",arguments={}},wire="generic-provider.ProviderOut"},outputs={{type={kind="named",name="nefor.contracts.ToolCalls",arguments={}},wire="generic-tool.ToolCalls"},{type={kind="named",name="nefor.contracts.FinalAnswer",arguments={}},wire="generic-provider.FinalAnswer"}},
        routes = { ["generic-provider.FinalAnswer"] = { "review.approve" } },
      },
      {
        id = "review.approve", factory = "human", params = { prompt = "Approve this result?" },
        evidence={version=2,identity="nefor.factory.human",arguments={},input={kind="named",name="nefor.contracts.FinalAnswer",arguments={}},output={kind="union",items={{kind="named",name="nefor.contracts.Approved",arguments={}},{kind="named",name="nefor.contracts.Rejected",arguments={}}}}},
        input={type={kind="named",name="nefor.contracts.FinalAnswer",arguments={}},wire="generic-provider.FinalAnswer"},outputs={{type={kind="named",name="test.Approved",arguments={}},wire="human.Approved"},{type={kind="named",name="test.Rejected",arguments={}},wire="human.Rejected"}},
        routes = {
          ["human.Approved"] = { "sink" },
          ["human.Rejected"] = { "review.rework" },
        },
      },
      {
        id = "review.rework", factory = "adapter", params = { seed = "provider-in" },
        evidence={version=2,identity="nefor.factory.adapter",arguments={{kind="named",name="test.Rejected",arguments={}}},input={kind="named",name="test.Rejected",arguments={}},output={kind="named",name="nefor.contracts.ProviderInput",arguments={}}},
        input={type={kind="named",name="test.Rejected",arguments={}},wire="human.Rejected"},outputs={{type={kind="named",name="nefor.contracts.ProviderInput",arguments={}},wire="generic-provider.ProviderOut"}},
        routes = { ["generic-provider.ProviderOut"] = { "review.produce" } },
      },
      { id = "sink", factory = "sink", params = {}, routes = {},
        evidence={version=2,identity="nefor.factory.sink",arguments={{kind="named",name="test.Approved",arguments={}}},input={kind="named",name="test.Approved",arguments={}},output={kind="primitive",name="Unit"}},
        input={type={kind="named",name="test.Approved",arguments={}},wire="human.Approved"},outputs={{type={kind="primitive",name="Unit"},wire="mag.Unit"}} },
    },
  })
  assert_eq(result.ok, true,
    "the gate template's wiring validates against the shipped factory contracts: "
    .. tostring(result.error))
end

print("mag-kernel human_test: all assertions passed")
