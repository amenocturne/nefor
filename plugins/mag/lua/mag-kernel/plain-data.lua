-- Serializable plain-data ownership for kernel interfaces.
local M = {}
local json_data = require("core.json_data")

local function fail(path, message)
  return nil, path .. ": " .. message
end

local function is_json_null(value)
  if type(value) ~= "userdata" then return false end
  local json = type(nefor) == "table" and type(nefor.json) == "table" and nefor.json or nil
  if not json or type(json.is_null) ~= "function" then return false end
  local ok, result = pcall(json.is_null, value)
  return ok and result == true
end

local function has_json_array_identity(value)
  local json = type(nefor) == "table" and type(nefor.json) == "table" and nefor.json or nil
  if not json or type(json.is_array) ~= "function" then return false end
  local ok, result = pcall(json.is_array, value)
  return ok and result == true
end

local function validate(value, path, visiting)
  local kind = type(value)
  if kind == "userdata" and is_json_null(value) then return true end
  if kind == "function" or kind == "thread" or kind == "userdata" then
    return fail(path, kind .. " values are not serializable")
  end
  if kind == "number" and (value ~= value or value == math.huge or value == -math.huge) then
    return fail(path, "non-finite numbers are not serializable")
  end
  if kind ~= "table" then return true end
  if getmetatable(value) ~= nil and not has_json_array_identity(value) then
    return fail(path, "metatables are not serializable plain data")
  end
  visiting = visiting or {}
  if visiting[value] then return fail(path, "cycle in plain data") end
  visiting[value] = true
  for key, child in pairs(value) do
    if type(key) ~= "string" and type(key) ~= "number" then
      visiting[value] = nil
      return fail(path, "table keys must be strings or positive array indices")
    end
    if type(key) == "number" and (key < 1 or key % 1 ~= 0) then
      visiting[value] = nil
      return fail(path, "numeric table keys must be positive array indices")
    end
    local ok, err = validate(child, path .. "." .. tostring(key), visiting)
    if not ok then visiting[value] = nil; return nil, err end
  end
  visiting[value] = nil
  return true
end

function M.copy(value)
  return json_data.copy(value)
end

function M.owned(value, path)
  local ok, err = validate(value, path or "value")
  if not ok then return nil, err end
  return json_data.copy(value)
end

return M
