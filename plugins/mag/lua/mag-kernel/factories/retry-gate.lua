local kinds = require("kinds")

local M = {}
local CONTINUE = "nefor.retry.Continue"
local EXHAUSTED = "nefor.retry.Exhausted"
local variable = { kind = "variable", name = "T" }
local function branch(name)
  return { kind = "named", name = name, arguments = { variable } }
end

M.declaration = {
  name = "retry-gate",
  type_variables = { "T" },
  semantic = {
    input = variable,
    output = { kind = "union", items = {
      branch("nefor.contracts.Continue"), branch("nefor.contracts.Exhausted"),
    } },
    inputs = { { wire = "nefor.retry.Input", type = variable } },
    outputs = {
      { wire = CONTINUE, type = branch("nefor.contracts.Continue") },
      { wire = EXHAUSTED, type = branch("nefor.contracts.Exhausted") },
    },
  },
  params = { max_retries = "int" },
  inputs = { value = "nefor.retry.Input" },
  outputs = { CONTINUE, EXHAUSTED },
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

    local output = attempts < maximum and CONTINUE or EXHAUSTED
    attempts = attempts + 1
    if output == EXHAUSTED then exhausted = true end
    emit({
      kind = output,
      from = id,
      value = message.value,
      semantic_value = { value = message.value },
    })
    return { status = "ok" }
  end

  emit({ kind = kinds.ready, from = id })
  return instance
end

return M
