local M = {}

local function copy(value, seen)
  if type(value) ~= "table" then return value end
  seen = seen or {}
  if seen[value] then return seen[value] end
  local out = {}
  seen[value] = out
  for key, item in pairs(value) do out[copy(key, seen)] = copy(item, seen) end
  return out
end

local function content_chunk(content)
  if content == nil or content == "" then return nil end
  if type(content) == "string" then return { kind = "text", data = content } end
  return { kind = "structured", data = copy(content) }
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

  local function emit(kind, fields)
    if turn_terminal then return false end
    event_sequence = event_sequence + 1
    local fact = {
      event_id = conversation_id .. ":" .. turn_id .. ":event:" .. tostring(event_sequence),
      conversation_id = conversation_id,
      kind = kind,
      turn_id = turn_id,
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

  local function next_message_id()
    message_sequence = message_sequence + 1
    return conversation_id .. ":" .. turn_id .. ":message:" .. tostring(message_sequence)
  end

  local function record_tool_calls(message_id, calls)
    for index, raw_call in ipairs(calls or {}) do
      local fn = type(raw_call["function"]) == "table" and raw_call["function"] or {}
      local call_id = raw_call.id or (message_id .. ":tool:" .. tostring(index))
      local tool_name = raw_call.name or fn.name
      local arguments = raw_call.args or raw_call.arguments or fn.arguments
      local exchange_id = message_id .. ":exchange:" .. tostring(index)
      exchanges[call_id] = exchange_id
      emit("tool_exchange_started", {
        exchange_id = exchange_id,
        message_id = message_id,
        tool_call_id = call_id,
        tool_name = tool_name,
      })
      emit("tool_call_fragment_appended", {
        exchange_id = exchange_id,
        fragment = { arguments = copy(arguments) },
      })
      emit("tool_call_completed", {
        exchange_id = exchange_id,
        call = { tool_call_id = call_id, name = tool_name, arguments = copy(arguments) },
      })
    end
  end

  local recorder = { conversation_id = conversation_id }

  function recorder:create(provenance)
    return emit("created", { provenance = provenance or {} })
  end

  function recorder:start_turn()
    return emit("turn_started", { provenance = provenance })
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
