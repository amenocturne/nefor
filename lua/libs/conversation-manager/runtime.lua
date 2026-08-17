local manager = require("libs.conversation-manager")
local domain = manager.domain
local projection = require("libs.conversation-manager.projection")
local service_lib = require("libs.conversation-manager.service")
local replay_window = require("core.replay_window")

local M = {}

local function nonempty(value)
  return type(value) == "string" and value ~= ""
end

function M.build(options)
  options = options or {}
  local json = options.json or nefor.json
  local now = options.now or function() return nefor.engine.now() end
  local emit = options.emit or function(payload) nefor.engine.send(payload) end
  local replay_active = options.replay_active or replay_window.active
  local service = options.service or service_lib.new()
  local store = service:writer()
  local reader = service:reader()
  local session_id = nil
  local active_conversation_id = nil
  local active_invocations = {}
  local usage_owners = {}
  local usage_queries = {}
  local usage_subscriptions = {}

  local function send(body)
    emit(json.encode({
      type = "event",
      from = "conversation-manager",
      ts = now(),
      body = body,
    }))
  end

  local function reject(fact, e)
    send({
      kind = "conversation.fact.rejected",
      event_id = type(fact) == "table" and fact.event_id or nil,
      conversation_id = type(fact) == "table" and fact.conversation_id or nil,
      code = e.code,
      context = domain.copy(e.context),
    })
  end

  local function diagnose(event, e)
    send({
      kind = "conversation-manager.diagnostic",
      level = "error",
      code = e.code,
      event_id = type(event) == "table" and event.event_id or nil,
      conversation_id = type(event) == "table" and event.conversation_id or nil,
      context = domain.copy(e.context),
    })
  end

  local function reset(next_session_id)
    store:reset()
    active_invocations = {}
    usage_queries = {}
    session_id = next_session_id
    active_conversation_id = nil
  end

  local function publish_recorded(event, duplicate, conversation)
    send({
      kind = "conversation.fact.recorded",
      event = event,
      duplicate = duplicate,
      session_id = session_id,
    })
    if not duplicate then
      send({
        kind = "conversation.projection.delta",
        conversation_id = event.conversation_id,
        sequence = event.sequence,
        change = projection.change(conversation, event),
      })
    end
  end

  local function append_fact(fact)
    local conversation, e, duplicate, event = store:append(fact)
    if not conversation then reject(fact, e); return nil end
    publish_recorded(event, duplicate, conversation)
    return conversation, duplicate, event
  end

  local function handle_append(body)
    if replay_active() then return end
    append_fact(body.fact)
  end

  local function handle_recorded(body)
    local event = body.event
    local conversation, e, duplicate = store:apply_recorded(event)
    if e then diagnose(body.event, e) end
    if conversation and not duplicate and replay_active() then
      send({
        kind = "conversation.projection.delta",
        session_id = session_id,
        conversation_id = event.conversation_id,
        sequence = event.sequence,
        replay = true,
        change = projection.change(conversation, event),
      })
    end
    if conversation and not duplicate and event.kind == "active_selected" then
      active_conversation_id = event.conversation_id
      send({
        kind = "conversation.active.changed",
        session_id = session_id,
        conversation_id = active_conversation_id,
        sequence = event.sequence,
        replay = replay_active() or nil,
      })
    end
  end

  local function handle_get(body)
    if replay_active() then return end
    local valid = nonempty(body.request_id) and nonempty(body.conversation_id)
    if not valid then
      send({
        kind = "conversation.query.rejected",
        request_id = body.request_id,
        code = "invalid_get_request",
      })
      return
    end
    local conversation = reader:conversation(body.conversation_id)
    send({
      kind = "conversation.snapshot",
      request_id = body.request_id,
      conversation_id = body.conversation_id,
      found = conversation ~= nil,
      projection = conversation,
    })
  end

  local function handle_list(body)
    if replay_active() then return end
    if not nonempty(body.request_id) then
      send({
        kind = "conversation.query.rejected",
        request_id = body.request_id,
        code = "invalid_list_request",
      })
      return
    end
    send({
      kind = "conversation.list",
      request_id = body.request_id,
      conversations = (function()
        local out = {}
        for _, conversation in ipairs(reader:list()) do out[#out + 1] = conversation end
        return out
      end)(),
    })
  end

  local function handle_context(body)
    if replay_active() then return end
    if not nonempty(body.request_id) or not nonempty(body.conversation_id) then
      send({ kind = "conversation.query.rejected", request_id = body.request_id, code = "invalid_context_request" })
      return
    end
    local context = reader:context(body.conversation_id)
    send({
      kind = "conversation.context.snapshot",
      request_id = body.request_id,
      conversation_id = body.conversation_id,
      found = context ~= nil,
      context = context,
    })
  end

  local function handle_compaction_request(body)
    if replay_active() then return end
    if not nonempty(body.request_id) or not nonempty(body.conversation_id) or not nonempty(body.provider) then
      send({ kind = "conversation.query.rejected", request_id = body.request_id, code = "invalid_compaction_request" })
      return
    end
    local conversation = store:owned(body.conversation_id)
    if not conversation then
      reject({ event_id = "compaction:" .. body.request_id .. ":requested", conversation_id = body.conversation_id },
        { code = "conversation_not_found", context = { request_id = body.request_id } })
      return
    end
    append_fact({
      event_id = "compaction:" .. body.request_id .. ":requested",
      conversation_id = body.conversation_id,
      kind = "context_compaction_requested",
      request_id = body.request_id,
      history_cutoff = projection.context(conversation).history_length,
      provider = body.provider,
      model = body.model,
    })
  end

  local function handle_compaction_terminal(body, kind)
    if replay_active() then return end
    if not nonempty(body.request_id) or not nonempty(body.conversation_id) then
      send({ kind = "conversation.query.rejected", request_id = body.request_id, code = "invalid_compaction_terminal" })
      return
    end
    append_fact({
      event_id = "compaction:" .. body.request_id .. ":" .. (kind == "context_compaction_completed" and "completed" or "failed"),
      conversation_id = body.conversation_id,
      kind = kind,
      request_id = body.request_id,
      checkpoint = domain.copy(body.checkpoint),
      error = domain.copy(body.error),
    })
  end

  local function handle_active_set(body)
    if replay_active() then return end
    if not nonempty(body.request_id) or not nonempty(body.conversation_id) then
      send({ kind = "conversation.query.rejected", request_id = body.request_id, code = "invalid_active_request" })
      return
    end
    if not store:owned(body.conversation_id) then
      send({ kind = "conversation.query.rejected", request_id = body.request_id, code = "conversation_not_found" })
      return
    end
    local _, duplicate, event = append_fact({
      event_id = body.event_id or ("active:" .. body.request_id),
      conversation_id = body.conversation_id,
      kind = "active_selected",
    })
    if event and not duplicate then
      active_conversation_id = body.conversation_id
      send({
        kind = "conversation.active.changed",
        request_id = body.request_id,
        session_id = session_id,
        conversation_id = active_conversation_id,
        sequence = event.sequence,
      })
    end
  end

  local function provider_rejection(action, body, code, context)
    send({
      kind = "conversation.provider." .. action .. ".rejected",
      request_id = type(body) == "table" and body.request_id or nil,
      conversation_id = type(body) == "table" and body.conversation_id or nil,
      provider = type(body) == "table" and body.provider or nil,
      code = code,
      context = domain.copy(context),
    })
  end

  local function valid_provider_address(body)
    return nonempty(body.request_id) and nonempty(body.conversation_id)
      and nonempty(body.provider)
  end

  local invocation_fields = {
    "model", "tools", "reasoning_effort", "output_schema",
    "max_corrections", "invocation",
  }

  local function handle_provider_invoke(body)
    if replay_active() then return end
    if not valid_provider_address(body) then
      provider_rejection("invoke", body, "invalid_provider_invoke_request")
      return
    end
    local conversation = store:owned(body.conversation_id)
    if not conversation then
      provider_rejection("invoke", body, "conversation_not_found")
      return
    end
    local prior = active_invocations[body.request_id]
    if prior then
      provider_rejection("invoke", body, "provider_request_id_conflict", {
        existing_conversation_id = prior.conversation_id,
        existing_provider = prior.provider,
      })
      return
    end

    active_invocations[body.request_id] = {
      conversation_id = body.conversation_id,
      provider = body.provider,
    }
    local invocation = {
      kind = "conversation.provider.invoke",
      request_id = body.request_id,
      conversation_id = body.conversation_id,
      provider = body.provider,
      watermark = conversation.last_sequence,
    }
    for _, field in ipairs(invocation_fields) do
      if body[field] ~= nil then invocation[field] = domain.copy(body[field]) end
    end
    send(invocation)
  end

  local function matching_invocation(action, body)
    if not nonempty(body.request_id) or not nonempty(body.provider) then
      provider_rejection(action, body, "invalid_provider_" .. action .. "_request")
      return nil
    end
    local active = active_invocations[body.request_id]
    if not active then
      provider_rejection(action, body, "provider_request_not_found")
      return nil
    end
    if active.provider ~= body.provider
        or (body.conversation_id ~= nil and active.conversation_id ~= body.conversation_id) then
      provider_rejection(action, body, "provider_request_mismatch", {
        expected_conversation_id = active.conversation_id,
        expected_provider = active.provider,
      })
      return nil
    end
    return active
  end

  local function handle_provider_cancel(body)
    if replay_active() then return end
    local active = matching_invocation("cancel", body)
    if not active then return end
    active_invocations[body.request_id] = nil
    send({
      kind = "conversation.provider.cancel",
      request_id = body.request_id,
      conversation_id = active.conversation_id,
      provider = body.provider,
    })
  end

  local terminal_provider_events = { completed = true, failed = true, interrupted = true }
  local provider_event_reserved = {
    kind = true, request_id = true, conversation_id = true, provider = true,
    event = true,
    messages = true, history = true, conversation_context = true,
    input = true, request = true, system = true, tool_specs = true,
  }

  local function handle_provider_event(body)
    if replay_active() then return end
    local active = matching_invocation("event", body)
    if not active then return end
    if not nonempty(body.event) then
      provider_rejection("event", body, "invalid_provider_event")
      return
    end
    local event = {
      kind = "conversation.provider.event",
      request_id = body.request_id,
      conversation_id = active.conversation_id,
      provider = body.provider,
      event = body.event,
    }
    for field, value in pairs(body) do
      if not provider_event_reserved[field] then event[field] = domain.copy(value) end
    end
    send(event)
    if terminal_provider_events[body.event] then active_invocations[body.request_id] = nil end
  end

  local function usage_id(value)
    return type(value) == "string" and #value <= 256
      and value:match("^[%w_.-]+/[%w_.-]+$") ~= nil
  end

  local function usage_ids(value)
    if type(value) ~= "table" then return nil end
    local out, seen = {}, {}
    for _, id in ipairs(value) do
      if not usage_id(id) or seen[id] then return nil end
      seen[id] = true
      out[#out + 1] = id
    end
    return out
  end

  local function ids_for_owner(ids, owner)
    local out = {}
    for _, id in ipairs(ids) do
      if usage_owners[id] == owner then out[#out + 1] = id end
    end
    return out
  end

  local function route_usage(kind, correlation_id, requester, ids)
    local by_owner = {}
    for _, id in ipairs(ids) do
      local owner = usage_owners[id]
      if owner then
        by_owner[owner] = by_owner[owner] or {}
        by_owner[owner][#by_owner[owner] + 1] = id
      end
    end
    local owners = {}
    for owner in pairs(by_owner) do owners[#owners + 1] = owner end
    table.sort(owners)
    for _, owner in ipairs(owners) do
      local owned = by_owner[owner]
      table.sort(owned)
      send({
        kind = "conversation.usage." .. kind .. ".forwarded",
        request_id = kind == "query" and correlation_id or nil,
        subscription_id = kind == "subscribe" and correlation_id or nil,
        requester = requester,
        owner = owner,
        usage_ids = owned,
      })
    end
    return by_owner
  end

  local function handle_usage_expose(body, owner)
    if replay_active() or owner == "conversation-manager" then return end
    local ids = usage_ids(body.usage_ids)
    if not nonempty(owner) or not ids or #ids == 0 then
      send({ kind = "conversation.usage.exposure.rejected", owner = owner,
        code = "invalid_usage_exposure" })
      return
    end
    for _, id in ipairs(ids) do
      if id:sub(1, #owner + 1) ~= owner .. "/" then
        send({ kind = "conversation.usage.exposure.rejected", owner = owner,
          usage_id = id, code = "usage_id_namespace_mismatch" })
        return
      end
      local existing = usage_owners[id]
      if existing and existing ~= owner then
        send({ kind = "conversation.usage.exposure.rejected", owner = owner,
          usage_id = id, existing_owner = existing, code = "usage_id_collision" })
        return
      end
    end
    table.sort(ids)
    local newly_exposed = {}
    for _, id in ipairs(ids) do
      if not usage_owners[id] then newly_exposed[#newly_exposed + 1] = id end
      usage_owners[id] = owner
    end
    send({ kind = "conversation.usage.exposed", owner = owner, usage_ids = ids })
    if #newly_exposed > 0 then
      local subscription_ids = {}
      for subscription_id in pairs(usage_subscriptions) do
        subscription_ids[#subscription_ids + 1] = subscription_id
      end
      table.sort(subscription_ids)
      for _, subscription_id in ipairs(subscription_ids) do
        local subscription = usage_subscriptions[subscription_id]
        local late = ids_for_owner(newly_exposed, owner)
        local requested = {}
        for _, id in ipairs(late) do
          if subscription.usage_ids[id] then requested[#requested + 1] = id end
        end
        if #requested > 0 then
          route_usage("subscribe", subscription_id, subscription.requester, requested)
        end
      end
    end
  end

  local function requested_by_owner(ids)
    local out = {}
    for _, id in ipairs(ids) do
      local owner = usage_owners[id]
      if owner then
        out[owner] = out[owner] or {}
        out[owner][id] = true
      end
    end
    return out
  end

  local function handle_usage_query(body, requester)
    if replay_active() or requester == "conversation-manager"
        or body.kind ~= "conversation.usage.query" then return end
    if nonempty(body.request_id) and usage_queries[body.request_id] then
      send({ kind = "conversation.usage.query.rejected", request_id = body.request_id,
        code = "usage_request_id_conflict" })
      return
    end
    local ids = usage_ids(body.usage_ids)
    if not nonempty(body.request_id) or not nonempty(requester) or not ids or #ids == 0 then
      send({ kind = "conversation.usage.query.rejected", request_id = body.request_id,
        code = "invalid_usage_query" })
      return
    end
    table.sort(ids)
    local exposed = {}
    for _, id in ipairs(ids) do if usage_owners[id] then exposed[#exposed + 1] = id end end
    if #exposed < #ids then
      local missing = {}
      for _, id in ipairs(ids) do if not usage_owners[id] then missing[#missing + 1] = id end end
      send({ kind = "conversation.usage.query.unavailable", request_id = body.request_id,
        requester = requester, usage_ids = missing, reason = "usage_id_not_exposed" })
    end
    if #exposed == 0 then return end
    route_usage("query", body.request_id, requester, exposed)
    usage_queries[body.request_id] = {
      requester = requester,
      requested = requested_by_owner(exposed),
    }
  end

  local function handle_usage_subscribe(body, requester)
    if replay_active() or requester == "conversation-manager"
        or body.kind ~= "conversation.usage.subscribe" then return end
    local existing = nonempty(body.subscription_id)
      and usage_subscriptions[body.subscription_id] or nil
    if existing and existing.requester ~= requester then
      send({ kind = "conversation.usage.subscription.rejected",
        subscription_id = body.subscription_id, code = "usage_subscription_id_conflict" })
      return
    end
    local ids = usage_ids(body.usage_ids)
    if not nonempty(body.subscription_id) or not nonempty(requester) or not ids or #ids == 0 then
      send({ kind = "conversation.usage.subscription.rejected",
        subscription_id = body.subscription_id, code = "invalid_usage_subscription" })
      return
    end
    table.sort(ids)
    local wanted = {}
    if existing then
      for id in pairs(existing.usage_ids) do wanted[id] = true end
    end
    for _, id in ipairs(ids) do wanted[id] = true end
    usage_subscriptions[body.subscription_id] = { requester = requester, usage_ids = wanted }
    route_usage("subscribe", body.subscription_id, requester, ids)
  end

  local valid_usage_kinds = { subscription = true, monetary = true, free = true, unknown = true }

  local function published_values(body, owner)
    local values = type(body.values) == "table" and body.values or nil
    if not values then return nil end
    local out, seen = {}, {}
    for _, value in ipairs(values) do
      if type(value) ~= "table" or not usage_id(value.usage_id)
          or seen[value.usage_id] or usage_owners[value.usage_id] ~= owner
          or type(value.usage) ~= "table"
          or not valid_usage_kinds[value.usage.kind] then return nil end
      seen[value.usage_id] = true
      if value.usage.kind == "monetary"
          and (type(value.usage.amount) ~= "string"
            or not value.usage.amount:match("^%d+%.?%d*$")
            or type(value.usage.currency) ~= "string") then return nil end
      out[#out + 1] = domain.copy(value)
    end
    table.sort(out, function(a, b) return a.usage_id < b.usage_id end)
    return out
  end

  local function handle_usage_snapshot(body, owner)
    if replay_active() or owner == "conversation-manager" then return end
    local query = nonempty(body.request_id) and usage_queries[body.request_id] or nil
    local requested = query and query.requested[owner] or nil
    local values = requested and published_values(body, owner) or nil
    if not values then
      send({ kind = "conversation.usage.publish.rejected", request_id = body.request_id,
        owner = owner, code = "usage_snapshot_mismatch" })
      return
    end
    local seen = {}
    for _, value in ipairs(values or {}) do
      if not requested[value.usage_id] or seen[value.usage_id] then
        send({ kind = "conversation.usage.publish.rejected", request_id = body.request_id,
          owner = owner, code = "usage_snapshot_id_mismatch" })
        return
      end
      seen[value.usage_id] = true
    end
    for id in pairs(requested or {}) do
      if not seen[id] then
        send({ kind = "conversation.usage.publish.rejected", request_id = body.request_id,
          owner = owner, code = "incomplete_usage_snapshot" })
        return
      end
    end
    query.requested[owner] = nil
    send({ kind = "conversation.usage.snapshot", request_id = body.request_id,
      requester = query.requester, values = values })
    if next(query.requested) == nil then usage_queries[body.request_id] = nil end
  end

  local function handle_usage_update(body, owner)
    if replay_active() or owner == "conversation-manager" then return end
    local subscription = nonempty(body.subscription_id)
      and usage_subscriptions[body.subscription_id] or nil
    local values = subscription and published_values(body, owner) or nil
    if not values then
      send({ kind = "conversation.usage.publish.rejected",
        subscription_id = body.subscription_id, owner = owner,
        code = "usage_update_mismatch" })
      return
    end
    if #values == 0 then
      send({ kind = "conversation.usage.publish.rejected",
        subscription_id = body.subscription_id, owner = owner,
        code = "empty_usage_update" })
      return
    end
    for _, value in ipairs(values) do
      if not subscription.usage_ids[value.usage_id] then
        send({ kind = "conversation.usage.publish.rejected",
          subscription_id = body.subscription_id, owner = owner,
          code = "usage_update_not_subscribed", usage_id = value.usage_id })
        return
      end
    end
    send({ kind = "conversation.usage.update", subscription_id = body.subscription_id,
      requester = subscription.requester, values = values })
  end

  local function receive_msg(entry)
    if type(entry.payload) ~= "string" or entry.payload == "" then return end
    local ok, decoded = pcall(json.decode, entry.payload)
    if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
    local body = decoded.body

    if body.kind == "sessions.session_end" then reset(nil); return end
    if body.kind == "sessions.session_start" then
      reset(body.session_id)
      local subscription_ids = {}
      for subscription_id in pairs(usage_subscriptions) do
        subscription_ids[#subscription_ids + 1] = subscription_id
      end
      table.sort(subscription_ids)
      for _, subscription_id in ipairs(subscription_ids) do
        local subscription = usage_subscriptions[subscription_id]
        route_usage("subscribe", subscription_id, subscription.requester,
          (function()
            local ids = {}
            for id in pairs(subscription.usage_ids) do ids[#ids + 1] = id end
            table.sort(ids)
            return ids
          end)())
      end
      return
    end
    if body.kind == "conversation.fact.append" then handle_append(body); return end
    if body.kind == "conversation.fact.recorded" then
      if decoded.from ~= "conversation-manager" then
        diagnose(body.event, {
          code = "recorded_fact_source_mismatch",
          context = { expected = "conversation-manager", actual = decoded.from },
        })
        return
      end
      handle_recorded(body)
      return
    end
    if body.kind == "conversation.get.request" then handle_get(body); return end
    if body.kind == "conversation.list.request" then handle_list(body); return end
    if body.kind == "conversation.context.request" then handle_context(body); return end
    if body.kind == "conversation.active.set" then handle_active_set(body); return end
    if body.kind == "conversation.context.compact.request" then handle_compaction_request(body); return end
    if body.kind == "conversation.context.compact.complete" then
      handle_compaction_terminal(body, "context_compaction_completed"); return
    end
    if body.kind == "conversation.context.compact.failed" then
      handle_compaction_terminal(body, "context_compaction_failed"); return
    end
    if body.kind == "conversation.provider.invoke.request" then handle_provider_invoke(body); return end
    if body.kind == "conversation.provider.cancel.request" then handle_provider_cancel(body); return end
    if body.kind == "conversation.provider.event.reported" then handle_provider_event(body); return end
    if body.kind == "conversation.usage.expose" then handle_usage_expose(body, decoded.from); return end
    if body.kind == "conversation.usage.query" then handle_usage_query(body, decoded.from); return end
    if body.kind == "conversation.usage.subscribe" then handle_usage_subscribe(body, decoded.from); return end
    if body.kind == "conversation.usage.snapshot.reported" then handle_usage_snapshot(body, decoded.from); return end
    if body.kind == "conversation.usage.update.reported" then handle_usage_update(body, decoded.from); return end
  end

  return {
    name = "conversation-manager",
    receive_msg = receive_msg,
    send_msg = function(_) end,
    _internals = {
      get = function(id) return reader:conversation(id) end,
      list = function() return reader:list() end,
      active_conversation_id = function() return active_conversation_id end,
      stats = function() return store:stats() end,
      active_invocations = function() return domain.copy(active_invocations) end,
      reader = function() return reader end,
      session_id = function() return session_id end,
      reset = reset,
    },
  }
end

return M
