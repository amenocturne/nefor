-- Run with the semantic fixture from compaction_boundary_test. The raw actor
-- bypasses its convenience pump, exposing actual suspension points.
return function(make, thresholds, calls, eq, count, last)
  local function start(t)
    t.transport.raw.deliver({ messages = {{ message = { text = "go" } }} })
  end
  local function include(t)
    local m = last(t.out, "capability.invoke")
    t.transport.raw.handle_observation({ binding = "conversation", id = "native-request",
      value = { kind = "context_included", request_id = "native-request",
        notice_id = m.request.notice_id, epoch_id = m.request.epoch_id,
        context_watermark = t.view.watermark, history_id = { "h" } } })
  end
  local function reply(t, result)
    t.transport.raw.deliver({ kind = "reply", ref = last(t.out, "capability.invoke").ref, result = result })
  end

  -- Enqueue is not acceptance: initial writes, preparation, append and reread
  -- can each be delayed independently without invoking the provider.
  for _, tokens in ipairs({ 60, 81 }) do
    local t = make(thresholds, tokens)
    start(t); eq(count(t.out, "capability.invoke"), 0)
    eq(t.transport.queue[1].operation, "barrier")
    t.transport.pump("prepare"); eq(count(t.out, "capability.invoke"), 0)
    t.transport.pump("append_notice"); eq(count(t.out, "capability.invoke"), 0)
    eq(t.notices(), 0)
    t.transport.pump("read")
    eq(t.notices(), 1); eq(count(t.out, "capability.invoke"), 0)
    t.transport.pump(); eq(count(t.out, "capability.invoke"), 1)
    eq(last(t.out, "capability.invoke").request.context_watermark, t.view.watermark)
  end

  -- Terminal tool results are data until bind AND delivery have been accepted.
  -- A native success can arrive before bind, without occupancy measurement.
  do
    local t = make(thresholds, 60); t.start(); include(t)
    local bind = t.transport.queue[1]
    t.transport.raw.handle_observation({ binding = "conversation", id = "native-request",
      value = { kind = "completion_event", event = "response_completed", request_id = "native-request" } })
    reply(t, calls("self_compact", "ordinary", "self_compact"))
    eq(count(t.out, "generic-tool.ToolCalls"), 0)
    t.transport.pump("acknowledge_notice")
    eq(count(t.out, "generic-tool.ToolCalls"), 0)
    local ack = t.transport.queue[1]
    bind.done({ kind = "Ok", value = { kind = "Accepted" } }) -- duplicate callback cannot resume
    eq(t.transport.queue[1], ack)
    t.transport.pump(); eq(count(t.out, "generic-tool.ToolCalls"), 1)
    eq(#last(t.out, "generic-tool.ToolCalls").calls, 1)
    local prefix = t.view.context.history_length - 1
    t.transport.raw.deliver({ messages = {{ message = { messages = {
      { role = "tool", tool_call_id = "2", name = "ordinary", content = "sibling result" },
    } } }} })
    assert(not t.op()); eq(t.transport.queue[1].operation, "barrier")
    t.transport.pump("read"); assert(not t.op())
    t.transport.pump("request_compaction"); assert(t.op())
    eq(t.op().history_cutoff, prefix)
    eq(count(t.out, "capability.invoke"), 1)
    assert(not t.transport.raw.handle_observation({ binding = "compaction", value = {
      kind = "context_compaction_accepted", request_id = t.op().request_id } }))
    eq(count(t.out, "capability.invoke"), 1)
    t.view.pending_compaction = nil
    t.view.policy_state = { epoch_id = t.op().request_id, notices = {} }
    t.view.latest_response_usage = nil
    t.transport.settle(t.transport.pending_compact, { kind = "Ok", value = {
      status = "compacted", request_id = t.op().request_id, epoch_id = t.op().request_id } })
    t.transport.pump("read"); eq(count(t.out, "capability.invoke"), 1)
    local self_results = 0
    for _, fact in ipairs(t.facts) do
      if fact.kind == "tool_result_recorded" and fact.result ~= "sibling result" then self_results = self_results + 1 end
    end
    eq(self_results, 0)
    t.transport.pump(); eq(count(t.out, "capability.invoke"), 2)
    t.transport.pending_compact.done({ kind = "Ok", value = { status = "compacted" } })
    eq(count(t.out, "capability.invoke"), 2)
  end

  -- Native success survives later enclosing failure. Neither success telemetry
  -- nor bind enqueue alone writes delivered eligibility.
  do
    local t = make(thresholds, 60); t.start(); include(t)
    t.transport.raw.handle_observation({ binding = "conversation", id = "native-request",
      value = { kind = "response_completed", request_id = "native-request" } })
    reply(t, { finish_reason = "error", error = "later round failed" })
    eq(count(t.out, "mag.failed"), 0)
    t.transport.pump("acknowledge_notice")
    assert(t.view.policy_state.delivered_notice_id == nil)
    t.transport.pump()
    eq(t.view.policy_state.delivered_notice_id, "notice1")
    eq(count(t.out, "mag.failed"), 1)
  end

  for _, operation in ipairs({ "bind_notice", "acknowledge_notice" }) do
    local t = make(thresholds, 60); t.start(); include(t)
    reply(t, calls("self_compact"))
    t.transport.pump(operation)
    local item = table.remove(t.transport.queue, 1)
    eq(item.operation, operation)
    t.transport.settle(item, { kind = "Error", error = "stale_notice" })
    item.done({ kind = "Error", error = "duplicate" })
    eq(count(t.out, "mag.failed"), 1)
    eq(count(t.out, "generic-tool.ToolCalls"), 0)
    assert(t.view.policy_state.delivered_notice_id == nil)
  end

  -- Cancellation invalidates even callbacks already queued for the pump.
  do
    local t = make(thresholds); start(t)
    local stale = table.remove(t.transport.queue, 1)
    t.transport.raw.handle_kill()
    stale.done({ kind = "Ok", value = { watermark = 0 } })
    t.transport.pump(); eq(count(t.out, "capability.invoke"), 0)
    eq(#t.transport.queue, 0)
  end
  do
    local t = make(thresholds, 95); t.start()
    local stale = t.transport.pending_compact
    t.actor.handle_kill()
    stale.done({ kind = "Ok", value = { status = "compacted", request_id = stale.handle.id } })
    t.transport.pump(); eq(count(t.out, "capability.invoke"), 0)
  end

  -- An earlier canonical write rejection fails the boundary, even though
  -- other captured writes could have succeeded later.
  do
    local t = make(thresholds); start(t)
    local barrier = table.remove(t.transport.queue, 1)
    t.transport.settle(barrier, { kind = "Error", error = "prior_write_rejected" })
    t.transport.pump()
    eq(count(t.out, "capability.invoke"), 0)
    eq(count(t.out, "mag.failed"), 1)
  end

  -- Drain while a provider round is live keeps its delivery chain alive;
  -- it can finish the round but must not start another provider request.
  do
    local t = make(thresholds, 60); t.start(); include(t)
    t.transport.raw.handle_drain()
    reply(t, { text = "done" })
    t.transport.pump()
    eq(t.view.policy_state.delivered_notice_id, "notice1")
    eq(count(t.out, "capability.invoke"), 1)
    eq(count(t.out, "answer"), 1)
  end

  -- Kill a self/sibling batch while its tail barrier is pending. Already
  -- recorded sibling tails stay once-only; held self calls receive errors.
  do
    local t = make(thresholds, 60); t.start(); t.include()
    t.reply(calls("self_compact", "ordinary", "self_compact"))
    t.transport.raw.deliver({ messages = {{ message = { messages = {
      { role = "tool", tool_call_id = "2", name = "ordinary", content = "sibling result" },
    } } }} })
    local stale = table.remove(t.transport.queue, 1)
    t.transport.raw.handle_kill()
    stale.done({ kind = "Ok", value = { watermark = t.view.watermark } })
    t.transport.pump(); assert(not t.op())
    local results, errors = 0, 0
    for _, fact in ipairs(t.facts) do
      if fact.kind == "tool_result_recorded" then results = results + 1 end
      if fact.kind == "tool_error_recorded" then errors = errors + 1 end
    end
    eq(results, 1); eq(errors, 2)
    eq(count(t.out, "capability.invoke"), 1)
  end

  -- Genuine supported capability under Disabled is not fabricated unsupported.
  do
    local t = make(nil, nil, "supported"); t.start(); t.reply(calls("self_compact"))
    t.actor.deliver({ messages = {{ message = { messages = {} } }} })
    assert(not t.op())
    local found = false
    for _, fact in ipairs(t.facts) do
      if fact.kind == "tool_error_recorded" then eq(fact.error, "compaction_notification_required"); found = true end
    end
    assert(found)
  end
  print("compaction_boundary_async_test: passed")
end
