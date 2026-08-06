local manager = require("libs.conversation-manager")
local projection = require("libs.conversation-manager.projection")

local M = {}

function M.new()
  local store = manager.new()

  local writer = {}
  local reader = {}

  function writer:append(fact)
    return store:append(fact)
  end

  function writer:apply_recorded(event)
    return store:apply_recorded(event)
  end

  function writer:owned(conversation_id)
    return store:peek(conversation_id)
  end

  function writer:list_owned()
    return store:list_owned()
  end

  function writer:reset()
    store = manager.new()
  end

  function writer:stats()
    return store:stats()
  end

  function reader:conversation(conversation_id)
    return projection.conversation(store:peek(conversation_id))
  end

  function reader:context(conversation_id)
    return projection.context(store:peek(conversation_id))
  end

  function reader:watermark(conversation_id)
    local conversation = store:peek(conversation_id)
    return conversation and conversation.last_sequence or nil
  end

  function reader:list()
    local out = {}
    for _, conversation in ipairs(store:list_owned()) do
      out[#out + 1] = projection.conversation(conversation)
    end
    return out
  end

  return {
    writer = function() return writer end,
    reader = function() return reader end,
  }
end

return M
