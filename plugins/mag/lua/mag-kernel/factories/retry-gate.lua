local kinds = require("kinds")
local node_approval = require("node-approval")

local M = {}
local RESULT = "nefor.retry.Result"
local value = { kind = "variable", name = "T" }
local decision = { kind = "variable", name = "R" }

M.declaration = {
  name = "retry-gate",
  -- Counts retries and routes a value; it changes nothing outside the run.
  approval = node_approval.inert("retry gate"),
  type_variables = { "T", "R" },
  semantic = {
    input = value,
    output = decision,
    inputs = {{ wire = "nefor.retry.Input", type = value }},
    outputs = {{ wire = RESULT, type = decision }},
  },
  params = { max_retries = "int" },
  inputs = { value = "nefor.retry.Input" },
  outputs = { RESULT },
  signals = {},
}

function M.construct(id, params, emit, deps)
  local maximum = params and params.max_retries
  if type(maximum) ~= "number" or maximum < 0 or maximum % 1 ~= 0 then
    return nil, string.format("retry-gate '%s': max_retries must be a non-negative integer", tostring(id))
  end

  local attempts, exhausted = 0, false
  local diagnostic = deps and deps.diagnostic
  local instance = { id = id }

  function instance.deliver(activation)
    local one = activation and activation.messages and activation.messages[1] or {}
    local message = one.message or {}
    if exhausted then
      if type(diagnostic) == "function" then
        diagnostic({ kind = "late_input_after_exhaustion", gate = id })
      end
      return { status = "ok" }
    end

    local constructor = attempts < maximum and "Continue" or "Exhausted"
    attempts = attempts + 1
    if constructor == "Exhausted" then exhausted = true end
    emit({ kind = RESULT, from = id,
      value = { constructor = constructor, value = message.value } })
    return { status = "ok" }
  end

  emit({ kind = kinds.ready, from = id })
  return instance
end

return M
