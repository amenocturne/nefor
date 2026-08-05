local manager = require("libs.conversation-manager")
local domain = manager.domain
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
  end

  local function handle_append(body)
    if replay_active() then return end
    local fact = body.fact
    local conversation, e, duplicate, event = store:append(fact)
    if not conversation then reject(fact, e); return end
    send({
      kind = "conversation.fact.recorded",
      event = event,
      duplicate = duplicate,
      session_id = session_id,
    })
  end

  local function handle_recorded(body)
    local _, e = store:apply_recorded(body.event)
    if e then diagnose(body.event, e) end
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
    local conversation = store:get(body.conversation_id)
    send({
      kind = "conversation.snapshot",
      request_id = body.request_id,
      conversation_id = body.conversation_id,
      found = conversation ~= nil,
      conversation = conversation,
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
      conversations = store:list(),
    })
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
  end

  return {
    name = "conversation-manager",
    receive_msg = receive_msg,
    send_msg = function(_) end,
    _internals = {
      get = function(id) return store:get(id) end,
      list = function() return store:list() end,
      session_id = function() return session_id end,
      reset = reset,
    },
  }
end

return M
