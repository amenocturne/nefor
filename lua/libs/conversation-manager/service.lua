local manager = require("libs.conversation-manager")
local domain = manager.domain
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

  function reader:context(conversation_id, options)
    return projection.context(store:peek(conversation_id), options)
  end

  function reader:read(args)
    local context = self:context(args.conversation_id, { provider = args.provider, model = args.model })
    if not context then return nil, { code = "conversation_not_found" } end
    if type(args.min_watermark) == "number" and context.watermark < args.min_watermark then
      return nil, { code = "conversation_watermark_stale" }
    end
    return { context = context, history_id = domain.copy(context.history_id), watermark = context.watermark,
      policy_state = domain.copy(context.policy_state), latest_response_usage = domain.copy(context.latest_response_usage),
      pending_compaction = domain.copy(context.pending_compaction) }
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
