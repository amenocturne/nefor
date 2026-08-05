-- Shared provider round/request lifecycle for provider-boundary factories.
-- Classification policy stays in the thin consumer (`llm` or
-- `structured-output`); correlation, history, tool calls, cancellation and
-- drain behavior live here once. Canonical conversation history is emitted as
-- facts and projected outside this request-local boundary.

local kinds = require("kinds")
local conversation_facts = require("conversation-facts")
local M = {}

local function copy_history(id, factory_name, seed)
  local copied = {}
  if seed == nil then return copied end
  if type(seed) ~= "table" then
    return nil, string.format("%s '%s': params.history must be an array of transcript messages (got %s)",
      factory_name, tostring(id), type(seed))
  end
  local keys = 0
  for _ in pairs(seed) do keys = keys + 1 end
  if keys ~= #seed then
    return nil, string.format("%s '%s': params.history must be an array of transcript messages, not a map",
      factory_name, tostring(id))
  end
  for i = 1, #seed do
    local entry = seed[i]
    if type(entry) ~= "table" then
      return nil, string.format("%s '%s': params.history[%d] must be a role-tagged message table",
        factory_name, tostring(id), i)
    end
    if type(entry.role) ~= "string" or entry.role == "" then
      return nil, string.format("%s '%s': params.history[%d] is missing a role",
        factory_name, tostring(id), i)
    end
    if entry.tool_calls ~= nil and type(entry.tool_calls) ~= "table" then
      return nil, string.format("%s '%s': params.history[%d].tool_calls must be an array",
        factory_name, tostring(id), i)
    end
    copied[i] = entry
  end
  return copied
end

local function encode_args(args)
  if type(args) == "string" then return args end
  if type(nefor) == "table" and type(nefor.json) == "table"
      and type(nefor.json.encode) == "function" then
    local ok, encoded = pcall(nefor.json.encode, args or {})
    if ok and type(encoded) == "string" then return encoded end
  end
  return args
end

function M.construct(id, params, emit, options)
  params = params or {}
  options = options or {}
  local factory_name = options.name or "provider-boundary"
  local provider = params.provider
  if type(provider) ~= "string" or provider == "" then
    return nil, string.format("%s '%s': params.provider is required", factory_name, tostring(id))
  end
  local request_messages, history_error = copy_history(id, factory_name, params.history)
  if not request_messages then return nil, history_error end

  local conversation = options.conversation
  if type(conversation) ~= "table" or type(conversation.id) ~= "string"
      or type(conversation.emit) ~= "function" then
    return nil, string.format("%s '%s': conversation dependency is required", factory_name, tostring(id))
  end
  local provenance = {}
  for key, value in pairs(conversation.provenance or {}) do provenance[key] = value end
  provenance.provider = provider
  provenance.model = params.model

  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }
  local state = {}
  local observe = type(options.preview) == "function" and options.preview or function() return false end
  local seq = 0
  local pending = nil
  local draining = false
  local turn_active = false
  local awaiting_continuation = false
  local steered_messages = {}
  local streamed_message_id = nil
  local streamed_text = ""
  local terminal_metadata = {}
  local facts = nil
  local firing_sequence = 0
  local conversation_created = conversation.is_root

  local function start_firing()
    firing_sequence = firing_sequence + 1
    local turn_id = conversation.turn_id
    if firing_sequence > 1 then
      turn_id = turn_id .. ":firing:" .. tostring(firing_sequence)
    end
    facts = conversation_facts.new({
      conversation_id = conversation.id,
      turn_id = turn_id,
      provenance = provenance,
      emit = conversation.emit,
    })
    if not conversation_created then
      facts:create(provenance)
      conversation_created = true
    end
    facts:start_turn()
    streamed_message_id = nil
    streamed_text = ""
    terminal_metadata = {}
  end

  local function merge_terminal(detail)
    local merged = {}
    for key, value in pairs(terminal_metadata) do merged[key] = value end
    for key, value in pairs(detail or {}) do merged[key] = value end
    local result = merged.result
    if type(result) == "table" then
      for _, key in ipairs({ "model", "duration_ms", "usage", "finish_reason" }) do
        if merged[key] == nil then merged[key] = result[key] end
      end
    end
    return merged
  end

  local function interrupt_stream(reason)
    if streamed_message_id == nil then return end
    facts:interrupt_message(streamed_message_id, { reason = reason })
    streamed_message_id = nil
    streamed_text = ""
  end

  function state:append(message, completion)
    request_messages[#request_messages + 1] = message
    if message.role == "assistant" and streamed_message_id then
      local content = message.content
      if type(content) == "string" and content ~= "" then
        if streamed_text == "" then
          facts:content(streamed_message_id, "text", content)
        elseif content:sub(1, #streamed_text) == streamed_text
            and #content > #streamed_text then
          facts:content(streamed_message_id, "text", content:sub(#streamed_text + 1))
        end
      end
      facts:finish_message(streamed_message_id, message, completion)
      streamed_message_id = nil
      streamed_text = ""
    else
      facts:message(message, completion)
    end
    if type(message.tool_calls) == "table" then
      for _, call in ipairs(message.tool_calls) do
        observe("append", "transcript", { kind = "tool_call", value = call })
      end
    end
  end
  function state:emit(message) emit(sign(message)) end
  function state:observe(operation, binding, value) return observe(operation, binding, value) end
  function state:is_draining() return draining end
  function state:finish(message, terminal_detail)
    self:emit(message)
    self:emit({ kind = kinds.complete })
    turn_active = false
    awaiting_continuation = false
    facts:complete_turn(merge_terminal(terminal_detail or {
      result = message.result,
      value = message.value,
      semantic_type_id = message.semantic_type_id,
    }))
  end
  function state:fail(detail)
    interrupt_stream("provider_failed")
    self:emit({ kind = kinds.failed, failure = kinds.Failed, value = { error = detail } })
    turn_active = false
    awaiting_continuation = false
    facts:fail_turn(merge_terminal({ error = detail }))
  end

  local function extend_history(input)
    if type(input) == "string" then
      state:append({ role = "user", content = input })
    elseif type(input) == "table" and type(input.messages) == "table" then
      for _, message in ipairs(input.messages) do state:append(message) end
    elseif type(input) == "table" and input.role ~= nil then
      state:append(input)
    elseif type(input) == "table" and input.text ~= nil then
      state:append({ role = "user", content = input.text })
    end
  end

  local function build_request()
    local messages = {}
    for i, message in ipairs(request_messages) do messages[i] = message end
    local model = params.model
    if type(model) == "table" then
      model = model.present and model.value or nil
    end
    return {
      routing_session_id = conversation.routing_session_id,
      model = model,
      system = params.system,
      tools = params.tools,
      reasoning_effort = params.reasoning_effort,
      conversation_context = params.conversation_context,
      output_schema = params.schema,
      max_corrections = params.max_corrections,
      input = { messages = messages },
    }
  end

  local function invoke_provider()
    if draining then return false end
    seq = seq + 1
    pending = { request_id = id .. "@r" .. tostring(seq) }
    state:emit({
      kind = "capability.invoke",
      capability = provider,
      request = build_request(),
      ref = pending.request_id,
    })
    return true
  end
  function state:retry(reason)
    facts:retry(reason or "provider_retry")
    return invoke_provider()
  end

  local function append_steered_messages()
    if #steered_messages == 0 then return false end
    for _, message in ipairs(steered_messages) do state:append(message) end
    steered_messages = {}
    return true
  end

  local function emit_failure(detail)
    if options.on_error then
      options.on_error(state, detail)
    else
      state:fail(detail)
    end
  end

  local function emit_tool_calls(result)
    local calls, wire_calls = {}, {}
    for i, tc in ipairs(result.tool_calls) do
      tc = tc or {}
      local fn = type(tc["function"]) == "table" and tc["function"] or {}
      calls[i] = {
        id = tc.id,
        name = tc.name or fn.name,
        args = tc.args or tc.arguments or fn.arguments,
      }
      wire_calls[i] = {
        id = calls[i].id,
        type = "function",
        ["function"] = { name = calls[i].name, arguments = encode_args(calls[i].args) },
      }
    end
    state:append({
      role = "assistant",
      content = type(result.text) == "string" and result.text or "",
      tool_calls = wire_calls,
    })
    local semantic_calls = {}
    for i, call in ipairs(calls) do
      semantic_calls[i] = { name = call.name, arguments = call.args }
    end
    state:emit({ kind = "generic-tool.ToolCalls",
      value = { calls = semantic_calls }, calls = calls })
    state:emit({ kind = kinds.complete })
    awaiting_continuation = true
  end

  function instance.deliver(activation)
    activation = activation or {}
    if activation.kind == "reply" then
      if pending == nil then return nil end
      pending = nil
      if activation.error ~= nil then
        emit_failure(activation.error)
        return nil
      end
      local result = activation.result
      if type(result) == "table" and result.finish_reason == "error" then
        local detail = result.error
        if type(detail) ~= "string" or detail == "" then
          detail = "provider returned finish_reason \"error\" with no detail"
        end
        emit_failure(detail)
        return nil
      end
      if type(result) == "table" and type(result.tool_calls) == "table" and #result.tool_calls > 0 then
        if options.on_tool_calls then options.on_tool_calls(state, result) end
        emit_tool_calls(result)
        return nil
      end
      if options.steerable and #steered_messages > 0 then
        if options.on_steered_final then options.on_steered_final(state, result) end
        append_steered_messages()
        invoke_provider()
      else
        options.on_final(state, result)
      end
      return nil
    end

    if draining then return nil end
    local continuation = awaiting_continuation
    awaiting_continuation = false
    if not continuation then start_firing() end
    if not turn_active then turn_active = true end
    extend_history(((activation.messages or {})[1] or {}).message)
    append_steered_messages()
    if not continuation and options.on_turn_start then options.on_turn_start(state) end
    invoke_provider()
    return { status = "pending" }
  end

  function instance.handle_kill()
    pending = nil
    turn_active = false
    awaiting_continuation = false
    interrupt_stream("actor_killed")
    if facts then facts:interrupt_turn({ reason = "actor_killed" }) end
  end

  function instance.handle_drain()
    if pending == nil then
      state:emit({ kind = kinds.complete })
      turn_active = false
      interrupt_stream("actor_drained")
      if facts then facts:interrupt_turn({ reason = "actor_drained" }) end
    else
      draining = true
    end
  end

  function instance.handle_steer(message)
    if not options.steerable or type(message) ~= "table" then return false end
    if type(message.role) ~= "string" or message.role == "" then return false end
    steered_messages[#steered_messages + 1] = message
    return true
  end

  function instance.handle_observation(observation)
    local value = observation and observation.value
    if observation.binding == "conversation" and type(value) == "table" then
      if value.kind == "retry" then
        facts:retry(value.error or value.message or "provider_retry", value)
      elseif value.kind == "usage" then
        terminal_metadata.usage = value.usage or value.result
        terminal_metadata.model = value.model or terminal_metadata.model
        terminal_metadata.duration_ms = value.duration_ms or terminal_metadata.duration_ms
      elseif value.kind == "interrupted" then
        interrupt_stream("provider_interrupted")
        facts:interrupt_turn(merge_terminal(value))
      elseif value.kind == "failed" or value.kind == "error" then
        interrupt_stream("provider_failed")
        facts:fail_turn(merge_terminal(value))
      end
      return true
    end
    if observation.binding ~= "transcript" or type(value) ~= "table"
        or type(value.text) ~= "string" or value.text == "" then
      return false
    end
    local chunk_kind = value.kind == "reasoning" and "reasoning" or "text"
    if streamed_message_id == nil then
      streamed_message_id = facts:start_message("assistant")
    end
    facts:content(streamed_message_id, chunk_kind, value.text)
    if chunk_kind == "text" then streamed_text = streamed_text .. value.text end
    return true
  end

  state:emit({ kind = kinds.ready })
  return instance
end

function M.answer_text(result)
  if type(result) == "string" then return result end
  if type(result) ~= "table" then return nil end
  if type(result.final_answer) == "string" then return result.final_answer end
  if type(result.text) == "string" then return result.text end
  if type(result.result) == "string" then return result.result end
  return nil
end

return M
