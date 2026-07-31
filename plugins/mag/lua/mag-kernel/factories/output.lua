local kinds = require("kinds")

local M = {}
local preview_components = require("preview-components")
local VALUE = "nefor.graph.Value"
local variable = { kind = "variable", name = "T" }

M.declaration = {
  preview = preview_components.output(),
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
    if activation and activation.shape == "product" and not activation.whole then
      local values, semantic_values = {}, {}
      local transcript_delta
      for index, position in ipairs(activation.messages or {}) do
        local message = position.message or {}
        local value = message.value
        if value == nil then value = message end
        local metadata = position.arrival and position.arrival.control_metadata
        if metadata and metadata.transcript_delta ~= nil then
          transcript_delta = metadata.transcript_delta
        end
        values[index] = value
        semantic_values[index] = value
        local arrival = position.arrival
        local component = arrival and arrival.declared_type
          and (arrival.declared_type.items or {})[index]
        if type(component) == "table" and component.kind == "union" then
          semantic_values[index] = {
            type = arrival.constructor_id,
            value = value,
          }
        end
      end
      emit({
        kind = VALUE,
        from = id,
        value = values,
        semantic_value = semantic_values,
        transcript_delta = transcript_delta,
      })
      return { status = "ok" }
    end
    local message = ((activation or {}).messages or {})[1]
    local arrival = message and message.arrival
    local forwarded = {}
    for key, value in pairs((message and message.message) or {}) do
      forwarded[key] = value
    end
    if arrival then
      forwarded.semantic_type_id = arrival.constructor_id or arrival.type_id
      forwarded.semantic_type = arrival.type
      forwarded.constructor_id = arrival.constructor_id
      forwarded.arrival_id = arrival.arrival_id
      local metadata = arrival.control_metadata
      if metadata and metadata.transcript_delta ~= nil then
        forwarded.transcript_delta = metadata.transcript_delta
      end
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
