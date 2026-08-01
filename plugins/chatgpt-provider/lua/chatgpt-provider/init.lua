local oa = require("openai-provider")

local function copy(value)
  local out = {}
  for key, item in pairs(value or {}) do out[key] = item end
  return out
end

local function translator(name)
  local t = oa.translator(name)
  local prefix = name .. "."
  local base_outbound = t.outbound
  local base_inbound = t.inbound

  t.kinds.completion_request = prefix .. "completion.request"
  t.kinds.completion_cancel = prefix .. "completion.cancel"
  t.kinds.completion_event = prefix .. "completion.event"
  t.kinds.stream_tool_call_delta = prefix .. "stream.tool_call_delta"
  t.kinds.usage_requested = prefix .. "usage.requested"
  t.kinds.usage_updated = prefix .. "usage.updated"
  t.kinds.usage_error = prefix .. "usage.error"

  t.complete = function(request)
    assert(type(request) == "table", "chatgpt-provider.complete: request required")
    assert(type(request.request_id) == "string" and #request.request_id > 0,
      "chatgpt-provider.complete: request_id required")
    assert(type(request.messages) == "table",
      "chatgpt-provider.complete: complete messages/history required")
    local body = copy(request)
    body.kind = t.kinds.completion_request
    return body
  end

  t.cancel = function(request_id)
    assert(type(request_id) == "string" and #request_id > 0,
      "chatgpt-provider.cancel: request_id required")
    return { kind = t.kinds.completion_cancel, request_id = request_id }
  end

  t.inbound = function(env)
    local body = type(env) == "table" and env.body or nil
    if type(body) == "table" and body.kind == "ProviderRequest" then
      if body.provider ~= nil and body.provider ~= name then return nil end
      if body.cancel == true then return t.cancel(body.request_id) end
      return t.complete(body)
    end
    if type(body) == "table" and body.kind == "chat.usage.requested" then
      if body.provider ~= name then return nil end
      return { kind = t.kinds.usage_requested }
    end
    return base_inbound(env)
  end

  t.outbound = function(env)
    local body = type(env) == "table" and env.body or nil
    local kind = type(body) == "table" and body.kind or nil
    if type(kind) ~= "string" then return base_outbound(env) end

    if kind == t.kinds.completion_event then
      if type(body.request_id) ~= "string" or #body.request_id == 0 then return nil end
      return copy(body)
    end

    if kind == t.kinds.usage_updated or kind == t.kinds.usage_error then
      local translated = copy(body)
      translated.kind = kind == t.kinds.usage_updated and "chat.usage.updated" or "chat.usage.error"
      translated.provider = name
      return translated
    end

    local request_id = body.request_id or body.chat_id
    if type(request_id) ~= "string" or #request_id == 0 then
      return base_outbound(env)
    end

    local event
    if kind == t.kinds.completion_event then
      event = body.event
    elseif kind == t.kinds.stream_delta then
      event = "text_delta"
    elseif kind == t.kinds.stream_reasoning_delta then
      event = "reasoning_delta"
    elseif kind == t.kinds.stream_reasoning_end then
      event = "reasoning_completed"
    elseif kind == t.kinds.stream_tool_call_delta then
      event = "tool_call_delta"
    elseif kind == t.kinds.stream_retry then
      event = "retry"
    elseif kind == t.kinds.session_stats then
      event = "usage"
    elseif kind == t.kinds.turn_error or kind == t.kinds.chat_error then
      return nil
    elseif kind == t.kinds.chat_complete_result then
      event = body.finish_reason == "error" and "failed"
        or body.finish_reason == "interrupted" and "interrupted"
        or "completed"
    elseif kind == t.kinds.stream_end then
      return nil
    else
      return base_outbound(env)
    end

    local translated = copy(body)
    translated.kind = t.kinds.completion_event
    translated.request_id = request_id
    translated.chat_id = nil
    translated.event = event
    if kind == t.kinds.chat_complete_result then
      translated.result = copy(body.output)
      translated.output = nil
      translated.finish_reason = nil
      translated.error = translated.result.error or body.error
    end
    return translated
  end

  return t
end

return {
  translator = translator,
}
