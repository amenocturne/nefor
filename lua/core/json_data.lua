-- Shape-preserving ownership for JSON-shaped Lua values.
--
-- mlua marks decoded arrays with a private metatable so `[]` remains
-- distinguishable from `{}`. Ordinary table copies discard that identity and
-- can silently change protocol values when they are encoded again.

local M = {}

local json = type(nefor) == "table" and nefor.json or nil

local function is_array(value)
  if type(value) ~= "table" or type(json) ~= "table"
      or type(json.is_array) ~= "function" then return false end
  local ok, result = pcall(json.is_array, value)
  return ok and result == true
end

local function copy(value, seen, on_table)
  if type(value) ~= "table" then return value end
  seen = seen or {}
  if seen[value] then return seen[value] end

  local out = {}
  seen[value] = out
  if on_table then on_table(value, out) end
  for key, item in pairs(value) do
    out[copy(key, seen, on_table)] = copy(item, seen, on_table)
  end
  if is_array(value) and type(json.mark_array) == "function" then
    json.mark_array(out)
  end
  return out
end

function M.copy(value, on_table)
  return copy(value, nil, on_table)
end

function M.is_array(value)
  return is_array(value)
end

return M
