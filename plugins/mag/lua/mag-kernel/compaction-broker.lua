-- Cross-process command ownership only. The manager remains the sole memory owner.
local M = {}
local commands = {
  prepare = "conversation.compaction.prepare.request",
  read = "conversation.context.request",
  append_notice = "conversation.context.notice.append",
  bind_notice = "conversation.context.notice.bind",
  acknowledge_notice = "conversation.context.notice.acknowledge",
  request_compaction = "conversation.context.compact.request",
  cancel_compaction = "conversation.context.compact.cancel",
}
local function copy(t)
  local out = {}; for k, v in pairs(t or {}) do out[k] = v end; return out
end
local function ok(value) return { kind = "Ok", value = value } end
local function failure(code, context)
  return { kind = "Error", error = { code = code, context = context } }
end
local function equal(a, b)
  if type(a) ~= type(b) then return false end
  if type(a) ~= "table" then return a == b end
  for k, v in pairs(a) do if not equal(v, b[k]) then return false end end
  for k in pairs(b) do if a[k] == nil then return false end end
  return true
end
function M.new(opts)
  local pending, writes, queue, watermarks, cancellations = {}, {}, {}, {}, {}
  local serial = 0
  local broker = {}
  local function settle(id, outcome)
    local p = pending[id]
    if not p then return end
    pending[id] = nil
    if p.op == "cancel_compaction" and cancellations[p.args.request_id] == id then
      cancellations[p.args.request_id] = nil
    end
    queue[#queue + 1] = { id = id, owner = p.owner, done = p.done, outcome = outcome }
  end
  local function check_barriers()
    local ready = {}
    for id, p in pairs(pending) do
      if p.op == "barrier" then
        local waiting, rejected = false, nil
        for event_id in pairs(p.captured) do
          local w = writes[event_id]
          if w.status == "pending" then waiting = true end
          if w.status == "rejected" then rejected = w.error end
        end
        if not waiting then
          if rejected then ready[id] = failure(rejected.code, rejected.context)
          else ready[id] = ok({ watermark = watermarks[p.args.conversation_id] or 0 }) end
        end
      end
    end
    for id, value in pairs(ready) do settle(id, value) end
    local needed = {}
    for _, p in pairs(pending) do
      for event_id in pairs(p.captured or {}) do needed[event_id] = true end
    end
    for event_id, w in pairs(writes) do
      if w.status == "accepted" and not needed[event_id] then writes[event_id] = nil end
    end
  end
  function broker.emit(fact)
    assert(type(fact.event_id) == "string", "canonical write needs event_id")
    writes[fact.event_id] = writes[fact.event_id] or { conversation_id = fact.conversation_id, status = "pending" }
    opts.emit({ kind = "conversation.fact.append", fact = fact })
  end
  function broker.facet(owner)
    local facet = {}
    local function register(op, args, done)
      assert(type(done) == "function", "compaction command needs continuation")
      serial = serial + 1
      local id = opts.scope .. "/command-" .. serial
      local input = copy(args)
      -- The native operation ID remains on the wire; the cancellation handle
      -- has a fresh identity so detaching it cannot affect a queued generation.
      if op == "cancel_compaction" then
        local request_id = input.request_id
        pending[request_id] = nil
        for _, entry in ipairs(queue) do
          if entry.id == request_id then entry.done = nil end
        end
        local previous = cancellations[request_id]
        if previous then settle(previous, failure("compaction_cancel_superseded")) end
        cancellations[request_id] = id
      end
      pending[id] = { op = op, args = input, done = done, owner = owner }
      if op == "barrier" then
        local captured = {}
        for event_id, w in pairs(writes) do
          if w.conversation_id == input.conversation_id then captured[event_id] = true end
        end
        pending[id].captured = captured
        check_barriers()
      else
        input.kind = commands[op]
        if op == "append_notice" or op == "bind_notice" or op == "acknowledge_notice" then input.command_id = id
        elseif op ~= "cancel_compaction" then input.request_id = id end
        opts.emit(input)
      end
      return { id = id }
    end
    for op in pairs(commands) do facet[op] = function(args, done) return register(op, args, done) end end
    facet.barrier = function(args, done) return register("barrier", args, done) end
    facet.detach = function(handle)
      if not handle then return end
      local p = pending[handle.id]
      if p and p.op == "cancel_compaction" and cancellations[p.args.request_id] == handle.id then
        cancellations[p.args.request_id] = nil
      end
      pending[handle.id] = nil
      for _, entry in ipairs(queue) do if entry.id == handle.id then entry.done = nil end end
    end
    return facet
  end
  function broker.detach_owner(owner)
    for id, p in pairs(pending) do
      if p.owner == owner then
        pending[id] = nil
        if p.op == "cancel_compaction" and cancellations[p.args.request_id] == id then
          cancellations[p.args.request_id] = nil
        end
        if p.op == "request_compaction" then
          opts.emit({ kind = "conversation.context.compact.cancel", request_id = id,
            conversation_id = p.args.conversation_id, reason = "Consumer detached" })
        end
      end
    end
    for _, entry in ipairs(queue) do if entry.owner == owner then entry.done = nil end end
  end
  function broker.interrupt(reason)
    for _, entry in ipairs(queue) do entry.outcome = failure("compaction_interrupted", { reason = reason }) end
    local ids = {}
    for id in pairs(pending) do ids[#ids + 1] = id end
    for _, id in ipairs(ids) do
      local p = pending[id]
      if p and p.op == "request_compaction" then
        opts.emit({ kind = "conversation.context.compact.cancel", request_id = id,
          conversation_id = p.args.conversation_id, reason = reason })
      end
      if p then settle(id, failure("compaction_interrupted", { reason = reason })) end
    end
  end
  function broker.pump()
    while #queue > 0 do
      local entry = table.remove(queue, 1)
      if entry.done and opts.live(entry.owner) then
        local completion = entry.done(entry.outcome)
        opts.complete(entry.owner, completion)
      end
    end
  end
  function broker.event(source, body)
    if body.replay then return false end
    local kind = body.kind
    if source ~= "conversation-manager" and kind ~= "conversation.compaction.prepared" then return false end
    local event = body.event
    if kind == "conversation.fact.recorded" or kind == "conversation.fact.rejected" then
      local event_id = event and event.event_id or body.event_id
      local w = writes[event_id]
      if w and w.status == "pending" and w.conversation_id == (event and event.conversation_id or body.conversation_id) then
        if kind == "conversation.fact.recorded" then
          w.status = "accepted"
          watermarks[w.conversation_id] = math.max(watermarks[w.conversation_id] or 0, event.sequence or 0)
        else w.status = "rejected"; w.error = body end
        check_barriers()
      end
      if kind == "conversation.fact.rejected" then return w ~= nil end
      if not event or (event.kind ~= "context_compaction_completed" and event.kind ~= "context_compaction_failed") then return w ~= nil end
      local p = pending[event.request_id]
      if not p or (p.op ~= "request_compaction" and p.op ~= "cancel_compaction") then return false end
      for _, field in ipairs({ "conversation_id", "epoch_id", "history_id", "provider", "model" }) do
        if p.args[field] ~= nil and not equal(p.args[field], event[field]) then return false end
      end
      if p.op == "cancel_compaction" then return true end -- explicit cancelled receipt settles cancellation
      if event.kind == "context_compaction_completed" then
        settle(event.request_id, ok({ status = "compacted", request_id = event.request_id,
          epoch_id = event.request_id, sequence = event.sequence }))
      else settle(event.request_id, { kind = "Error", error = event.error or { code = "compaction_failed" } }) end
      return true
    end
    local id = body.command_id or body.request_id
    if kind == "conversation.context.compact.cancelled" or kind == "conversation.context.compact.rejected" then
      id = cancellations[body.request_id] or id
    end
    local p = pending[id]
    if not p then return false end
    if body.conversation_id and body.conversation_id ~= p.args.conversation_id then return false end
    if body.command_id and p.args.request_id and body.request_id ~= p.args.request_id then return false end
    if kind == "conversation.context.snapshot" and body.conversation_id ~= p.args.conversation_id then return false end
    if kind == "conversation.context.notice.accepted" then
      if body.conversation_id ~= p.args.conversation_id or type(body.receipt) ~= "table" then return false end
      for _, field in ipairs({ "notice_id", "epoch_id" }) do
        if p.args[field] ~= nil and p.args[field] ~= body.receipt[field] then return false end
      end
    end
    if kind == "conversation.compaction.prepared" then
      if p.op ~= "prepare" or source ~= p.args.provider or body.provider ~= p.args.provider
          or type(body.outcome) ~= "table" then return false end
      if body.outcome.kind == "Ready" then settle(id, ok(body.outcome))
      elseif body.outcome.kind == "Error" then settle(id, body.outcome) end
    elseif source ~= "conversation-manager" then return false
    elseif kind == "conversation.query.rejected" then settle(id, failure(body.code, body.context))
    elseif kind == "conversation.context.snapshot" and p.op == "read" then
      if not body.found or type(body.context) ~= "table" then settle(id, failure("conversation_not_found"))
      elseif (body.context.watermark or 0) < (p.args.min_watermark or 0) then settle(id, failure("conversation_watermark_stale"))
      else
        local view = copy(body.context); view.context = body.context
        settle(id, ok(view))
      end
    elseif kind == "conversation.context.notice.accepted" and p.op:match("notice$") then
      local operations = { append_notice = "context_notice_appended", bind_notice = "context_notice_included", acknowledge_notice = "context_notice_delivered" }
      if body.operation == operations[p.op] then settle(id, ok(body.receipt)) end
    elseif kind == "conversation.context.notice.rejected" and p.op:match("notice$") then settle(id, failure(body.code))
    elseif kind == "conversation.context.compact.rejected" and (p.op == "request_compaction" or p.op == "cancel_compaction") then settle(id, failure(body.code, body.context))
    elseif kind == "conversation.context.compact.cancelled" and p.op == "cancel_compaction" then settle(id, ok(body))
    elseif kind ~= "conversation.context.compact.accepted" then return false end
    return true
  end
  return broker
end
return M
