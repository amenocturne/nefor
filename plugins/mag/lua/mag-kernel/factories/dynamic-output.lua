local kinds = require("kinds")
local node_approval = require("node-approval")

local M = {}
local INPUT = "nefor.dynamic.Collect"
local OUTPUT = "nefor.dynamic.Output"
local item = { kind = "variable", name = "T" }
local dynamic = { kind = "named", name = "nefor.dynamic.DynamicList", arguments = { item } }
local indexed = { kind = "named", name = "nefor.dynamic.Indexed", arguments = { item } }
local count = { kind = "named", name = "nefor.dynamic.DynamicCount", arguments = {} }

M.declaration = {
  name = "dynamic-output",
  -- Collects streamed items; plumbing.
  approval = node_approval.inert("dynamic output"),
  type_variables = { "T", "R" },
  semantic = {
    input = { kind = "variable", name = "R" },
    output = dynamic,
    inputs = { { wire = INPUT, type = { kind = "variable", name = "R" } } },
    outputs = { { wire = OUTPUT, type = dynamic } },
  },
  params = {},
  inputs = { value = INPUT },
  outputs = { OUTPUT },
  signals = { "kill", "drain" },
}

function M.construct(id, _, emit)
  local collection, count_value, values, semantic_values = nil, nil, {}, {}
  local received, finished = 0, false
  local instance = { id = id }

  local function failure(kind, value)
    finished = true
    values, semantic_values = {}, {}
    return { status = "failed", failure = kinds.Failed,
      value = { kind = kind, actor = id, value = value } }
  end

  local function finish_if_ready()
    if count_value == nil or received ~= count_value then return nil end
    for index = 0, count_value - 1 do
      if values[index] == nil then return nil end
    end
    for index = 0, count_value - 1 do
      emit({ kind = OUTPUT, from = id, value = values[index],
        semantic_value = semantic_values[index], dynamic = {
          kind = "item", collection = collection, index = index,
        } })
    end
    emit({ kind = OUTPUT, from = id, dynamic = {
      kind = "complete", collection = collection, count = count_value,
    } })
    finished = true
    values, semantic_values = {}, {}
    return { status = "ok" }
  end

  function instance.deliver(activation)
    local message = (((activation or {}).messages or {})[1] or {}).message or {}
    local envelope = message.value
    local value = type(envelope) == "table" and envelope.value or nil
    local constructor = type(envelope) == "table" and envelope.constructor or nil
    if finished or (constructor ~= "Item" and constructor ~= "Complete")
        or type(value) ~= "table" or type(value.collection) ~= "string" then
      return failure("dynamic_output_invalid_value", value)
    end
    if collection == nil then collection = value.collection end
    if value.collection ~= collection then
      return failure("dynamic_output_collection_changed", value)
    end
    if constructor == "Item" then
      if type(value.index) ~= "number" or value.index < 0 or value.index % 1 ~= 0
          or value.value == nil or values[value.index] ~= nil
          or (count_value ~= nil and value.index >= count_value) then
        return failure("dynamic_output_invalid_item", value)
      end
      local semantic_envelope = message.semantic_value
      local semantic_indexed = type(semantic_envelope) == "table"
        and semantic_envelope.value or nil
      values[value.index] = value.value
      semantic_values[value.index] = type(semantic_indexed) == "table"
        and semantic_indexed.value or value.value
      received = received + 1
    else
      if count_value ~= nil or type(value.count) ~= "number" or value.count < 0
          or value.count % 1 ~= 0 or received > value.count then
        return failure("dynamic_output_invalid_count", value)
      end
      count_value = value.count
    end
    return finish_if_ready()
  end

  function instance.handle_kill()
    finished = true
    values, semantic_values = {}, {}
  end
  function instance.handle_drain()
    if not finished then
      emit({ kind = kinds.failed, from = id, failure = kinds.Failed,
        value = { kind = "dynamic_output_drained_incomplete", actor = id } })
    end
    finished = true
    values, semantic_values = {}, {}
  end

  emit({ kind = kinds.ready, from = id })
  return instance
end

return M
