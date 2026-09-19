local M = {}
local json_data = require("core.json_data")
local history_path = require("libs.conversation-manager.history_path")

-- Within this domain, the fold is the sole transcript authority: events are
-- immutable facts, while lookup maps are derived indexes removed from the
-- neutral read model.
local terminal_conversation = { completed = true, interrupted = true, failed = true }
local chunk_kinds = { text = true, reasoning = true, structured = true, native = true }
local roles = { system = true, user = true, assistant = true, tool = true }
local completion_deliveries = { sync = true, async = true }
-- A message's transcript disposition. "transcript" is ordinary conversation;
-- "diagnostic" is model context that no surface renders as conversation (a
-- rejected structured-output attempt and its correction prompt). "discarded"
-- is an abandoned provider attempt retained only for audit: it appears in the
-- conversation projection but never in model context or a surface.
local visibilities = { transcript = true, diagnostic = true, discarded = true }

local copy = json_data.copy
M.copy = copy

local function equal(a, b, seen)
  if type(a) ~= type(b) then return false end
  if type(a) ~= "table" then return a == b end
  seen = seen or {}
  if seen[a] == b then return true end
  seen[a] = b
  for key, value in pairs(a) do if not equal(value, b[key], seen) then return false end end
  for key in pairs(b) do if a[key] == nil then return false end end
  return true
end
M.equal = equal

local function err(code, context)
  return nil, { code = code, context = context or {} }
end
M.error = err

local function nonempty(value)
  return type(value) == "string" and value ~= ""
end

local function require_id(event, field)
  if nonempty(event[field]) then return true end
  return err("invalid_" .. field, { kind = event.kind, field = field, value = event[field] })
end

local function find_message(conversation, id)
  return conversation.message_by_id[id]
end

local function find_exchange(conversation, id)
  return conversation.exchange_by_id[id]
end

local function find_turn(conversation, id)
  return conversation.turn_by_id[id]
end

local function require_live(conversation, event)
  if terminal_conversation[conversation.status] then
    return err("conversation_terminal", { conversation_id = conversation.id, status = conversation.status, kind = event.kind })
  end
  return true
end

local handlers = {}

handlers.created = function(conversation, event)
  if conversation then return err("created_more_than_once", { conversation_id = event.conversation_id }) end
  if event.sequence ~= 1 then return err("created_not_first", { conversation_id = event.conversation_id, sequence = event.sequence }) end
  if event.provenance ~= nil and type(event.provenance) ~= "table" then return err("invalid_provenance", { kind = event.kind }) end
  return {
    id = event.conversation_id,
    status = "active",
    provenance = copy(event.provenance or {}),
    messages = {}, message_by_id = {}, history_by_key = {},
    history_child_max = {}, head = {}, pending_edit = nil,
    exchanges = {}, exchange_by_id = {}, exchange_by_tool_call_id = {},
    retries = {}, retry_by_id = {},
    turns = {}, turn_by_id = {},
    compactions = {}, compaction_by_id = {},
    open_messages = 0, open_exchanges = 0,
    last_sequence = 0,
  }
end

handlers.provenance_updated = function(c, event)
  local ok, e = require_live(c, event); if not ok then return nil, e end
  if type(event.provenance) ~= "table" then return err("invalid_provenance", { kind = event.kind }) end
  for key, value in pairs(event.provenance) do c.provenance[key] = copy(value) end
  return c
end

handlers.message_started = function(c, event)
  local ok, e = require_live(c, event); if not ok then return nil, e end
  ok, e = require_id(event, "message_id"); if not ok then return nil, e end
  if c.message_by_id[event.message_id] then return err("message_id_conflict", { message_id = event.message_id }) end
  if not roles[event.role] then return err("invalid_role", { message_id = event.message_id, role = event.role }) end
  local tool_exchange = nil
  if event.role == "tool" then
    if not nonempty(event.tool_call_id) then return err("invalid_tool_call_id", { message_id = event.message_id }) end
    tool_exchange = c.exchange_by_tool_call_id[event.tool_call_id]
    if not tool_exchange then
      return err("tool_call_not_found", { message_id = event.message_id, tool_call_id = event.tool_call_id })
    end
  end
  local turn = event.turn_id and c.turn_by_id[event.turn_id] or nil
  if event.turn_id and not turn then return err("turn_not_found", { turn_id = event.turn_id }) end
  if turn and turn.status ~= "open" then return err("turn_not_open", { turn_id = turn.id, status = turn.status }) end
  if event.visibility ~= nil and not visibilities[event.visibility] then
    return err("invalid_visibility", { message_id = event.message_id, visibility = event.visibility })
  end
  local parent = history_path.copy(c.head)
  local allocated, allocation_error = history_path.allocate(parent, c.history_child_max)
  if not allocated then return err(allocation_error, { message_id = event.message_id }) end
  if event.history_id ~= nil and not equal(event.history_id, allocated) then
    return err("invalid_history_id", {
      message_id = event.message_id,
      expected = allocated,
      actual = copy(event.history_id),
    })
  end
  event.history_id = allocated
  local parent_key = history_path.key(parent)
  c.history_child_max[parent_key] = allocated[#allocated]
  local message = {
    id = event.message_id, history_id = copy(allocated), role = event.role, status = "open",
    visibility = event.visibility or "transcript",
    turn_id = event.turn_id, tool_call_id = event.tool_call_id,
    submission_ids = copy(event.submission_ids or {}),
    input_cause = event.input_cause,
    tool_name = event.tool_name or (tool_exchange and tool_exchange.tool_name),
    chunks = {}, attempts = {}, exchange_ids = {},
  }
  c.messages[#c.messages + 1] = message; c.message_by_id[message.id] = message
  c.history_by_key[history_path.key(message.history_id)] = message
  c.head = copy(message.history_id)
  if message.role == "user" and message.visibility == "transcript" then
    c.pending_edit = nil
  end
  c.open_messages = c.open_messages + 1
  if turn then turn.open_messages = turn.open_messages + 1 end
  return c
end

handlers.content_chunk_appended = function(c, event)
  local ok, e = require_live(c, event); if not ok then return nil, e end
  local message = find_message(c, event.message_id)
  if not message then return err("message_not_found", { message_id = event.message_id }) end
  if message.status ~= "open" then return err("message_not_open", { message_id = event.message_id, status = message.status }) end
  if type(event.chunk) ~= "table" or not chunk_kinds[event.chunk.kind] then
    return err("invalid_content_chunk", { message_id = event.message_id, chunk_kind = type(event.chunk) == "table" and event.chunk.kind or nil })
  end
  message.chunks[#message.chunks + 1] = copy(event.chunk)
  return c
end

handlers.tool_exchange_started = function(c, event)
  local ok, e = require_live(c, event); if not ok then return nil, e end
  ok, e = require_id(event, "exchange_id"); if not ok then return nil, e end
  if c.exchange_by_id[event.exchange_id] then return err("exchange_id_conflict", { exchange_id = event.exchange_id }) end
  local message = find_message(c, event.message_id)
  if not message then return err("message_not_found", { message_id = event.message_id }) end
  if message.status ~= "open" then return err("message_not_open", { message_id = event.message_id, status = message.status }) end
  if event.tool_call_id ~= nil and not nonempty(event.tool_call_id) then
    return err("invalid_tool_call_id", { exchange_id = event.exchange_id, tool_call_id = event.tool_call_id })
  end
  local exchange = {
    id = event.exchange_id,
    message_id = event.message_id,
    tool_call_id = event.tool_call_id,
    tool_name = event.tool_name,
    status = "call_open",
    call_chunks = {},
  }
  c.exchanges[#c.exchanges + 1] = exchange; c.exchange_by_id[exchange.id] = exchange
  message.exchange_ids[#message.exchange_ids + 1] = exchange.id
  c.open_exchanges = c.open_exchanges + 1
  return c
end

handlers.tool_call_fragment_appended = function(c, event)
  local ok, e = require_live(c, event); if not ok then return nil, e end
  local exchange = find_exchange(c, event.exchange_id)
  if not exchange then return err("exchange_not_found", { exchange_id = event.exchange_id }) end
  if exchange.status ~= "call_open" then return err("tool_call_not_open", { exchange_id = event.exchange_id, status = exchange.status }) end
  local message = find_message(c, exchange.message_id)
  if message.status ~= "open" then return err("message_not_open", { message_id = message.id, status = message.status }) end
  exchange.call_chunks[#exchange.call_chunks + 1] = copy(event.fragment)
  return c
end

handlers.tool_call_completed = function(c, event)
  local ok, e = require_live(c, event); if not ok then return nil, e end
  local exchange = find_exchange(c, event.exchange_id)
  if not exchange then return err("exchange_not_found", { exchange_id = event.exchange_id }) end
  if exchange.status ~= "call_open" then return err("tool_call_not_open", { exchange_id = event.exchange_id, status = exchange.status }) end
  if type(event.call) ~= "table" then return err("invalid_tool_call", { exchange_id = event.exchange_id }) end
  local external_id = event.call.id or event.tool_call_id or exchange.tool_call_id
  if not nonempty(external_id) then return err("invalid_tool_call_id", { exchange_id = event.exchange_id }) end
  local name = event.call.name or event.tool_name or exchange.tool_name
  if not nonempty(name) then return err("invalid_tool_name", { exchange_id = event.exchange_id }) end
  if event.call.arguments == nil then return err("invalid_tool_arguments", { exchange_id = event.exchange_id }) end
  local existing = c.exchange_by_tool_call_id[external_id]
  if existing and existing ~= exchange then
    local existing_message = find_message(c, existing.message_id)
    if not existing_message or existing_message.visibility ~= "discarded" then
      return err("tool_call_id_conflict", { exchange_id = event.exchange_id, tool_call_id = external_id })
    end
  end
  exchange.status = "call_completed"
  exchange.tool_call_id = external_id
  exchange.tool_name = name
  exchange.arguments = copy(event.call.arguments)
  c.exchange_by_tool_call_id[external_id] = exchange
  return c
end

local function finish_exchange(c, event, status, field)
  local ok, e = require_live(c, event); if not ok then return nil, e end
  local exchange = find_exchange(c, event.exchange_id)
  if not exchange then return err("exchange_not_found", { exchange_id = event.exchange_id }) end
  if exchange.status ~= "call_completed" then
    local terminal = exchange.status == "result" or exchange.status == "error"
    local code = terminal and "tool_exchange_terminal" or "tool_call_incomplete"
    return err(code, { exchange_id = event.exchange_id, status = exchange.status })
  end
  if event.completion_delivery ~= nil
      and not completion_deliveries[event.completion_delivery] then
    return err("invalid_completion_delivery", {
      exchange_id = event.exchange_id,
      completion_delivery = event.completion_delivery,
    })
  end
  exchange.status = status
  exchange[field] = copy(event[field])
  exchange.completion_delivery = event.completion_delivery
  c.open_exchanges = c.open_exchanges - 1
  return c
end
handlers.tool_result_recorded = function(c, e) return finish_exchange(c, e, "result", "result") end
handlers.tool_error_recorded = function(c, e) return finish_exchange(c, e, "error", "error") end

-- Visibility may only narrow at a message's terminal fact: content already
-- streamed to a surface can be retracted, but nothing that was recorded as
-- diagnostic may be promoted into the transcript afterwards.
local function settle_visibility(message, event)
  if event.visibility == nil then return true end
  if not visibilities[event.visibility] then
    return err("invalid_visibility", { message_id = message.id, visibility = event.visibility })
  end
  if message.visibility ~= "transcript" and event.visibility == "transcript" then
    return err("invalid_visibility_promotion", { message_id = message.id })
  end
  message.visibility = event.visibility
  return true
end

handlers.message_completed = function(c, event)
  local ok, e = require_live(c, event); if not ok then return nil, e end
  local message = find_message(c, event.message_id)
  if not message then return err("message_not_found", { message_id = event.message_id }) end
  if message.status ~= "open" then return err("message_not_open", { message_id = event.message_id, status = message.status }) end
  ok, e = settle_visibility(message, event); if not ok then return nil, e end
  for _, exchange_id in ipairs(message.exchange_ids) do
    local exchange = c.exchange_by_id[exchange_id]
    if exchange.status == "call_open" then
      return err("tool_call_incomplete", { exchange_id = exchange.id, message_id = message.id })
    end
  end
  local provider_context = event.provider_context
  if provider_context ~= nil
      and (type(provider_context) ~= "table" or not nonempty(provider_context.provider)
        or not nonempty(provider_context.format) or type(provider_context.artifact) ~= "table"
        or (provider_context.model ~= nil and not nonempty(provider_context.model))) then
    return err("invalid_provider_context", { message_id = message.id })
  end
  message.status = "completed"
  c.open_messages = c.open_messages - 1
  local turn = message.turn_id and find_turn(c, message.turn_id) or nil
  if turn then turn.open_messages = turn.open_messages - 1 end
  message.completion = copy(event.completion or {})
  if provider_context ~= nil then message.provider_context = copy(provider_context) end
  message.terminal = {
    model = event.model,
    duration_ms = event.duration_ms,
    usage = copy(event.usage),
    finish_reason = event.finish_reason,
  }
  return c
end

handlers.message_interrupted = function(c, event)
  local ok, e = require_live(c, event); if not ok then return nil, e end
  local message = find_message(c, event.message_id)
  if not message then return err("message_not_found", { message_id = event.message_id }) end
  if message.status ~= "open" then return err("message_not_open", { message_id = event.message_id, status = message.status }) end
  ok, e = settle_visibility(message, event); if not ok then return nil, e end
  message.status = "interrupted"
  c.open_messages = c.open_messages - 1
  local turn = message.turn_id and find_turn(c, message.turn_id) or nil
  if turn then turn.open_messages = turn.open_messages - 1 end
  message.terminal = copy(event.detail or event.terminal or {})
  return c
end

handlers.retry_started = function(c, event)
  local ok, e = require_live(c, event); if not ok then return nil, e end
  ok, e = require_id(event, "retry_id"); if not ok then return nil, e end
  if c.retry_by_id[event.retry_id] then return err("retry_id_conflict", { retry_id = event.retry_id }) end
  local message = find_message(c, event.message_id)
  if event.message_id ~= nil and not message then return err("message_not_found", { message_id = event.message_id }) end
  if message and message.status ~= "open" then return err("message_not_open", { message_id = event.message_id, status = message.status }) end
  local retry = { id = event.retry_id, message_id = event.message_id, reason = copy(event.reason), provenance = copy(event.provenance or {}) }
  c.retries[#c.retries + 1] = retry
  c.retry_by_id[retry.id] = retry
  if message then message.attempts[#message.attempts + 1] = retry end
  return c
end

handlers.turn_started = function(c, event)
  local ok, e = require_live(c, event); if not ok then return nil, e end
  ok, e = require_id(event, "turn_id"); if not ok then return nil, e end
  ok, e = require_id(event, "run_id"); if not ok then return nil, e end
  if c.turn_by_id[event.turn_id] then return err("turn_id_conflict", { turn_id = event.turn_id }) end
  local turn = {
    id = event.turn_id,
    run_id = event.run_id,
    actor_id = event.actor_id,
    status = "open",
    provenance = copy(event.provenance or {}),
    message_start = #c.messages + 1,
    sequence_start = event.sequence,
    open_messages = 0,
  }
  c.turns[#c.turns + 1] = turn
  c.turn_by_id[turn.id] = turn
  return c
end

local function finish_turn(status)
  return function(c, event)
    local ok, e = require_live(c, event); if not ok then return nil, e end
    local turn = c.turn_by_id[event.turn_id]
    if not turn then return err("turn_not_found", { turn_id = event.turn_id }) end
    if turn.status ~= "open" then return err("turn_not_open", { turn_id = turn.id, status = turn.status }) end
    if event.run_id ~= turn.run_id then
      return err("turn_run_mismatch", { turn_id = turn.id, expected = turn.run_id, actual = event.run_id })
    end
    if turn.open_messages ~= 0 then
      return err("open_message_at_turn_terminal", { turn_id = turn.id, open_messages = turn.open_messages })
    end
    turn.status = status
    turn.message_end = #c.messages
    turn.sequence_end = event.sequence
    turn.terminal = copy(event.detail or event.terminal or {})
    return c
  end
end
handlers.turn_completed = finish_turn("completed")
handlers.turn_failed = finish_turn("failed")
handlers.turn_interrupted = finish_turn("interrupted")

handlers.rewind_committed = function(c, event)
  local ok, e = require_live(c, event); if not ok then return nil, e end
  local open_turns = 0
  for _, turn in ipairs(c.turns) do
    if turn.status == "open" then open_turns = open_turns + 1 end
  end
  if c.open_messages ~= 0 or c.open_exchanges ~= 0 or open_turns ~= 0 then
    return err("rewind_while_active", {
      open_messages = c.open_messages,
      open_exchanges = c.open_exchanges,
      open_turns = open_turns,
    })
  end
  if not history_path.valid(event.target_history_id) then
    return err("invalid_rewind_target", { target_history_id = copy(event.target_history_id) })
  end
  if not history_path.valid(event.expected_head, true) or not equal(event.expected_head, c.head) then
    return err("stale_rewind_target", { expected_head = copy(event.expected_head), actual_head = copy(c.head) })
  end
  local message = c.history_by_key[history_path.key(event.target_history_id)]
  if not message or message.role ~= "user" or message.visibility ~= "transcript"
      or message.status ~= "completed" or not history_path.is_prefix(message.history_id, c.head) then
    return err("invalid_rewind_target", { target_history_id = copy(event.target_history_id) })
  end
  local text = {}
  for _, chunk in ipairs(message.chunks or {}) do
    if chunk.kind == "text" and type(chunk.data) == "string" then text[#text + 1] = chunk.data end
  end
  c.head = history_path.parent(message.history_id)
  c.pending_edit = {
    source_history_id = copy(message.history_id),
    text = table.concat(text),
  }
  return c
end

handlers.context_compaction_requested = function(c, event)
  local ok, e = require_live(c, event); if not ok then return nil, e end
  ok, e = require_id(event, "request_id"); if not ok then return nil, e end
  ok, e = require_id(event, "provider"); if not ok then return nil, e end
  if c.compaction_by_id[event.request_id] then
    return err("compaction_request_conflict", { request_id = event.request_id })
  end
  if type(event.history_cutoff) ~= "number" or event.history_cutoff % 1 ~= 0 or event.history_cutoff < 0 then
    return err("invalid_history_cutoff", { request_id = event.request_id, history_cutoff = event.history_cutoff })
  end
  local compaction = {
    request_id = event.request_id,
    status = "pending",
    history_cutoff = event.history_cutoff,
    history_id = copy(c.head),
    requested_sequence = event.sequence,
    provider = event.provider,
    model = event.model,
    provider_options = copy(event.provider_options),
  }
  c.compactions[#c.compactions + 1] = compaction
  c.compaction_by_id[compaction.request_id] = compaction
  return c
end

local function finish_compaction(status)
  return function(c, event)
    local ok, e = require_live(c, event); if not ok then return nil, e end
    local compaction = c.compaction_by_id[event.request_id]
    if not compaction then return err("compaction_request_not_found", { request_id = event.request_id }) end
    if compaction.status ~= "pending" then
      return err("compaction_request_terminal", { request_id = event.request_id, status = compaction.status })
    end
    if status == "completed" then
      if event.checkpoint == nil then return err("missing_checkpoint", { request_id = event.request_id }) end
      compaction.checkpoint = copy(event.checkpoint)
    else
      compaction.error = copy(event.error)
    end
    compaction.status = status
    compaction.terminal_sequence = event.sequence
    return c
  end
end
handlers.context_compaction_completed = finish_compaction("completed")
handlers.context_compaction_failed = finish_compaction("failed")

local function terminate(status)
  return function(c, event)
    local ok, e = require_live(c, event); if not ok then return nil, e end
    if status == "completed" then
      if c.open_messages ~= 0 then return err("open_message_at_completion", { open_messages = c.open_messages }) end
      if c.open_exchanges ~= 0 then return err("open_exchange_at_completion", { open_exchanges = c.open_exchanges }) end
    end
    c.status = status; c.terminal = copy(event.detail or {})
    return c
  end
end
handlers.conversation_completed = terminate("completed")
handlers.conversation_interrupted = terminate("interrupted")
handlers.conversation_failed = terminate("failed")

handlers.active_selected = function(c, event)
  return c
end

function M.fold(conversation, event)
  if type(event) ~= "table" then return err("invalid_event", { value_type = type(event) }) end
  if not nonempty(event.event_id) then return err("invalid_event_id", { kind = event.kind }) end
  if not nonempty(event.conversation_id) then return err("invalid_conversation_id", { kind = event.kind }) end
  if type(event.sequence) ~= "number" or event.sequence % 1 ~= 0 or event.sequence < 1 then return err("invalid_sequence", { sequence = event.sequence }) end
  local handler = handlers[event.kind]
  if not handler then return err("unknown_event_kind", { kind = event.kind }) end
  if not conversation and event.kind ~= "created" then return err("created_required", { conversation_id = event.conversation_id, kind = event.kind }) end
  if conversation then
    if event.conversation_id ~= conversation.id then return err("conversation_id_mismatch", { expected = conversation.id, actual = event.conversation_id }) end
    if event.sequence ~= conversation.last_sequence + 1 then return err("noncontiguous_sequence", { conversation_id = conversation.id, expected = conversation.last_sequence + 1, actual = event.sequence }) end
  end
  if conversation and event.turn_id and event.kind ~= "turn_started" then
    local turn = conversation.turn_by_id[event.turn_id]
    if not turn then return err("turn_not_found", { turn_id = event.turn_id }) end
    if event.run_id ~= nil and event.run_id ~= turn.run_id then
      return err("turn_run_mismatch", { turn_id = turn.id, expected = turn.run_id, actual = event.run_id })
    end
    if event.actor_id ~= nil and turn.actor_id ~= nil and event.actor_id ~= turn.actor_id then
      return err("turn_actor_mismatch", { turn_id = turn.id, expected = turn.actor_id, actual = event.actor_id })
    end
  end
  local folded, e = handler(conversation, event); if not folded then return nil, e end
  folded.last_sequence = event.sequence
  return folded
end

function M.read_model(conversation)
  if not conversation then return nil end
  local out = copy(conversation)
  out.message_by_id = nil; out.history_by_key = nil; out.history_child_max = nil
  out.exchange_by_id = nil; out.exchange_by_tool_call_id = nil
  out.retry_by_id = nil; out.turn_by_id = nil; out.compaction_by_id = nil
  out.open_messages = nil; out.open_exchanges = nil
  for _, message in ipairs(out.messages) do message.exchange_ids = nil end
  for _, turn in ipairs(out.turns) do turn.open_messages = nil end
  return out
end

return M
