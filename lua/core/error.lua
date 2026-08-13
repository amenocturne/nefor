-- Stable normalization and display for errors crossing untyped Lua boundaries.

local M = {}

local function scalar(value)
  local kind = type(value)
  if kind == "string" then return value end
  if kind == "number" or kind == "boolean" then return tostring(value) end
  return nil
end

local function detail_text(value, seen)
  local direct = scalar(value)
  if direct ~= nil then return direct end
  if type(value) ~= "table" then return type(value) end
  seen = seen or {}
  if seen[value] then return "<cycle>" end
  seen[value] = true
  local keys = {}
  for key in pairs(value) do keys[#keys + 1] = key end
  table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
  local parts = {}
  for _, key in ipairs(keys) do
    parts[#parts + 1] = tostring(key) .. "=" .. detail_text(value[key], seen)
  end
  seen[value] = nil
  return table.concat(parts, ", ")
end

function M.normalize(value, fallback_code, fallback_message)
  fallback_code = fallback_code or "unknown_error"
  fallback_message = fallback_message or "unknown error"
  if type(value) ~= "table" then
    return {
      code = fallback_code,
      message = scalar(value) or fallback_message,
    }
  end

  local nested = type(value.error) == "table" and value.error or nil
  local code = scalar(value.code) or (nested and scalar(nested.code)) or fallback_code
  local message = scalar(value.message)
    or (nested and scalar(nested.message))
    or fallback_message
  local detail = value.detail or value.context
    or (nested and (nested.detail or nested.context))
  return {
    code = code,
    message = message,
    detail = detail,
  }
end

function M.display(value, fallback_code, fallback_message)
  local normalized = M.normalize(value, fallback_code, fallback_message)
  local text = normalized.code .. ": " .. normalized.message
  if normalized.detail ~= nil then
    text = text .. " (" .. detail_text(normalized.detail) .. ")"
  end
  return text
end

return M
