local manager = require("libs.conversation-manager")
local domain = manager.domain
local projection = require("libs.conversation-manager.projection")
local replay_window = require("core.history_replay")

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
  local store = manager.new()
  local session_id = nil
  local active_conversation_id = nil

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
    store = manager.new()
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
    local conversation = projection.conversation(store:peek(body.conversation_id))
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
        for _, conversation in ipairs(store:list_owned()) do out[#out + 1] = projection.conversation(conversation) end
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
    local context = projection.context(store:peek(body.conversation_id))
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
    local conversation = store:peek(body.conversation_id)
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
    if not store:peek(body.conversation_id) then
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

  local function receive_msg(entry)
    if entry.origin == "step" and entry.target ~= nil then return end
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
  end

  return {
    name = "conversation-manager",
    receive_msg = receive_msg,
    send_msg = function(_) end,
    _internals = {
      get = function(id) return store:get(id) end,
      list = function() return store:list() end,
      active_conversation_id = function() return active_conversation_id end,
      stats = function() return store:stats() end,
      session_id = function() return session_id end,
      reset = reset,
    },
  }
end

return M
