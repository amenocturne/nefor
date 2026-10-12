local M = {}
local policy = require("libs.compaction-policy")
local json_data = require("core.json_data")

local function copy_json_object(value, label)
  if type(value) ~= "table" then
    return nil, label .. " must be a JSON object when present"
  end
  local encoded, decoded
  local ok
  ok, encoded = pcall(nefor.json.encode, value)
  if not ok then return nil, label .. " must contain only JSON values" end
  ok, decoded = pcall(nefor.json.decode, encoded)
  if not ok or type(decoded) ~= "table" or json_data.is_array(decoded) then
    return nil, label .. " must be a JSON object when present"
  end
  return decoded
end

local function copy_resolved_model(model, label)
  if type(model) ~= "table" then
    return nil, label .. " must be a table"
  end
  local copy = {}
  for key, value in pairs(model) do
    if key ~= "provider" and key ~= "model" and key ~= "reasoning_effort"
        and key ~= "provider_options" and key ~= "compaction_policy" then
      return nil, label .. " has unknown field " .. tostring(key)
    end
    copy[key] = value
  end
  if type(copy.provider) ~= "string" or copy.provider == "" then
    return nil, label .. " provider must be a non-empty string"
  end
  if type(copy.model) ~= "string" or copy.model == "" then
    return nil, label .. " model must be a non-empty string"
  end
  if copy.reasoning_effort ~= nil
      and (type(copy.reasoning_effort) ~= "string" or copy.reasoning_effort == "") then
    return nil, label .. " reasoning_effort must be a non-empty string when present"
  end
  if copy.provider_options ~= nil then
    local options, options_error = copy_json_object(
      copy.provider_options, label .. " provider_options")
    if options == nil then return nil, options_error end
    copy.provider_options = options
  end
  local validated, policy_error = policy.validate(copy.compaction_policy)
  if validated == nil then return nil, label .. " " .. policy_error end
  copy.compaction_policy = policy.raw(validated)
  return copy
end

function M.copy(snapshot)
  if type(snapshot) ~= "table" then
    return nil, "model snapshot callback must return a table"
  end
  local current = {}
  for key, value in pairs(snapshot) do
    if key ~= "provider" and key ~= "model" and key ~= "reasoning_effort"
        and key ~= "provider_options" and key ~= "compaction_policy"
        and key ~= "profiles" then
      return nil, "model snapshot has unknown field " .. tostring(key)
    end
    if key ~= "profiles" then current[key] = value end
  end
  local copy, current_error = copy_resolved_model(current, "model snapshot")
  if copy == nil then return nil, current_error end
  if snapshot.profiles == nil then return copy end
  if type(snapshot.profiles) ~= "table" then
    return nil, "model snapshot profiles must be a table when present"
  end
  copy.profiles = {}
  for name, model in pairs(snapshot.profiles) do
    if type(name) ~= "string" or name == "" then
      return nil, "model snapshot profile names must be non-empty strings"
    end
    local resolved, profile_error = copy_resolved_model(
      model, "model snapshot profile " .. string.format("%q", name))
    if resolved == nil then return nil, profile_error end
    copy.profiles[name] = resolved
  end
  return copy
end

return M
