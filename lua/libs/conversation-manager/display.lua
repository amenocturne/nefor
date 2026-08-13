local M = {}

local function semantic_name(type_tag)
  if type(type_tag) ~= "table" then return nil end
  local root = type_tag.root
  if type(root) ~= "table" or root.kind ~= "named" then return nil end
  return root.name
end

function M.structured_text(data)
  if type(data) ~= "table" then return nil end
  if semantic_name(data.mag_type) ~= "nefor.contracts.Task" then return nil end
  local value = data.value
  if type(value) ~= "table" or type(value.prompt) ~= "string" then return nil end
  return value.prompt
end

return M
