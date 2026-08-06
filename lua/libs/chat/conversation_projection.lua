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

local function record_message(state, message)
  if type(message) ~= "table" or type(message.id) ~= "string" then return end
  state.messages[message.id] = {
    role = message.role,
    turn_id = message.turn_id,
  }
end

local function message_completed(state, actions, message)
  record_message(state, message)
  if type(message) ~= "table" then return end
  if message.role == "user" then
    action(actions, "message", {
      role = message.role,
      text = message.text or "",
      message_id = message.id,
      turn_id = message.turn_id,
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
  action(actions, "snapshot_reset", {})

  local exchange_by_id = {}
  local exchange_by_call_id = {}
  local tool_message_by_call_id = {}
  for _, exchange in ipairs(projection.exchanges or {}) do
    exchange_by_id[exchange.id] = exchange
    local call = exchange.arguments
    if type(call) == "table" and type(call.id) == "string" then
      exchange_by_call_id[call.id] = exchange
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
    action(actions, "tool_completed", {
      exchange_id = exchange.id,
      output = exchange.result or exchange.error,
      error = exchange.status == "error",
    })
  end

  for _, message in ipairs(projection.messages or {}) do
    record_message(state, message)
    if message.role == "assistant" then
      if type(message.reasoning) == "string" and message.reasoning ~= "" then
        action(actions, "reasoning_delta", { text = message.reasoning, turn_id = message.turn_id })
      end
      if type(message.text) == "string" and message.text ~= "" then
        action(actions, "text_delta", { text = message.text, turn_id = message.turn_id })
        if message.turn_id ~= nil then state.turn_text[message.turn_id] = message.text end
      end
      for _, call in ipairs(message.tool_calls or {}) do
        state.exchanges[call.id] = call.status
        action(actions, "tool_started", {
          exchange_id = call.id,
          name = call.name,
          arguments = call.arguments,
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
        local exchange = exchange_by_id[call.id]
        local provider_call_id = type(exchange) == "table"
            and type(exchange.arguments) == "table"
            and exchange.arguments.id
          or nil
        if provider_call_id == nil or not tool_message_by_call_id[provider_call_id] then
          complete_exchange(exchange)
        end
      end
    elseif message.role == "user" then
      action(actions, "message", {
        role = message.role,
        text = message.text or "",
        message_id = message.id,
        turn_id = message.turn_id,
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
  }
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
  if kind == "conversation_created" then
    action(actions, "conversation_created", { conversation_id = active })
  elseif kind == "turn_started" then
    action(actions, "turn_started", {
      turn_id = change.turn_id,
      run_id = change.run_id,
    })
  elseif kind == "message_started" then
    record_message(state, change.message)
  elseif kind == "content_chunk_appended" then
    local message = state.messages[change.message_id]
    local chunk = change.chunk
    if message and message.role == "assistant" and type(chunk) == "table"
        and type(chunk.data) == "string" and chunk.data ~= "" then
      if chunk.kind == "text" then
        if message.turn_id ~= nil then
          state.turn_text[message.turn_id] = (state.turn_text[message.turn_id] or "") .. chunk.data
        end
        action(actions, "text_delta", { text = chunk.data, turn_id = message.turn_id })
      elseif chunk.kind == "reasoning" then
        action(actions, "reasoning_delta", { text = chunk.data, turn_id = message.turn_id })
      end
    end
  elseif kind == "message_completed" then
    message_completed(state, actions, change.message)
  elseif kind == "message_interrupted" then
    record_message(state, change.message)
    action(actions, "message_interrupted", {
      message = change.message,
      turn_id = change.turn_id,
    })
  elseif kind == "tool_call_completed" then
    local exchange = change.exchange or {}
    if state.exchanges[exchange.id] == nil then
      state.exchanges[exchange.id] = exchange.status
      local arguments = type(exchange.arguments) == "table"
          and (exchange.arguments.arguments or exchange.arguments.args)
        or exchange.arguments
      action(actions, "tool_started", {
        exchange_id = exchange.id,
        name = exchange.name,
        arguments = arguments,
        turn_id = change.turn_id,
      })
    end
  elseif kind == "tool_result_recorded" or kind == "tool_error_recorded" then
    local exchange = change.exchange or {}
    state.exchanges[exchange.id] = exchange.status
    action(actions, "tool_completed", {
      exchange_id = exchange.id,
      output = exchange.result or exchange.error,
      error = kind == "tool_error_recorded",
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
