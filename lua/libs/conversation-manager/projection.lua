local domain = require("libs.conversation-manager.domain")

local M = {}

local function find_by_id(items, id, field)
  field = field or "id"
  for _, item in ipairs(items or {}) do if item[field] == id then return item end end
  return nil
end

local function projected_exchange(exchange)
  return {
    id = exchange.id,
    message_id = exchange.message_id,
    name = exchange.tool_name,
    status = exchange.status,
    arguments = domain.copy(exchange.call),
    result = domain.copy(exchange.result),
    error = domain.copy(exchange.error),
  }
end

local function projected_message(conversation, message)
  local chunks = domain.copy(message.chunks)
  local text = {}
  local reasoning = {}
  local tool_calls = {}
  local tool_results = {}
  for _, chunk in ipairs(chunks) do
    if chunk.kind == "text" and type(chunk.data) == "string" then text[#text + 1] = chunk.data end
    if chunk.kind == "reasoning" and type(chunk.data) == "string" then reasoning[#reasoning + 1] = chunk.data end
  end
  for _, exchange in ipairs(conversation.exchanges) do
    if exchange.message_id == message.id then
      tool_calls[#tool_calls + 1] = {
        id = exchange.id,
        name = exchange.tool_name,
        arguments = domain.copy(exchange.call),
        status = exchange.status,
      }
      if exchange.status == "result" or exchange.status == "error" then
        local result_content = domain.copy(exchange.result)
        if type(exchange.result) == "table" and type(exchange.result.text) == "string" then
          result_content = exchange.result.text
        end
        tool_results[#tool_results + 1] = {
          role = "tool",
          tool_call_id = exchange.id,
          name = exchange.tool_name,
          content = result_content,
          error = domain.copy(exchange.error),
        }
      end
    end
  end
  return {
    id = message.id,
    turn_id = message.turn_id,
    role = message.role,
    status = message.status,
    tool_call_id = message.tool_call_id,
    tool_name = message.tool_name,
    chunks = chunks,
    content = table.concat(text),
    text = table.concat(text),
    reasoning = table.concat(reasoning),
    tool_calls = tool_calls,
    tool_results = tool_results,
    terminal = domain.copy(message.terminal),
  }
end

local function context_messages(conversation)
  local messages = {}
  for _, message in ipairs(conversation.messages) do
    if message.status ~= "open" then
      local projected = projected_message(conversation, message)
      messages[#messages + 1] = projected
      for _, result in ipairs(projected.tool_results) do messages[#messages + 1] = result end
    end
  end
  return messages
end

local function public_compaction(compaction, include_checkpoint)
  if not compaction then return nil end
  local out = {
    request_id = compaction.request_id,
    status = compaction.status,
    history_cutoff = compaction.history_cutoff,
    compatibility = domain.copy(compaction.compatibility),
    error = domain.copy(compaction.error),
  }
  if include_checkpoint then out.checkpoint = domain.copy(compaction.checkpoint) end
  return out
end

function M.conversation(conversation)
  if not conversation then return nil end
  local messages = {}
  for _, message in ipairs(conversation.messages) do
    messages[#messages + 1] = projected_message(conversation, message)
  end
  local exchanges = {}
  for _, exchange in ipairs(conversation.exchanges) do exchanges[#exchanges + 1] = projected_exchange(exchange) end
  local compactions = {}
  for _, compaction in ipairs(conversation.compactions) do
    compactions[#compactions + 1] = public_compaction(compaction, false)
  end
  return {
    id = conversation.id,
    status = conversation.status,
    provenance = domain.copy(conversation.provenance),
    messages = messages,
    exchanges = exchanges,
    retries = domain.copy(conversation.retries),
    turns = domain.copy(conversation.turns),
    compactions = compactions,
    terminal = domain.copy(conversation.terminal),
    last_sequence = conversation.last_sequence,
  }
end

function M.context(conversation, compatibility)
  if not conversation then return nil end
  local all = context_messages(conversation)
  local selected = nil
  for index = #conversation.compactions, 1, -1 do
    local candidate = conversation.compactions[index]
    if candidate.status == "completed"
        and (compatibility == nil or domain.equal(candidate.compatibility or {}, compatibility)) then
      selected = candidate
      break
    end
  end
  local cutoff = selected and selected.history_cutoff or 0
  local tail = {}
  for index = cutoff + 1, #all do tail[#tail + 1] = domain.copy(all[index]) end
  return {
    messages = domain.copy(all),
    tail_messages = tail,
    history_length = #all,
    compaction = public_compaction(selected, true),
  }
end

function M.change(before, after, event)
  local kind = event.kind
  local change = {
    kind = kind, turn_id = event.turn_id, run_id = event.run_id,
    actor_id = event.actor_id,
  }
  if not change.turn_id then
    local message = event.message_id and find_by_id(after.messages, event.message_id) or nil
    if not message and event.exchange_id then
      local exchange = find_by_id(after.exchanges, event.exchange_id)
      message = exchange and find_by_id(after.messages, exchange.message_id) or nil
    end
    change.turn_id = message and message.turn_id or nil
  end
  if change.turn_id then
    local turn = find_by_id(after.turns, change.turn_id)
    change.run_id = change.run_id or (turn and turn.run_id or nil)
    change.actor_id = change.actor_id or (turn and turn.actor_id or nil)
  end
  if kind == "created" then
    change.kind = "conversation_created"
    change.conversation = M.conversation(after)
  elseif kind == "message_started" or kind == "message_completed" or kind == "message_interrupted" then
    change.message = projected_message(after, find_by_id(after.messages, event.message_id))
    if kind ~= "message_started" then
      local previous_length = before and #context_messages(before) or 0
      local current = context_messages(after)
      change.context_messages = {}
      for index = previous_length + 1, #current do change.context_messages[#change.context_messages + 1] = current[index] end
    end
  elseif kind == "content_chunk_appended" then
    change.message_id = event.message_id
    change.chunk = domain.copy(event.chunk)
  elseif kind:sub(1, 5) == "tool_" then
    change.exchange = projected_exchange(find_by_id(after.exchanges, event.exchange_id))
  elseif kind == "retry_started" then
    change.retry = domain.copy(after.retries[#after.retries])
  elseif kind:sub(1, 5) == "turn_" then
    local turn = find_by_id(after.turns, event.turn_id)
    change.kind = kind
    change.turn_id = turn.id
    change.run_id = turn.run_id
    change.actor_id = turn.actor_id
    change.message_start = turn.message_start
    change.message_end = turn.message_end
    change.watermark = turn.sequence_end or event.sequence
    change.terminal = domain.copy(turn.terminal)
  elseif kind:sub(1, #"context_compaction") == "context_compaction" then
    local compaction = find_by_id(after.compactions, event.request_id, "request_id")
    change.kind = kind:gsub("_requested$", "_pending")
    change.compaction = public_compaction(compaction, false)
    if kind == "context_compaction_requested" then change.context = M.context(after, nil) end
  elseif kind == "provenance_updated" then
    change.provenance = domain.copy(after.provenance)
  elseif kind:sub(1, 13) == "conversation_" then
    change.status = after.status
    change.terminal = domain.copy(after.terminal)
  end
  return change
end

return M
