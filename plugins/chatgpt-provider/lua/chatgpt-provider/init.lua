local oa = require("openai-provider")
local json_data = require("core.json_data")

local function copy(value)
  return json_data.copy(value or {})
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

  t.complete = function(request, context)
    assert(type(request) == "table", "chatgpt-provider.complete: request required")
    assert(type(request.request_id) == "string" and #request.request_id > 0,
      "chatgpt-provider.complete: request_id required")
    assert(type(context) == "table" and type(context.messages) == "table",
      "chatgpt-provider.complete: conversation context required")
    local body = copy(request)
    body.provider = nil
    body.watermark = nil
    body.messages = {}
    for index, message in ipairs(context.messages) do
      body.messages[index] = t.context_message(message)
    end
    local lowered_context = copy(context)
    lowered_context.messages = {}
    for index, message in ipairs(context.messages) do
      lowered_context.messages[index] = t.context_message(message)
    end
    lowered_context.tail_messages = {}
    for index, message in ipairs(context.tail_messages or {}) do
      lowered_context.tail_messages[index] = t.context_message(message)
    end
    body.conversation_context = lowered_context
    body.kind = t.kinds.completion_request
    return body
  end

  t.cancel_completion = function(request_id)
    assert(type(request_id) == "string" and #request_id > 0,
      "chatgpt-provider.cancel: request_id required")
    return { kind = t.kinds.completion_cancel, request_id = request_id }
  end


  -- Interpret provider-owned checkpoints here, at the provider boundary.
  -- Conversation-manager and callers only carry the envelope; they never
  -- need to know that ChatGPT compaction is represented by Responses items.
  t.compact_context = function(change)
    local context = type(change) == "table" and change.context or nil
    if type(context) ~= "table" or type(context.messages) ~= "table" then
      return nil, "conversation context is missing complete messages"
    end

    local request_id = change.compaction and change.compaction.request_id
    if type(request_id) ~= "string" or request_id == "" then
      return nil, "conversation compaction request_id is missing"
    end

    local chat_id = "conversation-compact:" .. request_id
    local plan = {
      chat_id = chat_id,
      create = {
        kind = prefix .. "chat.create",
        chat_id = chat_id,
        conversation_id = change.conversation_id,
      },
      messages = context.messages,
      compact = {
        kind = prefix .. "chat.compact",
        chat_id = chat_id,
        trigger = "conversation-manager",
      },
      delete = { kind = prefix .. "chat.delete", chat_id = chat_id },
    }

    local selected = context.compaction
    local checkpoint = type(selected) == "table" and selected.checkpoint or nil
    if type(checkpoint) == "table"
        and checkpoint.provider == name
        and checkpoint.format == "chatgpt.responses.compaction.v1"
        and type(checkpoint.artifact) == "table"
        and type(checkpoint.artifact.items) == "table" then
      plan.restore = {
        kind = prefix .. "chat.compaction.restore",
        chat_id = chat_id,
        model_context_artifact = checkpoint.artifact,
      }
      plan.messages = type(context.tail_messages) == "table"
        and context.tail_messages or {}
    end
    for index, message in ipairs(plan.messages) do
      plan.messages[index] = t.context_message(message)
    end
    return plan
  end

  t.inbound = function(env)
    local body = type(env) == "table" and env.body or nil
    if type(body) == "table" and (body.kind == t.kinds.completion_request
        or body.kind == "ProviderRequest") then return nil end
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
