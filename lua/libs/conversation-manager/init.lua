local domain = require("libs.conversation-manager.domain")

local M = { domain = domain }

local function readonly(conversation)
  return domain.read_model(conversation)
end

local function append_to(conversations, event_index, fact)
  if type(fact) ~= "table" then return domain.error("invalid_event", { value_type = type(fact) }) end
  if type(fact.event_id) ~= "string" or fact.event_id == "" then
    return domain.error("invalid_event_id", { kind = fact.kind })
  end
  if type(fact.conversation_id) ~= "string" or fact.conversation_id == "" then
    return domain.error("invalid_conversation_id", { kind = fact.kind })
  end
  if fact.sequence ~= nil then
    return domain.error("sequence_manager_owned", { conversation_id = fact.conversation_id, event_id = fact.event_id, sequence = fact.sequence })
  end

  local prior = event_index[fact.event_id]
  if prior then
    if domain.equal(prior.fact, fact) then return readonly(conversations[prior.conversation_id]), nil, true end
    return domain.error("event_id_conflict", {
      event_id = fact.event_id,
      existing_conversation_id = prior.conversation_id,
      conversation_id = fact.conversation_id,
    })
  end

  local current = conversations[fact.conversation_id]
  local event = domain.copy(fact)
  event.sequence = current and current.last_sequence + 1 or 1
  local folded, e = domain.fold(current, event)
  if not folded then return nil, e end

  conversations[fact.conversation_id] = folded
  event_index[fact.event_id] = { conversation_id = fact.conversation_id, fact = domain.copy(fact) }
  return readonly(folded), nil, false
end

local function list(conversations)
  local ids = {}
  for id in pairs(conversations) do ids[#ids + 1] = id end
  table.sort(ids)
  local out = {}
  for _, id in ipairs(ids) do out[#out + 1] = readonly(conversations[id]) end
  return out
end

function M.new()
  local conversations = {}
  local event_index = {}

  local store = {}

  function store:append(fact)
    return append_to(conversations, event_index, fact)
  end

  function store:get(conversation_id)
    return readonly(conversations[conversation_id])
  end

  function store:list()
    return list(conversations)
  end

  function store:replay(events)
    if events ~= nil and type(events) ~= "table" then return domain.error("invalid_events", { value_type = type(events) }) end
    local next_conversations = domain.copy(conversations)
    local next_event_index = domain.copy(event_index)
    for index, event in ipairs(events or {}) do
      if type(event) ~= "table" then return domain.error("invalid_event", { value_type = type(event), replay_index = index }) end
      local current = next_conversations[event.conversation_id]
      local expected = current and current.last_sequence + 1 or 1
      if event.sequence ~= expected then
        return domain.error("noncontiguous_sequence", {
          conversation_id = event.conversation_id, expected = expected,
          actual = event.sequence, replay_index = index,
        })
      end
      local fact = domain.copy(event); fact.sequence = nil
      local result, e = append_to(next_conversations, next_event_index, fact)
      if not result then
        e.context = e.context or {}
        e.context.replay_index = index
        return nil, e
      end
    end
    conversations = next_conversations
    event_index = next_event_index
    return list(conversations)
  end

  return store
end

M.ConversationId = function(value)
  if type(value) ~= "string" or value == "" then return domain.error("invalid_conversation_id", { value = value }) end
  return value
end
M.ConversationEvent = domain.copy
M.Conversation = domain.read_model
M.Message = domain.copy
M.ContentChunk = domain.copy
M.ToolExchange = domain.copy

return M
