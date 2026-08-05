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

function M.new()
  local conversation_id = nil
  local sequence = 0
  local context = { messages = {}, tail_messages = {}, compaction = nil }
  local provenance = {}

  local projection = {}

  function projection:apply_delta(delta)
    if type(delta) ~= "table" or type(delta.conversation_id) ~= "string"
        or type(delta.change) ~= "table" then
      return false
    end
    local change = delta.change
    if change.kind == "conversation_created" then
      conversation_id = delta.conversation_id
      sequence = delta.sequence or 0
      context = { messages = {}, tail_messages = {}, compaction = nil }
      provenance = copy((change.conversation or {}).provenance or {})
      return true
    end
    if conversation_id ~= nil and delta.conversation_id ~= conversation_id then
      return false
    end
    if conversation_id == nil then return false end
    if type(delta.sequence) == "number" and delta.sequence <= sequence then
      return false
    end
    sequence = delta.sequence or sequence
    if change.kind == "turn_started" or change.kind == "provenance_updated" then
      for key, value in pairs(change.provenance or {}) do provenance[key] = copy(value) end
    end
    if change.kind == "message_completed" or change.kind == "message_interrupted" then
      for _, message in ipairs(change.context_messages or {}) do
        local item = copy(message)
        context.messages[#context.messages + 1] = item
        context.tail_messages[#context.tail_messages + 1] = copy(message)
      end
      return true
    end
    return true
  end

  function projection:apply_snapshot(snapshot)
    if type(snapshot) ~= "table" or snapshot.found ~= true
        or type(snapshot.conversation_id) ~= "string" then
      return false
    end
    if conversation_id ~= nil and snapshot.conversation_id ~= conversation_id then
      return false
    end
    conversation_id = snapshot.conversation_id
    sequence = snapshot.sequence or sequence
    context = copy(snapshot.context or {})
    context.messages = context.messages or {}
    context.tail_messages = context.tail_messages or {}
    return true
  end

  function projection:id() return conversation_id end
  function projection:sequence() return sequence end
  function projection:context() return copy(context) end
  function projection:history() return copy(context.messages) end
  function projection:compaction() return copy(context.compaction) end
  function projection:provenance() return copy(provenance) end

  function projection:reset()
    conversation_id = nil
    sequence = 0
    context = { messages = {}, tail_messages = {}, compaction = nil }
    provenance = {}
  end

  return projection
end

return M
