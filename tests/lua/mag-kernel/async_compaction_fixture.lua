-- Deferred transport for consumer fixtures. Pumping is explicit; receipt effects
-- happen at settlement, never at command enqueue. This owns no production state.
local copy = require("core.json_data").copy
local M = {}
function M.new(sync)
  local queue, detached, serial = {}, {}, 0
  local fixture = { facet = {}, queue = queue }
  function fixture.facet.detach(handle) detached[handle] = true end
  for _, name in ipairs({ "barrier", "prepare", "read", "append_notice", "bind_notice",
      "acknowledge_notice", "request_compaction", "cancel_compaction" }) do
    fixture.facet[name] = function(args, done)
      serial = serial + 1
      local handle = { id = "rpc-" .. serial }
      local item = { operation = name, args = copy(args), done = done, handle = handle }
      queue[#queue + 1] = item
      return handle
    end
  end
  function fixture.settle(item, outcome)
    if item.settled then return end
    item.settled = true
    if not detached[item.handle] then item.done(outcome) end
  end
  function fixture.pump(stop)
    while #queue > 0 do
      local item = queue[1]
      if item.operation == stop and stop ~= "request_compaction" then return item end
      if item.operation == stop then
        table.remove(queue, 1)
        fixture.pending_compact = item
        if sync[item.operation] then sync[item.operation](item.args, item.handle) end
        return item
      end
      table.remove(queue, 1)
      if not detached[item.handle] then
        local value, err = sync[item.operation](item.args, item.handle)
        fixture.settle(item, value and { kind = "Ok", value = value }
          or { kind = "Error", error = err })
      end
    end
  end
  function fixture.wrap(actor)
    fixture.raw = {}
    for _, method in ipairs({ "deliver", "handle_observation", "handle_kill", "handle_drain" }) do
      local original = actor[method]
      fixture.raw[method] = original
      if original then actor[method] = function(...)
        local result = original(...)
        fixture.pump("request_compaction")
        return result
      end end
    end
    return actor
  end
  return fixture
end
function M.disabled()
  return M.new({
    barrier = function() return { watermark = 0 } end,
    prepare = function() return { kind = "Ready", capability = { kind = "supported" } } end,
    read = function() return { watermark = 0, history_id = {}, context = { history_length = 0 },
      policy_state = { epoch_id = "initial", notices = {} } } end,
  })
end
return M
