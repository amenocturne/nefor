local M = {}

local MAX_COMPONENT = 2147483647

local function valid_component(value)
  return type(value) == "number" and value % 1 == 0
    and value >= 1 and value <= MAX_COMPONENT
end

function M.valid(path, allow_root)
  if type(path) ~= "table" then return false end
  if not allow_root and #path == 0 then return false end
  for index = 1, #path do
    if not valid_component(path[index]) then return false end
  end
  return true
end

function M.copy(path)
  local out = {}
  for index, component in ipairs(path or {}) do out[index] = component end
  return out
end

function M.key(path)
  if not M.valid(path, true) then return nil end
  return table.concat(path, ".")
end

function M.parent(path)
  if not M.valid(path) then return nil end
  local out = M.copy(path)
  out[#out] = nil
  return out
end

function M.is_prefix(prefix, path)
  if not M.valid(prefix, true) or not M.valid(path, true) or #prefix > #path then
    return false
  end
  for index = 1, #prefix do
    if prefix[index] ~= path[index] then return false end
  end
  return true
end

function M.prefixes(path)
  if not M.valid(path, true) then return nil end
  local out = {}
  for length = 1, #path do
    local prefix = {}
    for index = 1, length do prefix[index] = path[index] end
    out[#out + 1] = prefix
  end
  return out
end

function M.allocate(parent, child_max)
  parent = parent or {}
  if not M.valid(parent, true) then return nil, "invalid_history_parent" end
  local parent_key = M.key(parent)
  local component = (child_max[parent_key] or 0) + 1
  if component > MAX_COMPONENT then return nil, "history_component_exhausted" end
  local out = M.copy(parent)
  out[#out + 1] = component
  return out
end

M.MAX_COMPONENT = MAX_COMPONENT

return M
