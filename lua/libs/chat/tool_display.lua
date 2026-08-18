-- Strict declarative presentation contract for chat tools. The catalog owns
-- every compact/expanded choice; the renderer only interprets this data.
local M = {}

function M.fields(values)
  values = values or {}
  if type(values) ~= "table" then error("tool_display.fields: values must be a table", 2) end
  if type(nefor) == "table" and type(nefor.json) == "table"
      and type(nefor.json.mark_array) == "function" then
    nefor.json.mark_array(values)
  end
  return values
end

local FIELD_KINDS = {
  text = true, scalar = true, status = true, path = true, bytes = true,
  list = true, structured = true,
}

local function nonempty(value)
  return type(value) == "string" and value ~= ""
end

local function dense_list(value)
  if type(value) ~= "table" then return false end
  local count = 0
  for key, _ in pairs(value) do
    if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then return false end
    count = count + 1
  end
  if count == 0 then
    return next(value) == nil
  end
  for index = 1, count do if value[index] == nil then return false end end
  return true
end

local function validate_path(path, where)
  if type(path) == "string" then
    if nonempty(path) then return true end
    return nil, where .. " must be non-empty"
  end
  if not dense_list(path) or #path == 0 then
    return nil, where .. " must be a non-empty string or string list"
  end
  for index, part in ipairs(path) do
    if not nonempty(part) then return nil, where .. "[" .. index .. "] must be a non-empty string" end
  end
  return true
end

local function validate_selector(selector, where)
  if type(selector) ~= "table" then return nil, where .. " must be a table" end
  for key, _ in pairs(selector) do
    if key ~= "source" and key ~= "path" and key ~= "default" and key ~= "fallback" then
      return nil, where .. " has unknown field `" .. tostring(key) .. "`"
    end
  end
  if selector.source ~= "args" and selector.source ~= "result" then
    return nil, where .. ".source must be `args` or `result`"
  end
  local ok, err = validate_path(selector.path, where .. ".path")
  if not ok then return nil, err end
  if selector.fallback ~= nil then
    local fallback = selector.fallback
    if type(fallback) ~= "table" then return nil, where .. ".fallback must be a selector" end
    for key, _ in pairs(fallback) do
      if key ~= "source" and key ~= "path" then
        return nil, where .. ".fallback has unknown field `" .. tostring(key) .. "`"
      end
    end
    if fallback.source ~= "args" and fallback.source ~= "result" then
      return nil, where .. ".fallback.source must be `args` or `result`"
    end
    ok, err = validate_path(fallback.path, where .. ".fallback.path")
    if not ok then return nil, err end
  end
  return true
end

local function validate_field(field, where)
  if type(field) ~= "table" then return nil, where .. " must be a table" end
  for key, _ in pairs(field) do
    if key ~= "label" and key ~= "select" and key ~= "kind"
        and key ~= "omit" and key ~= "sensitive" and key ~= "max_lines"
        and key ~= "max_bytes" then
      return nil, where .. " has unknown field `" .. tostring(key) .. "`"
    end
  end
  if not nonempty(field.label) then return nil, where .. ".label must be non-empty" end
  local ok, err = validate_selector(field.select, where .. ".select")
  if not ok then return nil, err end
  if not FIELD_KINDS[field.kind] then return nil, where .. ".kind is unknown" end
  if field.omit ~= nil and field.omit ~= "missing" and field.omit ~= "empty" then
    return nil, where .. ".omit must be `missing` or `empty`"
  end
  if field.sensitive ~= nil and field.sensitive ~= "redact" and field.sensitive ~= "omit" then
    return nil, where .. ".sensitive must be `redact` or `omit`"
  end
  if field.kind == "text" then
    if type(field.max_lines) ~= "number" or field.max_lines < 1 or field.max_lines % 1 ~= 0 then
      return nil, where .. ".max_lines must be a positive integer for text"
    end
    if type(field.max_bytes) ~= "number" or field.max_bytes < 1 or field.max_bytes % 1 ~= 0 then
      return nil, where .. ".max_bytes must be a positive integer for text"
    end
  elseif field.max_lines ~= nil or field.max_bytes ~= nil then
    return nil, where .. " bounds are only valid for text"
  end
  return true
end

local function validate_fields(fields, where)
  if not dense_list(fields) then return nil, where .. " must be a JSON array" end
  for index, field in ipairs(fields) do
    local ok, err = validate_field(field, where .. "[" .. index .. "]")
    if not ok then return nil, err end
  end
  return true
end

local function validate_view(view, where, expanded)
  if type(view) ~= "table" then return nil, where .. " must be a table" end
  for key, _ in pairs(view) do
    if key ~= "label" and key ~= "primary" and (not expanded or key ~= "fields") then
      return nil, where .. " has unknown field `" .. tostring(key) .. "`"
    end
  end
  if not nonempty(view.label) then return nil, where .. ".label must be non-empty" end
  if view.primary ~= nil then
    local ok, err = validate_field(view.primary, where .. ".primary")
    if not ok then return nil, err end
  end
  if expanded then return validate_fields(view.fields, where .. ".fields") end
  return true
end

local function validate_policy(policy, where)
  if type(policy) ~= "table" then return nil, where .. " must be a table" end
  for key, _ in pairs(policy) do
    if key ~= "compact" and key ~= "expanded" and key ~= "result" and key ~= "lifecycle" and key ~= "variant" then
      return nil, where .. " has unknown field `" .. tostring(key) .. "`"
    end
  end
  local ok, err = validate_view(policy.compact, where .. ".compact", false)
  if not ok then return nil, err end
  ok, err = validate_view(policy.expanded, where .. ".expanded", true)
  if not ok then return nil, err end
  if policy.lifecycle ~= nil and policy.lifecycle ~= "delayed" then
    return nil, where .. ".lifecycle must be `delayed` when present"
  end
  if type(policy.result) ~= "table" then return nil, where .. ".result must be a table" end
  for key, _ in pairs(policy.result) do
    if key ~= "kind" and key ~= "text" and key ~= "fields" then
      return nil, where .. ".result has unknown field `" .. tostring(key) .. "`"
    end
  end
  if policy.result.kind ~= "content" and policy.result.kind ~= "receipt" then
    return nil, where .. ".result.kind must be `content` or `receipt`"
  end
  if policy.result.kind == "receipt" and not nonempty(policy.result.text) then
    return nil, where .. ".result.text must be non-empty for a receipt"
  end
  if policy.result.kind == "content" and policy.result.text ~= nil then
    return nil, where .. ".result.text is only valid for a receipt"
  end
  return validate_fields(policy.result.fields, where .. ".result.fields")
end

function M.validate(contract)
  if type(contract) ~= "table" then return nil, "display must be a table" end
  for key, _ in pairs(contract) do
    if key ~= "compact" and key ~= "expanded" and key ~= "result"
        and key ~= "lifecycle" and key ~= "variant" then
      return nil, "display has unknown field `" .. tostring(key) .. "`"
    end
  end
  local ok, err = validate_policy(contract, "display")
  if not ok then return nil, err end
  if contract.variant ~= nil then
    local variant = contract.variant
    if type(variant) ~= "table" then return nil, "display.variant must be a table" end
    for key, _ in pairs(variant) do
      if key ~= "select" and key ~= "cases" then return nil, "display.variant has unknown field `" .. tostring(key) .. "`" end
    end
    ok, err = validate_selector(variant.select, "display.variant.select")
    if not ok then return nil, err end
    if type(variant.cases) ~= "table" or dense_list(variant.cases) then return nil, "display.variant.cases must be an object" end
    for name, policy in pairs(variant.cases) do
      if not nonempty(name) then return nil, "display.variant case name must be non-empty" end
      ok, err = validate_policy(policy, "display.variant.cases." .. name)
      if not ok then return nil, err end
    end
  end
  return true
end

local function get_path(root, path)
  if path == "$" then return root end
  local value = root
  local parts = type(path) == "table" and path or { path }
  for _, part in ipairs(parts) do
    if type(value) ~= "table" then return nil end
    value = value[part]
  end
  return value
end

local function selected(selector, args, result)
  local root = selector.source == "args" and args or result
  local value = get_path(root, selector.path)
  if value == nil and selector.fallback ~= nil then
    local fallback_root = selector.fallback.source == "args" and args or result
    value = get_path(fallback_root, selector.fallback.path)
  end
  if value == nil then value = selector.default end
  return value
end

local function json_text(value)
  if type(value) ~= "table" then return tostring(value) end
  local ok, encoded = pcall(nefor.json.encode, value)
  return ok and encoded or tostring(value)
end

local function utf8_prefix(value, cap)
  if #value <= cap then return value, false end
  local finish = cap
  while finish > 0 and value:byte(finish + 1) and value:byte(finish + 1) >= 0x80
      and value:byte(finish + 1) <= 0xBF do finish = finish - 1 end
  return value:sub(1, finish), true
end

local function bounded_text(value, lines, bytes)
  local text = tostring(value)
  local kept, count = {}, 0
  for line in (text .. "\n"):gmatch("(.-)\n") do
    count = count + 1
    if count <= lines then kept[#kept + 1] = line end
  end
  local rendered = table.concat(kept, "\n")
  local clipped_lines = count > lines
  rendered, clipped_bytes = utf8_prefix(rendered, bytes)
  if clipped_lines or clipped_bytes then rendered = rendered .. "\n…" end
  return rendered
end

local function size_text(bytes)
  if bytes < 1024 then return tostring(bytes) .. " B" end
  return string.format("%.1f KiB", bytes / 1024)
end

local function render_value(field, value)
  if field.sensitive == "omit" then return nil end
  if field.sensitive == "redact" then return "[redacted]" end
  if value == nil and field.omit == "missing" then return nil end
  if (value == "" or (type(value) == "table" and next(value) == nil)) and field.omit == "empty" then return nil end
  if field.kind == "text" then return bounded_text(value or "", field.max_lines, field.max_bytes) end
  if field.kind == "bytes" then
    if type(value) == "number" then return size_text(value) end
    return size_text(#json_text(value or ""))
  end
  if field.kind == "list" and type(value) == "table" then
    local values = {}
    for _, item in ipairs(value) do values[#values + 1] = json_text(item) end
    return table.concat(values, " · ")
  end
  if field.kind == "structured" then return json_text(value or {}) end
  return value == nil and "" or tostring(value)
end

local function project_field(field, args, result)
  local value = selected(field.select, args, result)
  local rendered = render_value(field, value)
  if rendered == nil then return nil end
  return { label = field.label, value = rendered, kind = field.kind }
end

local function active_policy(contract, args, result)
  local variant = contract.variant
  if variant == nil then return contract end
  local name = selected(variant.select, args, result)
  return variant.cases[tostring(name)] or contract
end

function M.project(contract, args, result, is_error)
  local ok, err = M.validate(contract)
  if not ok then return nil, err end
  args = type(args) == "table" and args or {}
  local policy = active_policy(contract, args, result)
  local compact, expanded = policy.compact, policy.expanded
  local projection = {
    label = compact.label,
    expanded_label = expanded.label,
    arguments = {}, result_fields = {},
  }
  if compact.primary then
    local primary = project_field(compact.primary, args, result)
    if primary then
      projection.primary = primary.value
      projection.primary_is_path = primary.kind == "path"
    end
  end
  for _, field in ipairs(expanded.fields) do
    local projected = project_field(field, args, result)
    if projected then projection.arguments[#projection.arguments + 1] = projected end
  end
  for _, field in ipairs(policy.result.fields) do
    local projected = project_field(field, args, result)
    if projected then projection.result_fields[#projection.result_fields + 1] = projected end
  end
  if is_error then
    projection.result = { kind = "content", text = json_text(result or ""), error = true }
  elseif result == nil then
    projection.result = { kind = "running" }
  elseif policy.result.kind == "receipt" then
    projection.result = { kind = "receipt", text = policy.result.text }
  else
    local text = ""
    if #projection.result_fields == 0 then text = json_text(result) end
    projection.result = { kind = "content", text = text }
  end
  return projection
end

return M
