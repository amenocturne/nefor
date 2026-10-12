-- Exact completed-response measurements; operation billing is not occupancy.
local M = {}
function M.integer(value)
  return type(value) == "number" and value >= 0 and value < math.huge
    and value == math.floor(value)
end
function M.measurement(value)
  if type(value) ~= "table" or value.kind ~= "completed_response"
      or not M.integer(value.total_tokens) then return nil end
  for _, key in ipairs({ "input_tokens", "output_tokens" }) do
    if value[key] ~= nil and not M.integer(value[key]) then return nil end
  end
  return value
end
function M.total(value)
  local measurement = M.measurement(value)
  return measurement and measurement.total_tokens or nil
end
function M.from_response(usage)
  if type(usage) ~= "table" or usage.billing_components ~= nil
      or usage.accuracy == "estimated" or usage.context_input_accuracy == "estimated" then return nil end
  local input = usage.input_tokens or usage.prompt_tokens
  local output = usage.output_tokens or usage.completion_tokens
  local total = usage.total_tokens
  if not M.integer(total) and M.integer(input) and M.integer(output) then total = input + output end
  if not M.integer(total) then return nil end
  return M.measurement({ kind = "completed_response", total_tokens = total,
    input_tokens = input, output_tokens = output })
end
return M
