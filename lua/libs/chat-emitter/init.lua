-- Scoped chat message emitter.
--
-- Binds a chat_id at construction time so downstream code cannot
-- forget to set it. All chat.message.append emissions go through
-- this interface; raw body construction is not needed.

local M = {}

function M.scoped(chat_id, emit_fn)
  assert(type(emit_fn) == "function", "chat-emitter: emit_fn must be a function")

  local E = {}

  function E.system(text, opts)
    if type(text) ~= "string" or #text == 0 then return end
    opts = opts or {}
    emit_fn({
      kind    = "chat.message.append",
      role    = "system",
      text    = text,
      chat_id = chat_id,
      path    = opts.path,
      dir     = opts.dir,
    })
  end

  function E.chat_id()
    return chat_id
  end

  return E
end

return M
