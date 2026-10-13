-- Common boundary ordering with injected canonical/read and adapter facets.
local boundary = require("factories.provider-boundary")
local run_tool = require("factories.run-tool")
local tool_result = require("factories.tool-result")
local copy = require("core.json_data").copy
local Async = dofile(package.searchpath("kinds", package.path):match("^(.*)/") .. "/../../../../tests/lua/mag-kernel/async_compaction_fixture.lua")
local thresholds = { soft_percent = 60, step_percent = 10, hard_percent = 90 }
local function eq(a, b) assert(a == b, tostring(a) .. " ~= " .. tostring(b)) end
local function last(out, kind)
  for i = #out, 1, -1 do if out[i].kind == kind then return out[i] end end
end
local function count(out, kind)
  local n = 0; for _, m in ipairs(out) do if m.kind == kind then n = n + 1 end end; return n
end
local function make(policy, tokens, capability, local_specs)
  local out, log, facts = {}, {}, {}
  local view = { context = { history_length = 2 }, watermark = 4, history_id = { "h" },
    policy_state = { epoch_id = "e", notices = {} } }
  if tokens then view.latest_response_usage = { kind = "completed_response", total_tokens = tokens } end
  local operation, appends, binds, acknowledgments = nil, 0, 0, 0
  local sync = {
    barrier = function() return { watermark = view.watermark } end,
    prepare = function() return { kind = "Ready", regular_window = 100,
      capability = { kind = capability or "supported" } } end,
    read = function() log[#log + 1] = "read"; return copy(view) end,
    append_notice = function(args)
      appends = appends + 1; log[#log + 1] = "notice"
      assert(args.text:find("self_compact", 1, true))
      eq(args.expected_watermark, view.watermark)
      local n = { notice_id = "notice" .. appends, epoch_id = view.policy_state.epoch_id,
        provider = "p", model = "m", history_id = { "h" }, milestone_percent = args.notice_data.milestone_percent }
      view.policy_state.notices[n.notice_id] = n; view.policy_state.outstanding_notice = n
      view.watermark = view.watermark + 1
      return { kind = "Accepted", notice_id = n.notice_id, epoch_id = n.epoch_id, watermark = view.watermark }
    end,
    bind_notice = function(args)
      binds = binds + 1; log[#log + 1] = "bind"; eq(args.context_watermark, view.watermark)
      view.policy_state.notified_through = view.policy_state.outstanding_notice.milestone_percent
      return { kind = "Accepted" }
    end,
    acknowledge_notice = function(args)
      acknowledgments = acknowledgments + 1; log[#log + 1] = "ack"
      view.policy_state.delivered_notice_id = args.notice_id; view.policy_state.outstanding_notice = nil
      return { kind = "Accepted" }
    end,
    request_compaction = function(args, handle)
      assert(not operation); log[#log + 1] = "compact"
      args.request_id = handle.id
      operation = copy(args); view.pending_compaction = operation
      return { kind = "Accepted", request_id = args.request_id }
    end,
    cancel_compaction = function(args)
      eq(args.request_id, operation.request_id); log[#log + 1] = "cancel"
      view.pending_compaction = nil; return { kind = "Accepted" }
    end,
  }
  local transport = Async.new(sync)
  local facet = transport.facet
  local actor, err = boundary.construct("a", { tool_approval_policy = { constructor = "Default" },  provider = "p", model = "m", tools = {},
    tool_specs = local_specs, compaction_policy = policy },
    function(m) out[#out + 1] = m; log[#log + 1] = m.kind end, {
      conversation = { id = "c", turn_id = "t", emit = function(f)
        facts[#facts + 1] = f; log[#log + 1] = f.kind
        view.watermark = view.watermark + 1
        if f.kind == "message_completed" then view.context.history_length = view.context.history_length + 1 end
        if f.kind == "response_usage_recorded" then view.latest_response_usage = copy(f.latest_response_usage) end
      end }, compaction = facet, steerable = true,
      on_final = function(state, result) state:append({ role = "assistant", content = result.text or "" }); state:finish({ kind = "answer" }) end,
  })
  assert(actor, err)
  transport.wrap(actor)
  local function start() actor.deliver({ messages = { { message = { text = "go" } } } }) end
  local function reply(result)
    actor.deliver({ kind = "reply", ref = last(out, "capability.invoke").ref, result = result })
  end
  local function include()
    local m = last(out, "capability.invoke")
    actor.handle_observation({ binding = "conversation", value = { kind = "context_included",
      request_id = m.ref, notice_id = m.request.notice_id, epoch_id = m.request.epoch_id,
      context_watermark = view.watermark, history_id = { "h" } } })
  end
  local function commit(tokens_after)
    assert(operation)
    view.pending_compaction = nil; view.policy_state = { epoch_id = operation.request_id, notices = {} }
    view.latest_response_usage = tokens_after and { kind = "completed_response", total_tokens = tokens_after } or nil
    transport.settle(transport.pending_compact, { kind = "Ok", value = {
      status = "compacted", request_id = operation.request_id, epoch_id = operation.request_id } })
    transport.pump("request_compaction")
  end
  return { actor = actor, out = out, log = log, facts = facts, view = view, facet = facet, transport = transport,
    start = start, reply = reply, include = include, commit = commit,
    op = function() return operation end,
    notices = function() return appends, binds, acknowledgments end }
end
local function calls(...)
  local out = {}; for i, name in ipairs({ ... }) do out[i] = { id = tostring(i), name = name, args = {} } end
  return { tool_calls = out }
end
local function continue_batch(t)
  local emitted = last(t.out, "generic-tool.ToolCalls")
  local tools = {}
  local runner = assert(run_tool.construct("tools", { tool_approval_policy = { constructor = "Default" } },
    function(m) tools[#tools + 1] = m end))
  runner.deliver({ messages = { { message = emitted } } })
  for _, m in ipairs(tools) do
    if m.kind == "capability.invoke" then
      assert(m.capability ~= "self_compact")
      t.log[#t.log + 1] = "sibling_done"
      runner.deliver({ kind = "reply", ref = m.ref, result = "ordinary result" })
    end
  end
  local input
  local adapter = assert(tool_result.construct("adapt", {}, function(m)
    if m.kind == "generic-provider.ProviderOut" then input = { messages = m.messages } end
  end))
  adapter.deliver({ messages = { { message = last(tools, "generic-tool.ToolHandle") } } })
  t.actor.deliver({ messages = { { message = input } } })
end
-- Advertisement is unconditional, reserved names reject, hard cannot invoke until commit.
do
  local t = make(thresholds, 90); t.start(); eq(count(t.out, "capability.invoke"), 0)
  eq(t.op().trigger, "hard"); t.commit(); eq(count(t.out, "capability.invoke"), 1)
  local request = last(t.out, "capability.invoke").request
  eq(request.tools[1], "self_compact"); eq(request.tool_specs[1].parameters.additionalProperties, false)
  local actor, err = boundary.construct("bad", { tool_approval_policy = { constructor = "Default" }, compaction_policy = { constructor = "Disabled" },  provider = "p", tools = { "self_compact" } }, function() end, {})
  assert(not actor and err:find("conflicts"))
end
-- Reusing authored params never installs an intrinsic into the source graph.
do
  local params = { tool_approval_policy = { constructor = "Default" }, compaction_policy = { constructor = "Disabled" }, provider = "p", tools = {}, tool_specs = {} }
  for _, id in ipairs({ "lead", "opaque-worker" }) do
    local transport = Async.disabled()
    local actor = assert(boundary.construct(id, params, function() end, {
      compaction = transport.facet,
      conversation = { id = id, emit = function() end },
      on_final = function() end,
    }))
    transport.wrap(actor)
    eq(#params.tools, 0); eq(#params.tool_specs, 0)
  end
  local actor, err = boundary.construct("reserved-spec", { tool_approval_policy = { constructor = "Default" }, compaction_policy = { constructor = "Disabled" },  provider = "p",
    tool_specs = {{ name = "self_compact" }} }, function() end, {})
  assert(not actor and err:find("conflicts"))
end
-- Held siblings preserve exactly the advertised request-local tool names.
do
  local t = make(thresholds, 60, nil, {{ name = "ordinary" }})
  t.start(); t.include(); t.reply(calls("self_compact", "ordinary"))
  local emitted = last(t.out, "generic-tool.ToolCalls")
  eq(table.concat(emitted.request_local_tools, ","), "ordinary,self_compact")
  local tools = {}
  local runner = assert(run_tool.construct("local-tools", { tools = {},
    tool_approval_policy = { constructor = "Default" } }, function(m) tools[#tools + 1] = m end))
  runner.deliver({ messages = {{ message = emitted }} })
  local request = last(tools, "capability.invoke").request
  eq(table.concat(request.request_local_tools, ","), "ordinary,self_compact")
  eq(#request.allowlist, 0)
end
-- Known no progress fails rather than recursively compacting; unknown permits a request.
do
  local t = make(thresholds, 110); t.start(); t.commit(95)
  eq(count(t.out, "capability.invoke"), 0); eq(last(t.out, "mag.failed").value.error, "compaction_no_progress")
end
-- Actual inclusion, not enqueue, grants notice delivery. Missing usage can acknowledge.
do
  local t = make(thresholds, 81); t.start()
  local a, b, c = t.notices(); eq(a, 1); eq(b, 0); eq(c, 0)
  eq(t.view.policy_state.outstanding_notice.milestone_percent, 80)
  t.include(); t.reply(calls("ordinary"))
  a, b, c = t.notices(); eq(a, 1); eq(b, 1); eq(c, 1)
  continue_batch(t); a = t.notices(); eq(a, 1)
end
-- Successful included native rounds deliver before a later operation failure.
do
  local t = make(thresholds, 60); t.start(); t.include()
  t.actor.handle_observation({ binding = "conversation", value = { kind = "usage",
    latest_response_usage = { kind = "completed_response", total_tokens = 70 } } })
  local _, _, c = t.notices(); eq(c, 1)
  eq(t.view.policy_state.delivered_notice_id, "notice1")
  t.reply({ finish_reason = "error", error = "later native round failed" })
  _, _, c = t.notices(); eq(c, 1)
  eq(t.view.policy_state.delivered_notice_id, "notice1")
  eq(count(t.out, "mag.failed"), 1)
end
-- Generic billing, invalid measurement, and missing inclusion cannot grant delivery.
do
  for _, evidence in ipairs({ { usage = { total_tokens = 70 } },
      { latest_response_usage = { kind = "completed_response", total_tokens = -1 } } }) do
    local t = make(thresholds, 60); t.start(); t.include()
    evidence.kind = "usage"
    t.actor.handle_observation({ binding = "conversation", value = evidence })
    t.reply({ finish_reason = "error", error = "failed" })
    local _, _, c = t.notices(); eq(c, 0)
  end
  local t = make(thresholds, 60); t.start()
  t.actor.handle_observation({ binding = "conversation", value = { kind = "usage",
    latest_response_usage = { kind = "completed_response", total_tokens = 70 } } })
  local _, _, c = t.notices(); eq(c, 0)
end
-- Multiple successful rounds and terminal success acknowledge the same inclusion once.
do
  local t = make(thresholds, 60); t.start(); t.include()
  for i = 1, 2 do
    t.actor.handle_observation({ binding = "conversation", value = { kind = "usage",
      latest_response_usage = { kind = "completed_response", total_tokens = 70 + i } } })
  end
  t.reply({ text = "done" }); local _, _, c = t.notices(); eq(c, 1)
end
-- Successful malformed response still delivers notice; exact new usage gates its retry.
do
  local t = make(thresholds, 60); t.start(); t.include()
  t.reply({ usage = { total_tokens = 90 }, tool_calls = { { id = "x", name = "x", args = "{" } } })
  local _, _, c = t.notices(); eq(c, 1)
  eq(t.op().trigger, "hard"); eq(count(t.out, "capability.invoke"), 1)
end
-- A response that did not include a notice cannot manufacture eligibility.
do
  local t = make(thresholds, 60); t.start(); t.reply(calls("self_compact"))
  local emitted = last(t.out, "generic-tool.ToolCalls")
  eq(nefor.json.encode(emitted.value.calls), "[]", "self-only batch emits a marked empty semantic array")
  continue_batch(t)
  assert(not t.op()); local error
  for _, f in ipairs(t.facts) do if f.kind == "tool_error_recorded" then error = f.error end end
  eq(error, "compaction_notification_required")
end
-- Coalesced self + ordinary: ordinary work finishes first, prefix excludes whole assistant batch,
-- no self external slot, canonical commit precedes every self result and next request.
do
  local t = make(thresholds, 60); t.start(); t.include()
  local prefix = t.view.context.history_length
  t.reply(calls("self_compact", "ordinary", "self_compact"))
  local external = last(t.out, "generic-tool.ToolCalls").calls; eq(#external, 1); eq(external[1].name, "ordinary")
  continue_batch(t); eq(t.op().history_cutoff, prefix)
  eq(count(t.out, "capability.invoke"), 1)
  local sibling, compact
  for i, event in ipairs(t.log) do if event == "sibling_done" then sibling = i elseif event == "compact" then compact = i end end
  assert(sibling < compact)
  t.commit(); eq(count(t.out, "capability.invoke"), 2)
  local results = {}; for _, f in ipairs(t.facts) do if f.kind == "tool_result_recorded" then results[#results + 1] = f.result end end
  eq(#results, 3); eq(results[1], "ordinary result")
  eq(nefor.json.decode(results[2]).status, "compacted"); eq(results[2], results[3])
end
-- Unsupported takes precedence even under Disabled.
do
  local t = make(nil, nil, "unsupported"); t.start(); t.reply(calls("self_compact")); continue_batch(t)
  assert(not t.op()); local error
  for _, f in ipairs(t.facts) do if f.kind == "tool_error_recorded" then error = f.error end end
  eq(error, "compaction_unsupported")
end
-- Self + submit rejects all before ordinary effects, even without eligibility.
do
  local t = make(nil); t.start(); t.reply(calls("self_compact", "submit_output", "shell.script"))
  eq(count(t.out, "generic-tool.ToolCalls"), 0); assert(not t.op())
  local n = 0
  for _, f in ipairs(t.facts) do if f.kind == "tool_error_recorded" then eq(f.error, "compaction_terminal_conflict"); n = n + 1 end end
  eq(n, 3)
end
-- No speculative compaction for ended agents, and kill explicitly cancels canonical pending.
do
  local t = make(thresholds); t.start(); t.reply({ text = "done", usage = { total_tokens = 99 } }); assert(not t.op())
  local killed = make(thresholds, 95); killed.start(); killed.actor.handle_kill(); assert(table.concat(killed.log, ","):find("cancel"))
end
-- Canonical failure is terminal, settles self results, and never silently retries.
do
  local t = make(thresholds, 60); t.start(); t.include(); t.reply(calls("self_compact", "self_compact")); continue_batch(t)
  assert(not t.actor.handle_observation({ binding = "compaction", value = {
    kind = "context_compaction_completed", request_id = "wrong" } }))
  t.transport.settle(t.transport.pending_compact, { kind = "Error", error = { code = "native_failed" } })
  t.transport.pump("request_compaction")
  eq(count(t.out, "capability.invoke"), 1); eq(count(t.out, "mag.failed"), 1)
  local n = 0
  for _, f in ipairs(t.facts) do if f.kind == "tool_error_recorded" then eq(f.error, "native_failed"); n = n + 1 end end
  eq(n, 2)
end
-- A terminal notification alone is not evidence of an accepted visible commit.
do
  local t = make(thresholds, 90); t.start()
  t.transport.settle(t.transport.pending_compact, { kind = "Ok", value = { status = "compacted", request_id = t.op().request_id } })
  t.transport.pump("request_compaction")
  eq(count(t.out, "capability.invoke"), 0); eq(last(t.out, "mag.failed").value.error, "compaction_commit_not_visible")
end
-- Individual nonempty self arguments are rejected; valid siblings share one operation.
do
  local t = make(thresholds, 60); t.start(); t.include()
  local result = calls("self_compact", "self_compact"); result.tool_calls[1].args = { prompt = "override" }
  t.reply(result); continue_batch(t); t.commit()
  local bad, good = 0, 0
  for _, f in ipairs(t.facts) do
    if f.kind == "tool_error_recorded" then eq(f.error, "invalid_arguments"); bad = bad + 1 end
    if f.kind == "tool_result_recorded" then good = good + 1 end
  end
  eq(bad, 1); eq(good, 1)
end
-- Waiting preparation does not poll or dispatch; replay-pending needs explicit recovery.
do
  local t = make(thresholds)
  -- A deferred preparation owns its callback until the catalog arrives.
  local preparations = 0
  local delayed, handle
  t.facet.prepare = function(args, done)
    preparations = preparations + 1
    delayed = done; handle = { id = "catalog" }; return handle
  end
  t.start(); eq(preparations, 1); eq(count(t.out, "capability.invoke"), 0)
  delayed({ kind = "Ok", value = { kind = "Ready", regular_window = 100, capability = { kind = "supported" } } })
  t.transport.pump("request_compaction")
  eq(preparations, 1); eq(count(t.out, "capability.invoke"), 1)
  local pending = make(thresholds); pending.view.pending_compaction = { request_id = "replayed" }; pending.start()
  eq(count(pending.out, "capability.invoke"), 0); assert(not pending.op())
  eq(last(pending.out, "mag.failed").value.error, "compaction_recovery_required")
end
-- Milestones advance only following real inclusion; a smaller usage never re-notifies.
do
  local t = make(thresholds, 60); t.start()
  for _, tokens in ipairs({70, 80, 75}) do
    t.include(); local response = calls("ordinary"); response.usage = { total_tokens = tokens }
    t.reply(response); continue_batch(t)
  end
  local n = t.notices(); eq(n, 3)
end
-- Steering's next request goes through hard gating as well.
do
  local t = make(thresholds); t.start(); assert(t.actor.handle_steer({ role = "user", content = "continue" }))
  t.reply({ text = "done", usage = { total_tokens = 95 } })
  eq(count(t.out, "capability.invoke"), 1); eq(t.op().trigger, "hard")
end
dofile(package.searchpath("kinds", package.path):match("^(.*)/") .. "/../../../../tests/lua/mag-kernel/compaction_boundary_async_test.lua")(make, thresholds, calls, eq, count, last)
print("compaction_boundary_test: passed")
