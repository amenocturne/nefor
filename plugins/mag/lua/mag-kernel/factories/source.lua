local kinds = require("kinds")

local M = {}
local preview_components = require("preview-components")
local VALUE = "nefor.graph.Value"
local variable = { kind = "variable", name = "T" }

M.declaration = {
  preview = preview_components.source(),
  name = "source",
  type_variables = { "T" },
  semantic = {
    input = { kind = "primitive", name = "Unit" },
    output = variable,
    inputs = { { wire = kinds.Unit, type = { kind = "primitive", name = "Unit" } } },
    outputs = { { wire = VALUE, type = variable } },
  },
  params = { value = "data", value_type = "semantic-type-id" },
  inputs = { start = kinds.Unit },
  outputs = { VALUE },
  signals = {},
}

function M.construct(id, params, emit)
  local instance = { id = id }

  function instance.deliver()
    emit({
      kind = VALUE,
      from = id,
      value = params.value,
      semantic_type_id = params.value_type,
    })
    return { status = "ok" }
  end

  emit({ kind = kinds.ready, from = id })
  return instance
end

return M
