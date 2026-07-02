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
  t.kinds.chat_cancel = chat_cancel_kind
  t.cancel = function(request_id)
    assert(type(request_id) == "string" and #request_id > 0,
      "chatgpt-provider.cancel: request_id (chat_id) required")
    return { kind = chat_cancel_kind, chat_id = request_id }
  end
  return t
end

return {
  translator     = translator,
  replay_rebuild = oa.replay_rebuild,
}
