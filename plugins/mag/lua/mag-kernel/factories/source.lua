local kinds = require("kinds")
local node_approval = require("node-approval")

local M = {}
local VALUE = "nefor.graph.Value"
local variable = { kind = "variable", name = "T" }

M.declaration = {
  name = "source",
  -- Emits an authored constant.
  approval = node_approval.inert("source"),
  type_variables = { "T" },
  semantic = {
    input = { kind = "primitive", name = "Unit" },
    output = variable,
    inputs = { { wire = kinds.Unit, type = { kind = "primitive", name = "Unit" } } },
    outputs = { { wire = VALUE, type = variable } },
  },
  params = { value = "data", value_type = "semantic-type-id" },
  template = { relocations = {} },
  inputs = { start = kinds.Unit },
  outputs = { VALUE },
  signals = {},
}

function M.construct(id, params, emit)
  local instance = { id = id }

  function instance.deliver()
    local value = params.value
    if type(value) == "table" and value.type == params.value_type
        and value.value ~= nil then
      value = value.value
    end
    emit({
      kind = VALUE,
      from = id,
      value = value,
      semantic_type_id = params.value_type,
    })
    return { status = "ok" }
  end

  emit({ kind = kinds.ready, from = id })
  return instance
end

return M
