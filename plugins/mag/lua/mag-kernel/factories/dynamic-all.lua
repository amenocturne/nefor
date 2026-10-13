local kinds = require("kinds")
local node_approval = require("node-approval")

local M = {}
local INPUT = "nefor.dynamic.Input"
local OUTPUT = "generic-provider.ProviderOut"
local item = { kind = "variable", name = "T" }
local dynamic = { kind = "named", name = "nefor.dynamic.DynamicList", arguments = { item } }
local provider_input = {
  kind = "named", name = "nefor.contracts.ProviderInput", arguments = {},
}

M.declaration = {
  name = "dynamic-all",
  -- Collects the results of a dynamic workflow; plumbing.
  approval = node_approval.inert("dynamic collector"),
  type_variables = { "T" },
  semantic = {
    input = dynamic,
    output = provider_input,
    inputs = { { wire = INPUT, type = dynamic } },
    outputs = { { wire = OUTPUT, type = provider_input } },
  },
  params = { item_schema = "table" },
  inputs = { effect = INPUT },
  outputs = { OUTPUT },
  signals = { "kill", "drain" },
}

function M.construct(id, params, emit)
  if type(params) ~= "table" or type(params.item_schema) ~= "table"
      or type(params.item_schema.root) ~= "table" then
    return nil, string.format("dynamic-all '%s': item_schema is required", tostring(id))
  end
  local collection, next_index, values, finished = nil, 0, {}, false
  local instance = { id = id }

  local function failure(kind, protocol)
    finished = true
    values = {}
    return { status = "failed", failure = kinds.Failed,
      value = { kind = kind, actor = id, protocol = protocol } }
  end

  function instance.deliver(activation)
    local message = (((activation or {}).messages or {})[1] or {}).message or {}
    local protocol = message.dynamic
    if finished or type(protocol) ~= "table" or type(protocol.collection) ~= "string" then
      return failure("dynamic_all_invalid_protocol", protocol)
    end
    if collection == nil then collection = protocol.collection end
    if protocol.collection ~= collection then
      return failure("dynamic_all_collection_changed", protocol)
    end
    if protocol.kind == "item" then
      if protocol.index ~= next_index or message.value == nil then
        return failure("dynamic_all_noncontiguous_item", protocol)
      end
      values[#values + 1] = message.semantic_value or message.value
      next_index = next_index + 1
      return nil
    end
    if protocol.kind ~= "complete" or protocol.count ~= next_index then
      return failure("dynamic_all_invalid_completion", protocol)
    end

    local content = {
      mag_type = { version = params.item_schema.version or 1,
        root = { kind = "list", item = params.item_schema.root } },
      value = nefor.json.mark_array(values),
    }
    emit({ kind = OUTPUT, from = id, value = { content = content },
      messages = { { role = "user", content = content } } })
    finished = true
    values = {}
    return { status = "ok" }
  end

  function instance.handle_kill()
    finished = true
    values = {}
  end
  function instance.handle_drain()
    if not finished then
      emit({ kind = kinds.failed, from = id, failure = kinds.Failed,
        value = { kind = "dynamic_all_drained_incomplete", actor = id } })
    end
    finished = true
    values = {}
  end

  emit({ kind = kinds.ready, from = id })
  return instance
end

return M
