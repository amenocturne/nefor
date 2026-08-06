-- Provider-boundary translation for openai-compatible processes. Durable
-- conversation reconstruction belongs to conversation-manager.

local json = nefor.json
local json_data = require("core.json_data")

local M = {}
local clone_table = json_data.copy

-- Test-only: drop module-level state.
function M._reset()
end

function M.translator(name)
  assert(type(name) == "string" and #name > 0,
    "openai-provider.translator: name required")

  local prefix = name .. "."

  local kinds = {
    stream_delta            = prefix .. "stream.delta",
    stream_end              = prefix .. "stream.end",
    stream_reasoning_delta  = prefix .. "stream.reasoning_delta",
    stream_reasoning_end    = prefix .. "stream.reasoning_end",
    stream_retry            = prefix .. "stream.retry",
    session_stats           = prefix .. "session.stats",
    auth_status             = prefix .. "auth.status",
    auth_set                = prefix .. "auth.set",
    models_listed           = prefix .. "models.listed",
    models_list_requested   = prefix .. "models.list_requested",
    model_set               = prefix .. "model.set",
    model_set_ack           = prefix .. "model.set_ack",
    reasoning_set           = prefix .. "reasoning.set",
    reasoning_set_ack       = prefix .. "reasoning.set_ack",
    turn_error              = prefix .. "turn.error",
    chat_error              = prefix .. "chat.error",
    chat_complete_result    = prefix .. "chat.complete.result",
    chat_compact            = prefix .. "chat.compact",
    chat_compaction_commit  = prefix .. "chat.compaction.commit",
    chat_compaction_restore = prefix .. "chat.compaction.restore",
    chat_create             = prefix .. "chat.create",
    chat_append             = prefix .. "chat.append",
    chat_restore            = prefix .. "chat.restore",
    chat_cancel             = prefix .. "chat.cancel",
    completion_request      = prefix .. "completion.request",
    completion_cancel       = prefix .. "completion.cancel",
    completion_event        = prefix .. "completion.event",
    hello                   = prefix .. "hello",
    ready                   = prefix .. "ready",
    goodbye                 = prefix .. "goodbye",
    login_requested         = prefix .. "login_requested",
    logout_requested        = prefix .. "logout_requested",
    interrupt               = prefix .. "interrupt",
    reset                   = prefix .. "reset",
    prompt                  = prefix .. "prompt",
  }

  local injected_static = false

  -- binary -> bus. Returns the (shallow-copied) body with kind possibly
  -- renamed, or nil to drop.
  local function outbound(env)
    if type(env) ~= "table" or env.type ~= "event"
        or type(env.body) ~= "table" then
      return nil
    end

    -- Shallow-copy so callers can mutate without affecting the source.
    local body = {}
    for k, v in pairs(env.body) do body[k] = v end

    local k = body.kind
    if type(k) ~= "string" then return body end

    if k == "completion.event" then
      if type(body.request_id) ~= "string" or body.request_id == "" then return nil end
      body.kind = kinds.completion_event
      return body
    elseif k == kinds.stream_delta then
      body.kind = "chat.stream.delta"
      return body
    elseif k == kinds.stream_reasoning_delta then
      body.kind = "chat.stream.reasoning_delta"
      return body
    elseif k == kinds.stream_reasoning_end then
      body.kind = "chat.stream.reasoning_end"
      return body
    elseif k == kinds.stream_retry then
      return {
        kind   = "chat.toast",
        id     = body.id,
        text   = body.message or "retrying provider request",
        level  = "warn",
        ttl_ms = (body.delay_ms or 2000) + 1500,
      }
    elseif k == kinds.stream_end then
      body.kind = "chat.stream.end"
      body.finish_reason = nil
      return body
    elseif k == kinds.session_stats then
      body.kind = "chat.session.stats"
      return body
    elseif k == kinds.auth_status then
      body.kind = "chat.auth.status"
      body.provider = name
      return body
    elseif k == kinds.models_listed then
      body.kind = "chat.models.listed"
      body.provider = name
      return body
    elseif k == kinds.model_set_ack then
      body.kind = "chat.model.set_ack"
      body.provider = name
      return body
    elseif k == kinds.reasoning_set_ack then
      body.kind = "chat.reasoning.set_ack"
      body.provider = name
      return body
    elseif k == kinds.chat_compaction_commit then
      body.kind = "chat.compaction.commit"
      body.provider = name
      return body
    elseif k == kinds.turn_error then
      -- A missing message can happen if the binary emits turn.error for
      -- an unknown reason; fall back to a generic label rather than
      -- propagating "nil".
      local msg = tostring(body.message or "(unknown)")
      if msg == "interrupted" then
        return {
          kind = "chat.message.append",
          role = "system",
          text = "[interrupted]",
        }
      end
      return {
        kind = "chat.message.append",
        role = "system",
        text = "Error: " .. msg,
      }
    elseif k == kinds.hello then
      local model = body.model
      if type(model) == "string" and #model > 0 then
        return {
          kind     = "chat.model.set_ack",
          provider = name,
          model    = model,
        }
      end
      return nil
    elseif k == kinds.ready or k == kinds.goodbye then
      -- Control-plane envelopes; static-token injection runs through
      -- maybe_inject_static_token, not the bus.
      return nil
    end

    -- chat.complete.result / chat.error / chat.create / chat.append and
    -- other prefixed envelopes pass through unchanged: their kind stays
    -- prefixed so callers can pattern-match without losing shape.
    return body
  end

  -- bus -> binary. Returns body|nil; nil drops the delivery.
  local function inbound(env)
    if type(env) ~= "table" or env.type ~= "event"
        or type(env.body) ~= "table" then
      return nil
    end

    -- Don't echo back envelopes we ourselves published onto the bus.
    if env.from == name then return nil end

    local body = {}
    for k, v in pairs(env.body) do body[k] = v end

    local k = body.kind
    if type(k) ~= "string" then return body end

    -- Direct completion requests are private compositor-to-process traffic.
    -- Never accept their native wire kinds from the public bus; the compositor
    -- calls complete/cancel_completion and delivers the resulting body itself.
    if k == kinds.completion_request or k == kinds.completion_cancel then return nil end

    if k == kinds.chat_create and type(body.chat_id) == "string" then
      return body
    end

    -- The openai-provider binary doesn't speak the UI-shaped prompt
    -- contract: prompts arrive via tool.invoke + the binary's own
    -- chat.complete flow. Drop on delivery so a stale fan-out wiring
    -- can't accidentally re-introduce the legacy path. Single-chat
    -- cancel still uses chat.interrupt below.
    if k == "chat.input.submit" or k == "chat.interrupt_all" then
      return nil
    elseif k == "chat.interrupt" then
      body.kind = kinds.interrupt
      return body
    elseif k == "chat.reset" then
      body.kind = kinds.reset
      return body
    elseif k == "chat.auth.set" then
      if body.provider ~= name then return nil end
      return { kind = kinds.auth_set, token = body.token }
    elseif k == "chat.login_requested" then
      if body.provider ~= name then return nil end
      return { kind = kinds.login_requested }
    elseif k == "chat.logout_requested" then
      if body.provider ~= name then return nil end
      return { kind = kinds.logout_requested }
    elseif k == "chat.model.list_requested" then
      if body.provider ~= name then return nil end
      return { kind = kinds.models_list_requested }
    elseif k == "chat.model.set" then
      if body.provider ~= name then return nil end
      -- Return the bare provider+model body; the caller threads any
      -- active chat_id in before handing to deliver.
      return {
        kind  = kinds.model_set,
        model = body.model,
      }
    elseif k == "chat.reasoning.set" then
      if body.provider ~= name then return nil end
      return {
        kind   = kinds.reasoning_set,
        effort = body.effort or body.reasoning_effort,
      }
    elseif k == "chat.compaction.request" then
      -- Universal compaction is routed from conversation-manager
      -- projection deltas by the compositor.
      return nil
    end

    -- The provider process remains the final authority on accepted wire kinds.
    return body
  end

  local function publish(from, body)
    nefor.engine.send(json.encode({
      type = "event",
      from = from,
      ts   = nefor.engine.now(),
      body = body,
    }))
  end

  local function deliver(body)
    nefor.engine.deliver(name, json.encode({
      type = "event",
      from = "engine",
      ts   = nefor.engine.now(),
      body = body,
    }))
  end

  -- Once, when the binary's <prefix>.ready first arrives and
  -- opts.static_token is set, deliver an auth.set direct to the peer
  -- (don't pollute the bus log; auth.set is a targeted control
  -- envelope). Idempotent — second ready is a no-op.
  -- Returns true if an injection happened, false otherwise.
  local function maybe_inject_static_token(env, opts)
    if injected_static then return false end
    if type(env) ~= "table" or type(env.body) ~= "table" then return false end
    if env.body.kind ~= kinds.ready then return false end
    if type(opts) ~= "table" then return false end
    local token = opts.static_token
    if token == nil then return false end
    injected_static = true
    nefor.engine.deliver(name, json.encode({
      type = "event", from = "engine", ts = nefor.engine.now(),
      body = { kind = kinds.auth_set, token = token },
    }))
    return true
  end

  -- Provider-owned cancel helper. Cancellation is keyed by the
  -- completion's chat_id — the caller-supplied request id: the binary
  -- runs at most one in-flight completion per chat, so the chat_id IS
  -- the request handle (no parallel id is invented). Owns the
  -- `<prefix>.chat.cancel` envelope shape once so factories call
  -- `cancel(request_id)` instead of hand-rolling the body. It is the
  -- honor side of the runtime's cancel protocol: fire-and-forget,
  -- idempotent on the binary side (an unknown or already-finished
  -- request id is a logged no-op there).
  local function cancel(request_id)
    assert(type(request_id) == "string" and #request_id > 0,
      "openai-provider.cancel: request_id (chat_id) required")
    return { kind = kinds.chat_cancel, chat_id = request_id }
  end

  local function context_message(message)
    local out = clone_table(message)
    if type(out) ~= "table" or out.role ~= "assistant"
        or type(out.tool_calls) ~= "table" then
      return out
    end
    local calls = {}
    for _, call in ipairs(out.tool_calls) do
      if type(call) == "table" and type(call.id) == "string"
          and type(call.name) == "string" then
        local arguments = call.arguments
        if type(arguments) ~= "string" then
          local ok, encoded = pcall(json.encode, arguments == nil and {} or arguments)
          arguments = ok and encoded or "{}"
        end
        calls[#calls + 1] = {
          id = call.id,
          type = "function",
          ["function"] = { name = call.name, arguments = arguments },
        }
      else
        calls[#calls + 1] = clone_table(call)
      end
    end
    out.tool_calls = calls
    return out
  end

  -- Lower one provider-neutral invocation plus a manager-owned conversation
  -- view into the process's private completion wire shape. The returned table
  -- is for nefor.engine.deliver only; publishing it would duplicate history.
  local function complete(request, context)
    assert(type(request) == "table", "openai-provider.complete: request required")
    assert(type(request.request_id) == "string" and request.request_id ~= "",
      "openai-provider.complete: request_id required")
    assert(type(context) == "table" and type(context.messages) == "table",
      "openai-provider.complete: conversation context required")

    local body = clone_table(request)
    body.kind = kinds.completion_request
    body.provider = nil
    body.watermark = nil
    body.messages = {}
    for index, message in ipairs(context.messages) do
      body.messages[index] = context_message(message)
    end
    body.conversation_context = nil
    return body
  end

  local function cancel_completion(request_id)
    assert(type(request_id) == "string" and request_id ~= "",
      "openai-provider.cancel_completion: request_id required")
    return { kind = kinds.completion_cancel, request_id = request_id }
  end

  local function compact_context(_)
    return nil, "context compaction is not supported by " .. name
  end

  return {
    name                       = name,
    kinds                      = kinds,
    outbound                   = outbound,
    inbound                    = inbound,
    publish                    = publish,
    deliver                    = deliver,
    complete                   = complete,
    cancel_completion          = cancel_completion,
    cancel                     = cancel,
    compact_context            = compact_context,
    context_message            = context_message,
    maybe_inject_static_token  = maybe_inject_static_token,
  }
end

return M
