-- Shared provider round/transcript lifecycle for provider-boundary factories.
-- Classification policy stays in the thin consumer (`llm` or
-- `structured-output`); correlation, history, tool calls, cancellation and
-- drain behavior live here once.

local kinds = require("kinds")
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
  local history, history_error = copy_history(id, factory_name, params.history)
  if not history then return nil, history_error end

  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }
  local state = {}
  local seed_len = #history
  local seq = 0
  local pending = nil
  local draining = false
  local turn_active = false
  local awaiting_continuation = false

  function state:append(message) history[#history + 1] = message end
  function state:emit(message) emit(sign(message)) end
  function state:is_draining() return draining end
  function state:transcript_delta()
    local delta = {}
    for i = seed_len + 1, #history do delta[#delta + 1] = history[i] end
    return delta
  end
  function state:finish(message)
    self:emit(message)
    self:emit({ kind = kinds.complete })
    turn_active = false
    awaiting_continuation = false
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
    for i, message in ipairs(history) do messages[i] = message end
    return {
      chat_id = pending.chat_id,
      model = params.model,
      system = params.system,
      tools = params.tools,
      reasoning_effort = params.reasoning_effort,
      input = { messages = messages },
    }
  end

  local function invoke_provider()
    if draining then return false end
    seq = seq + 1
    pending = { chat_id = id .. "@r" .. tostring(seq) }
    state:emit({
      kind = "capability.invoke",
      capability = provider,
      request = build_request(),
      ref = pending.chat_id,
    })
    return true
  end
  function state:retry() return invoke_provider() end

  local function emit_failure(detail)
    state:emit({ kind = kinds.failed, failure = kinds.Failed, value = { error = detail } })
    turn_active = false
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
    state:emit({ kind = "generic-tool.ToolCalls", calls = calls })
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
        emit_tool_calls(result)
        return nil
      end
      options.on_final(state, result)
      return nil
    end

    if draining then return nil end
    local continuation = awaiting_continuation
    awaiting_continuation = false
    if not turn_active then turn_active = true end
    extend_history(((activation.messages or {})[1] or {}).message)
    if not continuation and options.on_turn_start then options.on_turn_start(state) end
    invoke_provider()
    return { status = "pending" }
  end

  function instance.handle_kill()
    if pending ~= nil then
      state:emit({ kind = provider .. ".chat.cancel", chat_id = pending.chat_id })
      pending = nil
    end
    turn_active = false
  end

  function instance.handle_drain()
    if pending == nil then
      state:emit({ kind = kinds.complete })
      turn_active = false
    else
      draining = true
    end
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
