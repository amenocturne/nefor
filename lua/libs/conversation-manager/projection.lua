local domain = require("libs.conversation-manager.domain")
local display = require("libs.conversation-manager.display")

local M = {}

local function projected_exchange(exchange)
  return {
    id = exchange.id,
    tool_call_id = exchange.tool_call_id,
    message_id = exchange.message_id,
    name = exchange.tool_name,
    status = exchange.status,
    arguments = domain.copy(exchange.arguments),
    result = domain.copy(exchange.result),
    error = domain.copy(exchange.error),
  }
end

local function projected_message(conversation, message)
  local chunks = domain.copy(message.chunks)
  local text = {}
  local reasoning = {}
  local structured = {}
  local tool_calls = {}
  for _, chunk in ipairs(chunks) do
    if chunk.kind == "text" and type(chunk.data) == "string" then text[#text + 1] = chunk.data end
    if chunk.kind == "reasoning" and type(chunk.data) == "string" then reasoning[#reasoning + 1] = chunk.data end
    if chunk.kind == "structured" then structured[#structured + 1] = domain.copy(chunk.data) end
  end
  for _, exchange_id in ipairs(message.exchange_ids) do
    local exchange = conversation.exchange_by_id[exchange_id]
    if exchange then
      tool_calls[#tool_calls + 1] = {
        id = exchange.tool_call_id,
        name = exchange.tool_name,
        arguments = domain.copy(exchange.arguments),
        status = exchange.status,
      }
    end
  end
  local content = table.concat(text)
  local display_text = content
  if display_text == "" and #structured == 1 then
    display_text = display.structured_text(structured[1]) or ""
  end
  if content == "" and #structured == 1 then content = domain.copy(structured[1]) end
  return {
    id = message.id,
    turn_id = message.turn_id,
    submission_ids = domain.copy(message.submission_ids or {}),
    role = message.role,
    status = message.status,
    tool_call_id = message.tool_call_id,
    tool_name = message.tool_name,
    chunks = chunks,
    content = content,
    text = table.concat(text),
    display_text = display_text,
    reasoning = table.concat(reasoning),
    structured = structured,
    tool_calls = tool_calls,
    terminal = domain.copy(message.terminal),
  }
end

local function context_messages(conversation)
  local messages = {}
  for _, message in ipairs(conversation.messages) do
    if message.status ~= "open" then
      local projected = projected_message(conversation, message)
      messages[#messages + 1] = projected
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
    provider = compaction.provider,
    model = compaction.model,
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
    watermark = conversation.last_sequence,
  }
end

function M.context(conversation)
  if not conversation then return nil end
  local all = context_messages(conversation)
  local selected = nil
  for index = #conversation.compactions, 1, -1 do
    local candidate = conversation.compactions[index]
    if candidate.status == "completed" then
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
    watermark = conversation.last_sequence,
  }
end

function M.change(after, event)
  local kind = event.kind
  local change = {
    kind = kind, turn_id = event.turn_id, run_id = event.run_id,
    actor_id = event.actor_id,
  }
  if not change.turn_id then
    local message = event.message_id and after.message_by_id[event.message_id] or nil
    if not message and event.exchange_id then
      local exchange = after.exchange_by_id[event.exchange_id]
      message = exchange and after.message_by_id[exchange.message_id] or nil
    end
    change.turn_id = message and message.turn_id or nil
  end
  if change.turn_id then
    local turn = after.turn_by_id[change.turn_id]
    change.run_id = change.run_id or (turn and turn.run_id or nil)
    change.actor_id = change.actor_id or (turn and turn.actor_id or nil)
  end
  if kind == "created" then
    change.kind = "conversation_created"
    change.conversation = M.conversation(after)
  elseif kind == "message_started" or kind == "message_completed" or kind == "message_interrupted" then
    change.message = projected_message(after, after.message_by_id[event.message_id])
    if kind ~= "message_started" then
      change.context_messages = { domain.copy(change.message) }
    end
  elseif kind == "content_chunk_appended" then
    change.message_id = event.message_id
    change.chunk = domain.copy(event.chunk)
  elseif kind:sub(1, 5) == "tool_" then
    change.exchange = projected_exchange(after.exchange_by_id[event.exchange_id])
  elseif kind == "retry_started" then
    change.retry = domain.copy(after.retries[#after.retries])
  elseif kind == "turn_started" then
    local turn = after.turn_by_id[event.turn_id]
    change.provenance = domain.copy(turn and turn.provenance or {})
  elseif kind:sub(1, 5) == "turn_" then
    local turn = after.turn_by_id[event.turn_id]
    change.kind = kind
    change.turn_id = turn.id
    change.run_id = turn.run_id
    change.actor_id = turn.actor_id
    change.message_start = turn.message_start
    change.message_end = turn.message_end
    change.watermark = turn.sequence_end or event.sequence
    change.terminal = domain.copy(turn.terminal)
  elseif kind:sub(1, #"context_compaction") == "context_compaction" then
    local compaction = after.compaction_by_id[event.request_id]
    change.kind = kind:gsub("_requested$", "_pending")
    change.compaction = public_compaction(compaction, false)
    if kind == "context_compaction_requested" then change.context = M.context(after) end
  elseif kind == "provenance_updated" then
    change.provenance = domain.copy(after.provenance)
  elseif kind:sub(1, 13) == "conversation_" then
    change.status = after.status
    change.terminal = domain.copy(after.terminal)
  end
  return change
end

return M
