local kinds = require("kinds")

local M = {}
local VALUE = "nefor.graph.Value"
local variable = { kind = "variable", name = "T" }

M.declaration = {
  name = "source",
  type_variables = { "T" },
  semantic = {
    input = { kind = "primitive", name = "Unit" },
    output = variable,
    inputs = { { wire = kinds.Unit, type = { kind = "primitive", name = "Unit" } } },
    outputs = { { wire = VALUE, type = variable } },
  },
  params = { value = "data" },
  inputs = { start = kinds.Unit },
  outputs = { VALUE },
  signals = {},
}

function M.construct(id, params, emit)
  local instance = { id = id }

  function instance.deliver()
    emit({ kind = VALUE, from = id, value = params.value })
    return { status = "ok" }
  end

  emit({ kind = kinds.ready, from = id })
  return instance
end

return M
