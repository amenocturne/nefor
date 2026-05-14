-- chatgpt-provider Lua translator.
--
-- The chatgpt-provider Rust binary emits the same prefixed event kinds
-- as openai-provider (chat.create/append/complete, stream.delta,
-- auth.status, etc.), so the openai-provider Lua translator works
-- byte-for-byte against our wire. This module is a thin re-export so
-- the compositor can load us by name (`require("chatgpt-provider")`)
-- without learning about openai-provider directly.

local oa = require("openai-provider")

return {
  translator     = oa.translator,
  replay_rebuild = oa.replay_rebuild,
}
