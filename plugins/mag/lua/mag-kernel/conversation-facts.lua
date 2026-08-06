local M = {}
local copy = require("core.json_data").copy

local function content_chunk(content)
  if content == nil or content == "" then return nil end
  if type(content) == "string" then return { kind = "text", data = content } end
  return { kind = "structured", data = copy(content) }
end

local function universal_arguments(arguments)
  if type(arguments) ~= "string" then return copy(arguments) end
  if type(nefor) == "table" and type(nefor.json) == "table"
      and type(nefor.json.decode) == "function" then
    local ok, decoded = pcall(nefor.json.decode, arguments)
    if ok then return copy(decoded) end
  end
  return arguments
end

function M.new(options)
  options = options or {}
  assert(type(options.conversation_id) == "string" and options.conversation_id ~= "",
    "conversation facts need an explicit conversation_id")
  assert(type(options.emit) == "function", "conversation facts need an emit function")

  local conversation_id = options.conversation_id
  local turn_id = assert(options.turn_id, "conversation facts need a turn_id")
  local provenance = copy(options.provenance or {})
  local emit_fact = options.emit
  local event_sequence = 0
  local message_sequence = 0
  local retry_sequence = 0
  local exchanges = {}
  local turn_terminal = false

  local function emit_event(kind, fields, event_turn_id)
    if turn_terminal then return false end
    event_sequence = event_sequence + 1
    local fact_turn_id = event_turn_id
    if fact_turn_id == false then fact_turn_id = nil end
    local fact = {
      event_id = conversation_id .. ":" .. turn_id .. ":event:" .. tostring(event_sequence),
      conversation_id = conversation_id,
      kind = kind,
      turn_id = fact_turn_id,
      run_id = provenance.run_id,
      actor_id = provenance.actor_id,
    }
    for key, value in pairs(fields or {}) do fact[key] = copy(value) end
    emit_fact(fact)
    if kind == "turn_completed" or kind == "turn_interrupted" or kind == "turn_failed" then
      turn_terminal = true
    end
    return true
  end

  local function emit(kind, fields)
    return emit_event(kind, fields, turn_id)
  end

  local function next_message_id()
    message_sequence = message_sequence + 1
    return conversation_id .. ":" .. turn_id .. ":message:" .. tostring(message_sequence)
  end

  local function record_tool_calls(message_id, calls, event_turn_id)
    if event_turn_id == nil then event_turn_id = turn_id end
    for index, raw_call in ipairs(calls or {}) do
      local fn = type(raw_call["function"]) == "table" and raw_call["function"] or {}
      local call_id = raw_call.id or (message_id .. ":tool:" .. tostring(index))
      local tool_name = raw_call.name or fn.name
      local arguments = universal_arguments(raw_call.args or raw_call.arguments or fn.arguments)
      local exchange_id = message_id .. ":exchange:" .. tostring(index)
      exchanges[call_id] = exchange_id
      emit_event("tool_exchange_started", {
        exchange_id = exchange_id,
        message_id = message_id,
        tool_call_id = call_id,
        tool_name = tool_name,
      }, event_turn_id)
      emit_event("tool_call_fragment_appended", {
        exchange_id = exchange_id,
        fragment = { arguments = copy(arguments) },
      }, event_turn_id)
      emit_event("tool_call_completed", {
        exchange_id = exchange_id,
        call = { tool_call_id = call_id, name = tool_name, arguments = copy(arguments) },
      }, event_turn_id)
    end
  end

  local recorder = { conversation_id = conversation_id }

  function recorder:create(provenance)
    return emit("created", { provenance = provenance or {} })
  end

  function recorder:start_turn()
    return emit("turn_started", { provenance = provenance })
  end

  -- Authored history is initial conversation state, not part of the live turn
  -- that follows it. Record it without a turn id so a provider can rebuild the
  -- same context from canonical facts without MAG retaining another transcript.
  function recorder:seed_message(message)
    if type(message) ~= "table" or type(message.role) ~= "string" then return nil end
    local message_id = next_message_id()
    emit_event("message_started", {
      message_id = message_id,
      role = message.role,
      tool_call_id = message.tool_call_id,
      name = message.name,
    }, nil)
    local chunk = content_chunk(message.content)
    if chunk then
      emit_event("content_chunk_appended", {
        message_id = message_id,
        chunk = { kind = chunk.kind, data = chunk.data },
      }, nil)
    end
    if message.role == "assistant" then
      record_tool_calls(message_id, message.tool_calls, false)
    elseif message.role == "tool" then
      local exchange_id = exchanges[message.tool_call_id]
      if exchange_id then
        local field = message.error ~= nil and "error" or "result"
        emit_event(message.error ~= nil and "tool_error_recorded" or "tool_result_recorded", {
          exchange_id = exchange_id,
          [field] = message.error or message.content,
        }, nil)
      end
    end
    emit_event("message_completed", { message_id = message_id, completion = {} }, nil)
    return message_id
  end

  function recorder:start_message(role, fields)
    local message_id = next_message_id()
    local started = { message_id = message_id, role = role }
    for key, value in pairs(fields or {}) do started[key] = copy(value) end
    emit("message_started", started)
    return message_id
  end

  function recorder:content(message_id, kind, data)
    if data == nil or data == "" then return false end
    return emit("content_chunk_appended", {
      message_id = message_id,
      chunk = { kind = kind, data = data },
    })
  end

  function recorder:finish_message(message_id, message, completion)
    message = message or {}
    if message.role == "assistant" then
      record_tool_calls(message_id, message.tool_calls)
    elseif message.role == "tool" then
      local exchange_id = exchanges[message.tool_call_id]
      if exchange_id then
        local field = message.error ~= nil and "error" or "result"
        emit(message.error ~= nil and "tool_error_recorded" or "tool_result_recorded", {
          exchange_id = exchange_id,
          [field] = message.error or message.content,
        })
      end
    end
    emit("message_completed", { message_id = message_id, completion = completion or {} })
  end

  function recorder:interrupt_message(message_id, detail)
    return emit("message_interrupted", { message_id = message_id, detail = detail or {} })
  end

  function recorder:message(message, completion)
    if type(message) ~= "table" or type(message.role) ~= "string" then return nil end
    local message_id = self:start_message(message.role, {
      tool_call_id = message.tool_call_id,
      name = message.name,
    })
    local chunk = content_chunk(message.content)
    if chunk then self:content(message_id, chunk.kind, chunk.data) end
    self:finish_message(message_id, message, completion)
    return message_id
  end

  function recorder:retry(reason, provenance)
    retry_sequence = retry_sequence + 1
    emit("retry_started", {
      retry_id = conversation_id .. ":" .. turn_id .. ":retry:" .. tostring(retry_sequence),
      reason = reason,
      provenance = provenance or {},
    })
  end

  function recorder:complete_turn(detail)
    return emit("turn_completed", { detail = detail or {} })
  end

  function recorder:interrupt_turn(detail)
    return emit("turn_interrupted", { detail = detail or {} })
  end

  function recorder:fail_turn(detail)
    return emit("turn_failed", { detail = detail or {} })
  end

  return recorder
end

return M
