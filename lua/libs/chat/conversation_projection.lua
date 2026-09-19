local display = require("libs.conversation-manager.display")

local M = {}

local function copy_map(source)
  local out = {}
  for key, value in pairs(source or {}) do out[key] = value end
  return out
end

local function initial(active_conversation_id)
  return {
    active_conversation_id = active_conversation_id,
    messages = {},
    exchanges = {},
    turn_text = {},
    user_prompts = {},
    history_head = {},
    pending_edit = nil,
  }
end

function M.new()
  return initial(nil)
end

local function action(actions, kind, fields)
  local value = { kind = kind }
  for key, item in pairs(fields or {}) do value[key] = item end
  actions[#actions + 1] = value
end

local function terminal_text(terminal)
  if type(terminal) ~= "table" then return nil end
  if type(terminal.error) == "string" then return terminal.error end
  if type(terminal.message) == "string" then return terminal.message end
  if type(terminal.reason) == "string" then return terminal.reason end
  return nil
end

-- Hidden messages never become conversation on a surface. Diagnostic messages
-- remain model context; discarded transport attempts are audit-only. Either may
-- be classified only after content streamed, so the surface must retract it.
local function hidden(message)
  return type(message) == "table"
    and (message.visibility == "diagnostic" or message.visibility == "discarded")
end

local function record_message(state, message)
  if type(message) ~= "table" or type(message.id) ~= "string" then return end
  local previous = state.messages[message.id] or {}
  state.messages[message.id] = {
    role = message.role,
    turn_id = message.turn_id,
    display_text = previous.display_text,
    visibility = message.visibility or previous.visibility or "transcript",
    streamed = previous.streamed == true,
  }
  if type(message.history_id) == "table" then state.history_head = message.history_id end
end

-- Returns true when a terminal fact hides a message from the surface, retracting
-- anything already streamed for it.
local function settle_hidden(state, actions, message)
  local recorded = state.messages[type(message) == "table" and message.id or ""]
  local was_streamed = type(recorded) == "table" and recorded.streamed == true
  record_message(state, message)
  if not hidden(message) then return false end
  if was_streamed then
    action(actions, "message_discarded", {
      message_id = message.id,
      turn_id = message.turn_id,
    })
  end
  return true
end

local function visible_text(message)
  if type(message) ~= "table" then return "" end
  if type(message.display_text) == "string" then return message.display_text end
  if type(message.text) == "string" and message.text ~= "" then return message.text end
  if type(message.structured) == "table" and #message.structured == 1 then
    return display.structured_text(message.structured[1]) or ""
  end
  return ""
end

local function exchange_arguments(exchange)
  local arguments = type(exchange) == "table" and exchange.arguments or nil
  if type(arguments) == "table" then
    return arguments.arguments or arguments.args or arguments
  end
  return arguments or {}
end

local function remember_user_prompt(state, message, text)
  if type(message.history_id) ~= "table" or message.status ~= "completed"
      or (message.visibility or "transcript") ~= "transcript" then return end
  state.user_prompts[#state.user_prompts + 1] = {
    history_id = message.history_id,
    text = text,
  }
end

local function message_completed(state, actions, message)
  if settle_hidden(state, actions, message) then return end
  if type(message) ~= "table" then return end
  if message.role == "user" and message.input_cause ~= "internal_async_completion" then
    local text = visible_text(message)
    local recorded = state.messages[message.id]
    if text == "" and type(recorded) == "table"
        and type(recorded.display_text) == "string" then text = recorded.display_text end
    remember_user_prompt(state, message, text)
    action(actions, "message", {
      role = message.role,
      text = text,
      message_id = message.id,
      turn_id = message.turn_id,
      submission_ids = message.submission_ids,
    })
  elseif message.role == "assistant" then
    action(actions, "assistant_completed", {
      text = message.text or "",
      message_id = message.id,
      turn_id = message.turn_id,
      terminal = message.terminal,
    })
  end
end

local function snapshot_actions(state, projection, actions)
  state.messages = {}
  state.exchanges = {}
  state.turn_text = {}
  state.user_prompts = {}
  state.history_head = projection.history_head or {}
  state.pending_edit = projection.pending_edit
  action(actions, "snapshot_reset", {})

  local exchange_by_id = {}
  local exchange_by_call_id = {}
  local tool_message_by_call_id = {}
  for _, exchange in ipairs(projection.exchanges or {}) do
    exchange_by_id[exchange.id] = exchange
    if type(exchange.tool_call_id) == "string" then
      exchange_by_call_id[exchange.tool_call_id] = exchange
    end
  end
  for _, message in ipairs(projection.messages or {}) do
    if message.role == "tool" and type(message.tool_call_id) == "string" then
      tool_message_by_call_id[message.tool_call_id] = true
    end
  end

  local function complete_exchange(exchange)
    if type(exchange) ~= "table" then return end
    if exchange.status ~= "result" and exchange.status ~= "error" then return end
    local is_error = exchange.status == "error"
    local output = exchange.result
    if is_error then output = exchange.error end
    action(actions, "tool_completed", {
      exchange_id = exchange.id,
      output = output,
      error = is_error,
      completion_delivery = exchange.completion_delivery,
    })
  end

  for _, message in ipairs(projection.messages or {}) do
    record_message(state, message)
    if hidden(message) then
      -- Hidden messages never replay into the transcript.
    elseif message.role == "assistant" then
      if type(message.reasoning) == "string" and message.reasoning ~= "" then
        action(actions, "reasoning_delta", {
          text = message.reasoning, message_id = message.id, turn_id = message.turn_id,
        })
      end
      if type(message.text) == "string" and message.text ~= "" then
        action(actions, "text_delta", {
          text = message.text, message_id = message.id, turn_id = message.turn_id,
        })
        if message.turn_id ~= nil then state.turn_text[message.turn_id] = message.text end
      end
      for _, call in ipairs(message.tool_calls or {}) do
        local exchange = exchange_by_id[call.id] or exchange_by_call_id[call.id]
        local exchange_id = exchange and exchange.id or call.id
        state.exchanges[exchange_id] = call.status
        action(actions, "tool_started", {
          exchange_id = exchange_id,
          name = call.name,
          arguments = exchange_arguments(exchange or call),
          turn_id = message.turn_id,
        })
      end
      action(actions, "assistant_completed", {
        text = message.text or "",
        message_id = message.id,
        turn_id = message.turn_id,
        terminal = message.terminal,
      })
      for _, call in ipairs(message.tool_calls or {}) do
        local exchange = exchange_by_id[call.id] or exchange_by_call_id[call.id]
        local provider_call_id = type(exchange) == "table" and exchange.tool_call_id or nil
        if provider_call_id == nil or not tool_message_by_call_id[provider_call_id] then
          complete_exchange(exchange)
        end
      end
    elseif message.role == "user" and message.input_cause ~= "internal_async_completion" then
      local text = visible_text(message)
      remember_user_prompt(state, message, text)
      action(actions, "message", {
        role = message.role,
        text = text,
        message_id = message.id,
        turn_id = message.turn_id,
        submission_ids = message.submission_ids,
      })
    elseif message.role == "tool" then
      complete_exchange(exchange_by_call_id[message.tool_call_id])
    end
  end
  for _, turn in ipairs(projection.turns or {}) do
    if turn.status ~= "open" then
      action(actions, "turn_" .. turn.status, {
        turn_id = turn.id,
        run_id = turn.run_id,
        terminal = turn.terminal,
        answer = state.turn_text[turn.id],
      })
    end
  end
  for _, compaction in ipairs(projection.compactions or {}) do
    action(actions, "compaction_" .. compaction.status, { compaction = compaction })
  end
  if type(projection.pending_edit) == "table" then
    action(actions, "rewind_restored", { pending_edit = projection.pending_edit })
  end
end

function M.reduce(previous, body)
  previous = previous or M.new()
  if type(body) ~= "table" then return previous, {} end

  if body.kind == "conversation.active.changed" then
    if type(body.conversation_id) ~= "string" or body.conversation_id == "" then
      return initial(nil), { { kind = "active_cleared" } }
    end
    if previous.active_conversation_id == body.conversation_id then return previous, {} end
    return initial(body.conversation_id), {
      { kind = "active_changed", conversation_id = body.conversation_id },
    }
  end

  local active = previous.active_conversation_id
  if active == nil or body.conversation_id ~= active then return previous, {} end

  local state = {
    active_conversation_id = active,
    messages = copy_map(previous.messages),
    exchanges = copy_map(previous.exchanges),
    turn_text = copy_map(previous.turn_text),
    user_prompts = {},
    history_head = previous.history_head,
    pending_edit = previous.pending_edit,
  }
  for index, prompt in ipairs(previous.user_prompts or {}) do state.user_prompts[index] = prompt end
  local actions = {}

  if body.kind == "conversation.snapshot" then
    if body.found == true and type(body.projection) == "table" then
      snapshot_actions(state, body.projection, actions)
    end
    return state, actions
  end
  if body.kind ~= "conversation.projection.delta" or type(body.change) ~= "table" then
    return previous, {}
  end

  local change = body.change
  local kind = change.kind
  if kind == "rewind_committed" and type(change.projection) == "table" then
    snapshot_actions(state, change.projection, actions)
    return state, actions
  end
  if kind == "conversation_created" then
    action(actions, "conversation_created", { conversation_id = active })
  elseif kind == "turn_started" then
    action(actions, "turn_started", {
      turn_id = change.turn_id,
      run_id = change.run_id,
    })
  elseif kind == "message_started" then
    local clears_rewind = state.pending_edit ~= nil
      and type(change.message) == "table" and change.message.role == "user"
    record_message(state, change.message)
    if clears_rewind then
      state.pending_edit = nil
      action(actions, "rewind_cleared", {})
    end
  elseif kind == "content_chunk_appended" then
    local message = state.messages[change.message_id]
    local chunk = change.chunk
    if hidden(message) then
      -- Model context only; no delta reaches the transcript.
    elseif message and message.role == "user" and type(chunk) == "table"
        and chunk.kind == "structured" then
      message.display_text = display.structured_text(chunk.data)
    elseif message and message.role == "assistant" and type(chunk) == "table"
        and type(chunk.data) == "string" and chunk.data ~= "" then
      message.streamed = true
      if chunk.kind == "text" then
        if message.turn_id ~= nil then
          state.turn_text[message.turn_id] = (state.turn_text[message.turn_id] or "") .. chunk.data
        end
        action(actions, "text_delta", {
          text = chunk.data, message_id = change.message_id, turn_id = message.turn_id,
        })
      elseif chunk.kind == "reasoning" then
        action(actions, "reasoning_delta", {
          text = chunk.data, message_id = change.message_id, turn_id = message.turn_id,
        })
      end
    end
  elseif kind == "message_completed" then
    message_completed(state, actions, change.message)
  elseif kind == "message_interrupted" then
    if settle_hidden(state, actions, change.message) then return state, actions end
    action(actions, "message_interrupted", {
      message = change.message,
      turn_id = change.turn_id,
    })
  elseif kind == "tool_exchange_started" or kind == "tool_call_completed" then
    local exchange = change.exchange or {}
    state.exchanges[exchange.id] = exchange.status
    action(actions, "tool_started", {
      exchange_id = exchange.id,
      name = exchange.name,
      arguments = exchange_arguments(exchange),
      turn_id = change.turn_id,
    })
  elseif kind == "tool_result_recorded" or kind == "tool_error_recorded" then
    local exchange = change.exchange or {}
    state.exchanges[exchange.id] = exchange.status
    local is_error = kind == "tool_error_recorded"
    local output = exchange.result
    if is_error then output = exchange.error end
    action(actions, "tool_completed", {
      exchange_id = exchange.id,
      output = output,
      error = is_error,
      completion_delivery = exchange.completion_delivery,
      turn_id = change.turn_id,
    })
  elseif kind == "retry_started" then
    action(actions, "retry_started", {
      retry = change.retry,
      turn_id = change.turn_id,
    })
  elseif kind == "turn_completed" or kind == "turn_failed" or kind == "turn_interrupted" then
    action(actions, kind, {
      turn_id = change.turn_id,
      run_id = change.run_id,
      terminal = change.terminal,
      answer = state.turn_text[change.turn_id],
    })
  elseif kind == "context_compaction_pending" then
    action(actions, "compaction_pending", { compaction = change.compaction })
  elseif kind == "context_compaction_completed" then
    action(actions, "compaction_completed", { compaction = change.compaction })
  elseif kind == "context_compaction_failed" then
    action(actions, "compaction_failed", { compaction = change.compaction })
  elseif kind == "conversation_interrupted" or kind == "conversation_failed" then
    action(actions, kind, {
      terminal = change.terminal,
      message = terminal_text(change.terminal),
    })
  end
  return state, actions
end

return M
