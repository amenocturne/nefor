local kinds = require("kinds")
local node_approval = require("node-approval")

local M = {}
local INPUT = "nefor.dynamic.Input"
local INDEXED = "nefor.dynamic.Indexed"
local COMPLETE = "nefor.dynamic.Complete"
local item = { kind = "variable", name = "T" }
local dynamic = { kind = "named", name = "nefor.dynamic.DynamicList", arguments = { item } }
local indexed = { kind = "named", name = "nefor.dynamic.Indexed", arguments = { item } }
local count = { kind = "named", name = "nefor.dynamic.DynamicCount", arguments = {} }

M.declaration = {
  name = "dynamic-each",
  -- Starts one instance of a template per item; what those instances do is
  -- declared by the template's own nodes, which are assessed with the workflow.
  approval = node_approval.inert("dynamic fan-out"),
  type_variables = { "T" },
  semantic = {
    input = dynamic,
    output = { kind = "primitive", name = "JsonValue" },
    inputs = { { wire = INPUT, type = dynamic } },
    outputs = {
      { wire = INDEXED, type = indexed },
      { wire = COMPLETE, type = count },
    },
  },
  params = {},
  inputs = { effect = INPUT },
  outputs = { INDEXED, COMPLETE },
  signals = { "kill", "drain" },
}

function M.construct(id, _, emit)
  local active, next_index, finished = nil, 0, false
  local instance = { id = id }

  local function failure(kind, protocol)
    finished = true
    return { status = "failed", failure = kinds.Failed,
      value = { kind = kind, actor = id, protocol = protocol } }
  end

  function instance.deliver(activation)
    local message = (((activation or {}).messages or {})[1] or {}).message or {}
    local protocol = message.dynamic
    if finished or type(protocol) ~= "table" or type(protocol.collection) ~= "string" then
      return failure("dynamic_each_invalid_protocol", protocol)
    end
    if active == nil then active = protocol.collection end
    if protocol.collection ~= active then
      return failure("dynamic_each_collection_changed", protocol)
    end
    if protocol.kind == "item" then
      if protocol.index ~= next_index or message.value == nil then
        return failure("dynamic_each_noncontiguous_item", protocol)
      end
      emit({ kind = INDEXED, from = id, value = {
        collection = active, index = next_index, value = message.value,
      }, semantic_value = {
        collection = active, index = next_index,
        value = message.semantic_value or message.value,
      } })
      next_index = next_index + 1
      return nil
    end
    if protocol.kind == "complete" then
      if protocol.count ~= next_index then
        return failure("dynamic_each_wrong_final_count", protocol)
      end
      emit({ kind = COMPLETE, from = id,
        value = { collection = active, count = next_index } })
      finished = true
      return { status = "ok" }
    end
    return failure("dynamic_each_unknown_event", protocol)
  end

  function instance.handle_kill() finished = true end
  function instance.handle_drain()
    if not finished then
      emit({ kind = kinds.failed, from = id, failure = kinds.Failed,
        value = { kind = "dynamic_each_drained_incomplete", actor = id } })
    end
    finished = true
  end

  emit({ kind = kinds.ready, from = id })
  return instance
end

return M
