local domain = require("libs.conversation-manager.domain")

local M = { domain = domain }

local function readonly(conversation)
  return domain.read_model(conversation)
end

local function canonical(value, seen)
  local value_type = type(value)
  if value_type == "nil" then return "nil" end
  if value_type == "boolean" then return value and "b:1" or "b:0" end
  if value_type == "number" then return "n:" .. tostring(value) end
  if value_type == "string" then return "s:" .. #value .. ":" .. value end
  if value_type ~= "table" then return value_type .. ":" .. tostring(value) end
  seen = seen or {}
  if seen[value] then return "cycle" end
  seen[value] = true
  local keys = {}
  for key in pairs(value) do keys[#keys + 1] = key end
  table.sort(keys, function(a, b) return canonical(a) < canonical(b) end)
  local parts = { "t:{" }
  for _, key in ipairs(keys) do
    parts[#parts + 1] = canonical(key)
    parts[#parts + 1] = "="
    parts[#parts + 1] = canonical(value[key], seen)
    parts[#parts + 1] = ";"
  end
  parts[#parts + 1] = "}"
  seen[value] = nil
  return table.concat(parts)
end

local function fingerprint(value)
  local encoded = canonical(value)
  local first, second = 2166136261, 16777619
  for index = 1, #encoded do
    local byte = encoded:byte(index)
    first = (first * 131 + byte) % 2147483647
    second = (second * 257 + byte) % 2147483629
  end
  return tostring(#encoded) .. ":" .. tostring(first) .. ":" .. tostring(second)
end

local function append_to(conversations, event_index, fact, stats)
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
    if prior.fact_fingerprint == fingerprint(fact) then
      local event = domain.copy(fact)
      event.sequence = prior.sequence
      return conversations[prior.conversation_id], nil, true, event
    end
    return domain.error("event_id_conflict", {
      event_id = fact.event_id,
      existing_conversation_id = prior.conversation_id,
      conversation_id = fact.conversation_id,
    })
  end

  local current = conversations[fact.conversation_id]
  local event = domain.copy(fact)
  event.sequence = current and current.last_sequence + 1 or 1
  stats.fold_count = stats.fold_count + 1
  local folded, e = domain.fold(current, event)
  if not folded then return nil, e end

  conversations[fact.conversation_id] = folded
  event_index[fact.event_id] = {
    conversation_id = fact.conversation_id,
    sequence = event.sequence,
    fact_fingerprint = fingerprint(fact),
    event_fingerprint = fingerprint(event),
  }
  return folded, nil, false, domain.copy(event)
end

local function apply_recorded_to(conversations, event_index, event, stats)
  if type(event) ~= "table" then return domain.error("invalid_event", { value_type = type(event) }) end
  if type(event.event_id) ~= "string" or event.event_id == "" then
    return domain.error("invalid_event_id", { kind = event.kind })
  end
  if type(event.conversation_id) ~= "string" or event.conversation_id == "" then
    return domain.error("invalid_conversation_id", { kind = event.kind })
  end

  local prior = event_index[event.event_id]
  if prior then
    if prior.event_fingerprint == fingerprint(event) then
      return conversations[prior.conversation_id], nil, true
    end
    return domain.error("event_id_conflict", {
      event_id = event.event_id,
      existing_conversation_id = prior.conversation_id,
      conversation_id = event.conversation_id,
    })
  end

  local current = conversations[event.conversation_id]
  local expected = current and current.last_sequence + 1 or 1
  if event.sequence ~= expected then
    return domain.error("noncontiguous_sequence", {
      conversation_id = event.conversation_id,
      expected = expected,
      actual = event.sequence,
    })
  end

  stats.fold_count = stats.fold_count + 1
  local folded, e = domain.fold(current, event)
  if not folded then return nil, e end
  conversations[event.conversation_id] = folded
  event_index[event.event_id] = {
    conversation_id = event.conversation_id,
    sequence = event.sequence,
    fact_fingerprint = (function()
      local fact = domain.copy(event)
      fact.sequence = nil
      return fingerprint(fact)
    end)(),
    event_fingerprint = fingerprint(event),
  }
  return folded, nil, false
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
  local stats = { fold_count = 0 }

  local store = {}

  function store:append(fact)
    return append_to(conversations, event_index, fact, stats)
  end

  function store:apply_recorded(event)
    return apply_recorded_to(conversations, event_index, event, stats)
  end

  function store:get(conversation_id)
    return readonly(conversations[conversation_id])
  end

  -- Runtime-only read handle. The manager owns this table; callers must never
  -- retain or mutate it. Public query responses use get/list snapshots.
  function store:peek(conversation_id)
    return conversations[conversation_id]
  end

  function store:list()
    return list(conversations)
  end

  function store:list_owned()
    local ids = {}
    for id in pairs(conversations) do ids[#ids + 1] = id end
    table.sort(ids)
    local out = {}
    for _, id in ipairs(ids) do out[#out + 1] = conversations[id] end
    return out
  end

  function store:replay(events)
    if events ~= nil and type(events) ~= "table" then return domain.error("invalid_events", { value_type = type(events) }) end
    for index, event in ipairs(events or {}) do
      if type(event) ~= "table" then return domain.error("invalid_event", { value_type = type(event), replay_index = index }) end
      local result, e = apply_recorded_to(conversations, event_index, event, stats)
      if not result then
        e.context = e.context or {}
        e.context.replay_index = index
        return nil, e
      end
    end
    return list(conversations)
  end

  function store:stats()
    return { fold_count = stats.fold_count, event_ids = (function()
      local count = 0
      for _ in pairs(event_index) do count = count + 1 end
      return count
    end)() }
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
