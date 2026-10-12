local domain = require("libs.conversation-manager.domain")
local authored_prompt = require("libs.conversation-manager.authored_prompt")
local history_path = require("libs.conversation-manager.history_path")

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
    completion_delivery = exchange.completion_delivery,
  }
end

local function projected_message(conversation, message, include_provider_context)
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
  if content == "" and #structured == 1 then content = domain.copy(structured[1]) end
  local presentation = {
    role = message.role,
    authored_prompt = message.authored_prompt,
    text = table.concat(text),
    structured = structured,
  }
  local display_text = authored_prompt.display_text(presentation)
  local projected_content = include_provider_context
      and authored_prompt.provider_content(presentation, content) or content
  local projected = {
    id = message.id,
    history_id = domain.copy(message.history_id),
    turn_id = message.turn_id,
    submission_ids = domain.copy(message.submission_ids or {}),
    input_cause = message.input_cause,
    role = message.role,
    status = message.status,
    visibility = message.visibility or "transcript",
    tool_call_id = message.tool_call_id,
    tool_name = message.tool_name,
    chunks = chunks,
    content = projected_content,
    text = table.concat(text),
    display_text = display_text,
    authored_prompt = message.authored_prompt,
    reasoning = table.concat(reasoning),
    structured = structured,
    tool_calls = tool_calls,
    terminal = domain.copy(message.terminal),
  }
  if include_provider_context then
    projected.provider_context = domain.copy(message.provider_context)
    local empty_text = projected.content == nil
      or (type(projected.content) == "string" and projected.content:match("^%s*$") ~= nil)
    if projected.role == "assistant" and projected.status == "interrupted" and empty_text
        and #projected.tool_calls == 0 then
      projected.content = "[interrupted by user]"
    end
  end
  return projected
end

local function active_message(conversation, message)
  return history_path.is_prefix(message.history_id, conversation.head)
end

local function context_messages(conversation, include_provider_context)
  local messages = {}
  for _, message in ipairs(conversation.messages) do
    if active_message(conversation, message)
        and message.status ~= "open" and message.visibility ~= "discarded" then
      local projected = projected_message(conversation, message, include_provider_context)
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
    history_id = domain.copy(compaction.history_id),
    provider = compaction.provider,
    model = compaction.model,
    provider_options = domain.copy(compaction.provider_options),
    epoch_id = compaction.epoch_id, context_watermark = compaction.context_watermark,
    trigger = compaction.trigger, tool_call_id = compaction.tool_call_id,
    conversation_id = compaction.conversation_id,
    error = domain.copy(compaction.error),
  }
  if include_checkpoint then out.checkpoint = domain.copy(compaction.checkpoint) end
  return out
end

local function latest_response(conversation, provider, model)
  local cutoff = 0
  local latest
  for _, compaction in ipairs(conversation.compactions) do
    if compaction.status == "completed" and history_path.is_prefix(compaction.history_id, conversation.head) then
      if (compaction.terminal_sequence or 0) > cutoff then
        cutoff = compaction.terminal_sequence or 0
        latest = compaction.optional_usage and { sequence = cutoff, history_id = compaction.history_id,
          provider = compaction.provider, model = compaction.model, latest_response_usage = compaction.optional_usage } or nil
      end
    end
  end
  local function consider(usage)
    if (provider == nil or (usage.provider == provider and usage.model == model))
        and usage.sequence > cutoff and history_path.is_prefix(usage.history_id, conversation.head)
        and (not latest or usage.sequence > latest.sequence) then latest = usage end
  end
  for _, usage in ipairs(conversation.response_usages or {}) do consider(usage) end
  for _, message in ipairs(conversation.messages) do
    local terminal = message.terminal or {}
    local measured = require("libs.chat.context_usage").measurement(terminal.latest_response_usage)
    if message.status == "completed" and message.visibility ~= "discarded" and measured then
      local turn = message.turn_id and conversation.turn_by_id[message.turn_id]
      consider({ history_id = message.history_id, sequence = message.completed_sequence,
        provider = terminal.provider or (turn and turn.provenance.provider) or conversation.provenance.provider,
        model = terminal.model, latest_response_usage = measured })
    end
  end
  if latest and provider and (latest.provider ~= provider or latest.model ~= model) then return nil end
  return domain.copy(latest)
end

function M.conversation(conversation)
  if not conversation then return nil end
  local messages, active_message_ids, active_turn_ids = {}, {}, {}
  for _, message in ipairs(conversation.messages) do
    if active_message(conversation, message) then
      messages[#messages + 1] = projected_message(conversation, message)
      active_message_ids[message.id] = true
      if message.turn_id then active_turn_ids[message.turn_id] = true end
    end
  end
  local exchanges = {}
  for _, exchange in ipairs(conversation.exchanges) do
    if active_message_ids[exchange.message_id] then
      exchanges[#exchanges + 1] = projected_exchange(exchange)
    end
  end
  local turns = {}
  for _, turn in ipairs(conversation.turns) do
    if active_turn_ids[turn.id] then turns[#turns + 1] = domain.copy(turn) end
  end
  local compactions = {}
  for _, compaction in ipairs(conversation.compactions) do
    if history_path.is_prefix(compaction.history_id, conversation.head) then
      compactions[#compactions + 1] = public_compaction(compaction, false)
    end
  end
  return {
    id = conversation.id,
    status = conversation.status,
    provenance = domain.copy(conversation.provenance),
    messages = messages,
    exchanges = exchanges,
    retries = domain.copy(conversation.retries),
    turns = turns,
    compactions = compactions,
    response_usage = latest_response(conversation, conversation.provenance.provider, conversation.provenance.model),
    policy_state = domain.policy_state(conversation, conversation.provenance.provider, conversation.provenance.model),
    pending_edit = domain.copy(conversation.pending_edit),
    history_head = domain.copy(conversation.head),
    terminal = domain.copy(conversation.terminal),
    last_sequence = conversation.last_sequence,
    watermark = conversation.last_sequence,
  }
end

function M.context(conversation, options)
  if not conversation then return nil end
  options = domain.copy(options or {})
  if options.provider == nil then
    options.provider, options.model = conversation.provenance.provider, conversation.provenance.model
  end
  local all = context_messages(conversation, true)
  if options.history_cutoff ~= nil then
    local prefix = {}
    for index = 1, options.history_cutoff do prefix[index] = all[index] end
    all = prefix
  end
  local selected = nil
  for index = #conversation.compactions, 1, -1 do
    local candidate = conversation.compactions[index]
    if candidate.status == "completed"
        and history_path.is_prefix(candidate.history_id, conversation.head)
        and candidate.history_cutoff <= #all
        and (options.provider == nil or (candidate.provider == options.provider and candidate.model == options.model)) then
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
    history_id = domain.copy(conversation.head),
    policy_state = domain.policy_state(conversation, options.provider, options.model),
    latest_response_usage = (function()
      local usage = latest_response(conversation, options.provider, options.model)
      return usage and usage.latest_response_usage or nil
    end)(),
    pending_compaction = (function()
      for _, op in ipairs(conversation.compactions) do
        if op.status == "pending" then return public_compaction(op, false) end
      end
    end)(),
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
  elseif kind == "rewind_committed" then
    change.projection = M.conversation(after)
    change.context = M.context(after)
    change.pending_edit = domain.copy(after.pending_edit)
    change.history_head = domain.copy(after.head)
  elseif kind:sub(1, #"context_compaction") == "context_compaction" then
    local compaction = after.compaction_by_id[event.request_id]
    change.kind = kind:gsub("_requested$", "_pending")
    change.compaction = public_compaction(compaction, false)
    if kind == "context_compaction_requested" then
      local context = M.context(after, { history_cutoff = compaction.history_cutoff,
        provider = compaction.provider, model = compaction.model })
      context.watermark = compaction.context_watermark
      context.pending_compaction = nil
      local request = {}
      for _, field in ipairs({ "request_id", "conversation_id", "epoch_id", "history_id", "context_watermark",
          "history_cutoff", "provider", "model", "provider_options", "trigger", "tool_call_id" }) do
        request[field] = domain.copy(compaction[field])
      end
      change.context = context
      change.input = { request = request, context = domain.copy(context) }
    else
      change.context = M.context(after)
    end
    change.request_id = event.request_id
  elseif kind:sub(1, #"context_notice_") == "context_notice_" then
    change.notice = domain.copy(after.notice_by_id[event.notice_id])
    change.request_id = event.request_id
    change.policy_state = domain.policy_state(after, event.provider, event.model)
    if kind == "context_notice_appended" then
      change.message = projected_message(after, after.message_by_id[event.message_id], true)
      change.context_messages = { domain.copy(change.message) }
    end
  elseif kind == "response_usage_recorded" then
    change.response_usage = latest_response(after, after.provenance.provider, after.provenance.model)
  elseif kind == "provenance_updated" then
    change.provenance = domain.copy(after.provenance)
  elseif kind:sub(1, 13) == "conversation_" then
    change.status = after.status
    change.terminal = domain.copy(after.terminal)
  end
  return change
end

return M
