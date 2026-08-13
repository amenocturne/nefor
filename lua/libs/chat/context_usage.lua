-- Token-usage projections for the chat surface.

local M = {}

function M.current_input_tokens(usage)
  if type(usage) ~= "table" then return nil end
  return usage.context_input_tokens or usage.input_tokens or usage.prompt_tokens
end

return M
