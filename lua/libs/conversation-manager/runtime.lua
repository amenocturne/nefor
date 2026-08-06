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

  local function receive_msg(entry)
    if type(entry.payload) ~= "string" or entry.payload == "" then return end
    local ok, decoded = pcall(json.decode, entry.payload)
    if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
    local body = decoded.body

    if body.kind == "sessions.session_end" then reset(nil); return end
    if body.kind == "sessions.session_start" then reset(body.session_id); return end
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
