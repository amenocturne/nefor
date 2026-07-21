local kinds = require("kinds")

local M = {}
local VALUE = "nefor.graph.Value"
local variable = { kind = "variable", name = "T" }

M.declaration = {
  name = "output",
  type_variables = { "T" },
  semantic = {
    input = variable,
    output = variable,
    inputs = { { wire = VALUE, type = variable } },
    outputs = { { wire = VALUE, type = variable } },
  },
  params = {},
  inputs = { value = VALUE },
  outputs = { VALUE },
  signals = {},
}

function M.construct(id, _, emit)
  local instance = { id = id }

  function instance.deliver(activation)
    local message = ((activation or {}).messages or {})[1]
    local forwarded = {}
    for key, value in pairs((message and message.message) or {}) do
      forwarded[key] = value
    end
    forwarded.kind = VALUE
    forwarded.from = id
    emit(forwarded)
    return { status = "ok" }
  end

  emit({ kind = kinds.ready, from = id })
  return instance
end

return M
