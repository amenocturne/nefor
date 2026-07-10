-- chatgpt-provider Lua translator.
--
-- The chatgpt-provider Rust binary emits the same prefixed event kinds
-- as openai-provider (chat.create/append/complete, stream.delta,
-- auth.status, etc.), so the openai-provider Lua translator works
-- byte-for-byte against our wire. This module is a thin re-export so
-- the compositor can load us by name (`require("chatgpt-provider")`)
-- without learning about openai-provider directly.

local oa = require("openai-provider")

-- Wrap the shared translator to add a provider-owned cancel helper.
--
-- Cancellation for this provider is keyed by the completion's chat_id —
-- the caller-supplied request id: the binary runs at most one in-flight
-- completion per chat, so the chat_id IS the request handle (no parallel
-- id is invented). The helper owns the `<prefix>.chat.cancel` envelope
-- shape once so factories call `cancel(request_id)` instead of
-- hand-rolling the body. It is the honor side of the runtime's cancel
-- protocol: fire-and-forget, idempotent on the binary side (an unknown
-- or already-finished request id is a logged no-op there).
local function translator(name)
  local t = oa.translator(name)
  local chat_cancel_kind = name .. ".chat.cancel"
  local usage_requested_kind = name .. ".usage.requested"
  local usage_updated_kind = name .. ".usage.updated"
  local usage_error_kind = name .. ".usage.error"
  local base_outbound = t.outbound
  local base_inbound = t.inbound
  t.kinds.chat_cancel = chat_cancel_kind
  t.kinds.usage_requested = usage_requested_kind
  t.kinds.usage_updated = usage_updated_kind
  t.kinds.usage_error = usage_error_kind
  t.cancel = function(request_id)
    assert(type(request_id) == "string" and #request_id > 0,
      "chatgpt-provider.cancel: request_id (chat_id) required")
    return { kind = chat_cancel_kind, chat_id = request_id }
  end
  t.outbound = function(env)
    local kind = type(env) == "table"
      and type(env.body) == "table"
      and env.body.kind
      or nil
    if kind == usage_updated_kind or kind == usage_error_kind then
      local body = {}
      for k, v in pairs(env.body) do body[k] = v end
      body.kind = (kind == usage_updated_kind) and "chat.usage.updated" or "chat.usage.error"
      body.provider = name
      return body
    end
    return base_outbound(env)
  end
  t.inbound = function(env)
    local body = type(env) == "table" and env.body or nil
    if type(body) == "table" and body.kind == "chat.usage.requested" then
      if body.provider ~= name then return nil end
      return { kind = usage_requested_kind }
    end
    return base_inbound(env)
  end
  return t
end

return {
  translator     = translator,
  replay_rebuild = oa.replay_rebuild,
}
