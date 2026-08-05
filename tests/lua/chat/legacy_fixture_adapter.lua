-- Test-only bridge for pre-conversation-manager chat fixtures. Production has
-- no legacy transcript handlers; this keeps UI behavior tests on canonical
-- projection actions while fixtures are migrated individually.
local M = {}

local function copy(value)
  if type(value) ~= "table" then return value end
  local out = {}
  for key, item in pairs(value) do out[key] = copy(item) end
  return out
end

function M.wrap(update)
  local conversation_generation = 1
  local conversation_id = "fixture-conversation-1"
  local sequence, turn_sequence, message_sequence = 0, 0, 0
  local active, turn_id, assistant_id = false, nil, nil
  local text_seen = false
  local terminal = {}
  local last_turn_id, last_terminal = nil, nil

  local function reset_fixture_conversation()
    conversation_generation = conversation_generation + 1
    conversation_id = "fixture-conversation-" .. conversation_generation
    active, turn_id, assistant_id = false, nil, nil
    text_seen = false
    terminal = {}
    last_turn_id, last_terminal = nil, nil
  end

  local function run(body, state, effects)
    local next_state, next_effects = update(body, state)
    for _, effect in ipairs(next_effects or {}) do effects[#effects + 1] = effect end
    return next_state
  end
  local function canonical(change)
    sequence = sequence + 1
    return { kind = "conversation.projection.delta", conversation_id = conversation_id,
      sequence = sequence, change = change }
  end
  local function ensure_active(state, effects)
    if active then return state end
    active = true
    return run({ kind = "conversation.active.changed", conversation_id = conversation_id }, state, effects)
  end
  local function ensure_turn(state, effects)
    state = ensure_active(state, effects)
    if turn_id then return state end
    turn_sequence = turn_sequence + 1
    turn_id = "fixture-turn-" .. turn_sequence
    terminal = {}
    text_seen = false
    return run(canonical({ kind = "turn_started", turn_id = turn_id, run_id = turn_id }), state, effects)
  end
  local function ensure_assistant(state, effects)
    state = ensure_turn(state, effects)
    if assistant_id then return state end
    message_sequence = message_sequence + 1
    assistant_id = "fixture-assistant-" .. message_sequence
    return run(canonical({ kind = "message_started", turn_id = turn_id,
      message = { id = assistant_id, turn_id = turn_id, role = "assistant" } }), state, effects)
  end
  local function append_chunk(state, effects, kind, text)
    if type(text) ~= "string" or text == "" then return state end
    state = ensure_assistant(state, effects)
    if kind == "text" then text_seen = true end
    return run(canonical({ kind = "content_chunk_appended", turn_id = turn_id,
      message_id = assistant_id, chunk = { kind = kind, data = text } }), state, effects)
  end
  local function merge_terminal(body)
    terminal.model = body.model or terminal.model
    terminal.duration_ms = body.duration_ms or body.last_turn_duration_ms or terminal.duration_ms
    terminal.usage = terminal.usage or {}
    terminal.usage.input_tokens = body.input_tokens or body.last_turn_input_tokens
      or body.prompt_tokens
      or terminal.usage.input_tokens
    terminal.usage.output_tokens = body.output_tokens or body.last_turn_output_tokens
      or body.completion_tokens
      or terminal.usage.output_tokens
  end
  local function finish_turn(state, effects, body)
    merge_terminal(body or {})
    if assistant_id then
      state = run(canonical({ kind = "message_completed", turn_id = turn_id,
        message = { id = assistant_id, turn_id = turn_id, role = "assistant",
          text = body and body.text or "", terminal = copy(terminal) } }), state, effects)
    end
    if turn_id then
      state = run(canonical({ kind = "turn_completed", turn_id = turn_id,
        run_id = turn_id, terminal = copy(terminal) }), state, effects)
      last_turn_id, last_terminal = turn_id, copy(terminal)
    end
    turn_id, assistant_id = nil, nil
    text_seen = false
    return state
  end
  local function append_message(state, effects, body)
    state = ensure_active(state, effects)
    if body.role == "assistant" then
      state = append_chunk(ensure_assistant(state, effects), effects, "text", body.text)
      return finish_turn(state, effects, body)
    end
    message_sequence = message_sequence + 1
    local id = "fixture-message-" .. message_sequence
    state = run(canonical({ kind = "message_started",
      message = { id = id, role = body.role or "user" } }), state, effects)
    if type(body.text) == "string" and body.text ~= "" then
      state = run(canonical({ kind = "content_chunk_appended", message_id = id,
        chunk = { kind = "text", data = body.text } }), state, effects)
    end
    return run(canonical({ kind = "message_completed",
      message = { id = id, role = body.role or "user", text = body.text or "" } }), state, effects)
  end

  return function(body, state)
    local kind = type(body) == "table" and body.kind or nil
    local effects = {}
    if kind == "conversation.active.changed" then
      active = type(body.conversation_id) == "string" and body.conversation_id ~= ""
      if active then conversation_id = body.conversation_id end
      return update(body, state)
    end
    -- Activate before any legacy fixture event can create visible state:
    -- activating a conversation intentionally replaces the projection.
    if not active and type(kind) == "string"
        and kind ~= "sessions.session_end" and kind ~= "sessions.session_start"
        and kind:sub(1, 13) ~= "conversation." then
      state = ensure_active(state, effects)
    end
    if kind == "sessions.session_end" then
      state, effects = update(body, state)
      reset_fixture_conversation()
    elseif kind == "sessions.session_start" then
      reset_fixture_conversation()
      state, effects = update(body, state)
      state = ensure_active(state, effects)
    elseif kind == "chat.message.append" then
      state = append_message(state, effects, body)
    elseif kind == "chat.stream.delta" then
      state = append_chunk(state, effects, "text", body.text)
    elseif kind == "chat.reasoning.delta" or kind == "chat.stream.reasoning_delta" then
      state = append_chunk(state, effects, "reasoning", body.text)
    elseif kind == "chat.reasoning.end" then
      return state, effects
    elseif kind == "chat.stream.end" then
      state = append_chunk(state, effects, "text", body.text)
      merge_terminal(body)
      -- Structured providers historically emitted reasoning, stream.end,
      -- session.stats, and only then the semantic assistant message. Keep that
      -- turn open until the message arrives so its footer receives the stats.
      if assistant_id and text_seen then state = finish_turn(state, effects, body) end
    elseif kind == "chat.session.stats" then
      if turn_id == nil and last_terminal ~= nil then terminal = copy(last_terminal) end
      merge_terminal(body)
      local patch = copy(state)
      patch.stats = copy(state.stats or {})
      for key, value in pairs(body) do
        if key ~= "kind" then patch.stats[key] = copy(value) end
      end
      patch.stats.model = terminal.model or patch.stats.model
      patch.stats.last_turn_duration_ms = terminal.duration_ms
        or patch.stats.last_turn_duration_ms
      patch.stats.last_turn_input_tokens = terminal.usage.input_tokens
        or patch.stats.last_turn_input_tokens
      patch.stats.last_turn_output_tokens = terminal.usage.output_tokens
        or patch.stats.last_turn_output_tokens
      local context_tokens = body.last_turn_context_tokens
        or body.last_turn_input_tokens or body.context_tokens or body.prompt_tokens
      if context_tokens ~= nil then patch.current_context_tokens = context_tokens end
      patch.model = terminal.model or state.model
      patch.max_tokens = body.max_context_tokens or state.max_tokens
      state = patch
      if turn_id == nil and last_turn_id ~= nil then
        last_terminal = copy(terminal)
        state = run(canonical({ kind = "turn_completed", turn_id = last_turn_id,
          run_id = last_turn_id, terminal = copy(last_terminal) }), state, effects)
      end
    elseif kind == "chat.tool.start" then
      state = ensure_active(state, effects)
      state = run(canonical({ kind = "tool_call_completed", turn_id = turn_id,
        exchange = { id = body.id, name = body.name, status = "call_completed",
          arguments = body.input or {} } }), state, effects)
    elseif kind == "chat.tool.end" then
      state = ensure_active(state, effects)
      state = run(canonical({ kind = body.error and "tool_error_recorded" or "tool_result_recorded",
        turn_id = turn_id, exchange = { id = body.id,
          status = body.error and "error" or "result", result = body.output,
          error = copy(body.error) } }), state, effects)
    elseif kind == "chat.compaction.commit" or kind == "chat.compaction.failed" then
      if kind == "chat.compaction.commit"
          and type(body.model_context_artifact) ~= "table" then
        return state, effects
      end
      state = ensure_active(state, effects)
      local failed = kind == "chat.compaction.failed"
      state = run(canonical({ kind = failed and "context_compaction_failed"
          or "context_compaction_completed",
        compaction = { request_id = body.request_id or "fixture-compaction",
          status = failed and "failed" or "completed",
          provider = body.provider, model = body.model, strategy = body.strategy,
          trigger = body.trigger, display_summary = body.display_summary,
          metadata = copy(body.metadata), error = body.error or body.message } }), state, effects)
    else
      state, effects = update(body, state)
    end
    return state, effects
  end
end

return M
