local M = {}

local function nonempty(v)
  return type(v) == "string" and v ~= ""
end

local function selector(v, where)
  if type(v) ~= "table" or not nonempty(v.arg) then
    return nil, where .. " must contain a non-empty `arg` string"
  end
  for k, _ in pairs(v) do
    if k ~= "arg" and k ~= "cwd_arg" and k ~= "default" then
      return nil, where .. " has unknown field `" .. tostring(k) .. "`"
    end
  end
  if v.cwd_arg ~= nil and not nonempty(v.cwd_arg) then
    return nil, where .. ".cwd_arg must be a non-empty string"
  end
  if v.default ~= nil and type(v.default) ~= "string" then
    return nil, where .. ".default must be a string"
  end
  return true
end

function M.validate(c)
  if type(c) ~= "table" then return nil, "display must be a table" end
  for k, _ in pairs(c) do
    if k ~= "label" and k ~= "primary" and k ~= "arguments" and k ~= "result" then
      return nil, "display has unknown field `" .. tostring(k) .. "`"
    end
  end
  if not nonempty(c.label) then
    local ok, err = selector(c.label, "display.label")
    if not ok then return nil, err end
  end
  if c.primary ~= nil then
    local ok, err = selector(c.primary, "display.primary")
    if not ok then return nil, err end
  end
  if c.arguments ~= nil then
    if type(c.arguments) ~= "table" then
      return nil, "display.arguments must be a JSON array"
    end
    local is_json_array = type(nefor) == "table"
      and type(nefor.json) == "table"
      and type(nefor.json.is_array) == "function"
      and nefor.json.is_array(c.arguments)
    local count = 0
    for key, _ in pairs(c.arguments) do
      if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then
        return nil, "display.arguments must contain only list entries"
      end
      count = count + 1
    end
    if count == 0 and not is_json_array then
      return nil, "display.arguments must be a JSON array"
    end
    for i = 1, count do
      if c.arguments[i] == nil then
        return nil, "display.arguments must not contain holes"
      end
    end
    for i, f in ipairs(c.arguments) do
      if type(f) ~= "table" or not nonempty(f.label) then
        return nil, "display.arguments[" .. i .. "] needs label and arg strings"
      end
      if not nonempty(f.arg) then
        return nil, "display.arguments[" .. i .. "] needs label and arg strings"
      end
      if f.cwd_arg ~= nil and not nonempty(f.cwd_arg) then
        return nil, "display.arguments[" .. i .. "].cwd_arg must be a non-empty string"
      end
      if f.default ~= nil and type(f.default) ~= "string" then
        return nil, "display.arguments[" .. i .. "].default must be a string"
      end
      for k, _ in pairs(f) do
        if k ~= "label" and k ~= "arg" and k ~= "cwd_arg" and k ~= "default" then
          return nil, "display.arguments[" .. i .. "] has unknown field `" .. tostring(k) .. "`"
        end
      end
    end
  end
  if type(c.result) ~= "table" then
    return nil, "display.result must be a table"
  end
  for k, _ in pairs(c.result) do
    if k ~= "kind" and k ~= "text" then
      return nil, "display.result has unknown field `" .. tostring(k) .. "`"
    end
  end
  if c.result.kind ~= "content" and c.result.kind ~= "receipt" then
    return nil, "display.result.kind must be `content` or `receipt`"
  end
  if c.result.kind == "receipt" and not nonempty(c.result.text) then
    return nil, "display.result.text must be non-empty for a receipt"
  end
  if c.result.kind == "content" and c.result.text ~= nil then
    return nil, "display.result.text is only valid for a receipt"
  end
  return true
end

local function sorted_keys(value)
  local keys = {}
  for key, _ in pairs(value or {}) do
    if type(key) == "string" then keys[#keys + 1] = key end
  end
  table.sort(keys)
  return keys
end

local function is_dense_array(value)
  if type(value) ~= "table" then return false end
  local count = 0
  for key, _ in pairs(value) do
    if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then return false end
    count = count + 1
  end
  if count == 0 then
    return type(nefor) == "table" and type(nefor.json) == "table"
      and type(nefor.json.is_array) == "function" and nefor.json.is_array(value)
  end
  for i = 1, count do
    if value[i] == nil then return false end
  end
  return true
end

local function display_value(value)
  local kind = type(value)
  if kind == "string" then return value end
  if kind == "number" or kind == "boolean" then return tostring(value) end
  if kind ~= "table" then return nil end
  if is_dense_array(value) then
    local items = {}
    for i = 1, #value do
      local rendered = display_value(value[i])
      items[#items + 1] = rendered or "<complex value>"
    end
    return table.concat(items, " · ")
  end
  return nil
end

local function is_absolute_path(value)
  return type(value) == "string"
    and (value:sub(1, 1) == "/" or value:match("^%a:[/\\]") ~= nil)
end

local function pick(selector_value, args)
  if type(selector_value) == "string" then return selector_value end
  if type(selector_value) ~= "table" or type(args) ~= "table" then return nil end
  local value = args[selector_value.arg]
  if value == nil then value = selector_value.default end
  local rendered = display_value(value)
  if rendered == nil then return nil end
  if selector_value.cwd_arg ~= nil and not is_absolute_path(value) then
    local cwd = display_value(args[selector_value.cwd_arg])
    if nonempty(cwd) then
      return rendered .. " (cwd: " .. cwd .. ")"
    end
  end
  return rendered
end

local function selector_is_path(selector_value)
  return type(selector_value) == "table"
    and (selector_value.arg == "path" or selector_value.arg == "file")
end

local function byte_count(value)
  if value == nil then return 0 end
  if type(value) == "string" then return #value end
  if type(nefor) == "table" and type(nefor.json) == "table"
      and type(nefor.json.encode) == "function" then
    local ok, encoded = pcall(nefor.json.encode, value)
    if ok and type(encoded) == "string" then return #encoded end
  end
  return #tostring(value)
end

local function size_text(bytes)
  if bytes < 1024 then return tostring(bytes) .. " B" end
  return string.format("%.1f KiB", bytes / 1024)
end

local function value_shape(value)
  local kind = type(value)
  if kind == "nil" then return "null · 0 B" end
  local bytes = size_text(byte_count(value))
  if kind == "string" then return "string · " .. bytes end
  if kind == "number" then return "number · " .. bytes end
  if kind == "boolean" then return "boolean · " .. bytes end
  if kind ~= "table" then return kind .. " · " .. bytes end
  if is_dense_array(value) then
    return "array · " .. tostring(#value) .. (#value == 1 and " item · " or " items · ") .. bytes
  end
  return "object · " .. tostring(#sorted_keys(value)) .. " fields · " .. bytes
end

local function fallback(name, args)
  local projection = { label = name or "?", arguments = {} }
  if type(args) ~= "table" or is_dense_array(args) then
    projection.arguments[1] = { label = "input", value = value_shape(args) }
    return projection
  end
  for _, key in ipairs(sorted_keys(args)) do
    projection.arguments[#projection.arguments + 1] = {
      label = key,
      value = value_shape(args[key]),
    }
  end
  return projection
end

function M.project(c, args, output, is_error, name)
  local raw_args = args
  local ok, err = M.validate(c)
  args = type(args) == "table" and args or {}
  local projection
  if ok then
    projection = {
      label = pick(c.label, args),
      primary = pick(c.primary, args),
      primary_is_path = selector_is_path(c.primary),
      arguments = {},
    }
    if not nonempty(projection.label) then
      return nil, "display label could not be derived from invocation"
    end
    for _, field in ipairs(c.arguments or {}) do
      local rendered = pick(field, args)
      if rendered ~= nil then
        projection.arguments[#projection.arguments + 1] = {
          label = field.label,
          value = rendered,
        }
      end
    end
  else
    projection = fallback(name, raw_args)
    projection.fallback_error = err
  end

  if is_error then
    local text = type(output) == "string" and output or display_value(output)
    if text == nil and type(output) == "table" and type(nefor) == "table"
        and type(nefor.json) == "table" and type(nefor.json.encode) == "function" then
      local encoded_ok, encoded = pcall(nefor.json.encode, output)
      if encoded_ok then text = encoded end
    end
    projection.result = { kind = "content", text = text or tostring(output or ""), error = true }
  elseif output == nil then
    projection.result = { kind = "running" }
  else
    local receipt = ok and c.result.text or nil
    if not nonempty(receipt) then receipt = "completed" end
    projection.result = {
      kind = "receipt",
      text = receipt .. " · " .. size_text(byte_count(output)) .. " hidden",
    }
  end
  return projection
end

return M
