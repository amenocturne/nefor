-- Tool parameter schemas: definition-time checking and call-time validation.
--
-- Supports exactly the JSON Schema keywords Nefor tool schemas use:
--
--   type (object, string, integer, number, boolean, array), properties,
--   required, additionalProperties (only `false`), enum, oneOf, items,
--   minItems, minLength, minimum, maximum, pattern, and the annotations
--   description and default.
--
-- `check` rejects every other keyword, so validation never silently ignores
-- a constraint a schema advertises. Every object schema is closed: arguments
-- not declared in `properties` are rejected whether or not the schema spells
-- `additionalProperties: false`. Patterns use the subset documented in
-- `libs.tool.pattern`.
--
-- JSON values are Lua values as `nefor.json.decode` produces them: arrays
-- carry the JSON array marker, JSON null is a userdata sentinel. Lua-built
-- values have no marker, so a dense 1..n table is an array and an empty
-- unmarked table satisfies either `object` or `array`.

local json_data = require("core.json_data")
local pattern = require("libs.tool.pattern")

local M = {}

local KEYWORDS = {
  type = true, properties = true, required = true, additionalProperties = true,
  enum = true, oneOf = true, items = true, minItems = true, minLength = true,
  minimum = true, maximum = true, pattern = true, description = true, default = true,
}

local TYPES = {
  object = true, string = true, integer = true, number = true, boolean = true, array = true,
}

local KEYWORD_TYPES = {
  properties = { object = true },
  required = { object = true },
  additionalProperties = { object = true },
  items = { array = true },
  minItems = { array = true },
  minLength = { string = true },
  pattern = { string = true },
  minimum = { integer = true, number = true },
  maximum = { integer = true, number = true },
}

local function sorted_keys(map)
  local keys = {}
  for key in pairs(map) do keys[#keys + 1] = key end
  table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
  return keys
end

local function dense_count(value)
  local count = 0
  for key in pairs(value) do
    if math.type(key) ~= "integer" or key < 1 then return nil end
    count = count + 1
  end
  for index = 1, count do
    if value[index] == nil then return nil end
  end
  return count
end

local function is_list(value)
  if type(value) ~= "table" then return false end
  if json_data.is_array(value) then return true end
  local count = dense_count(value)
  return count ~= nil and count > 0
end

local function is_map(value)
  if type(value) ~= "table" or json_data.is_array(value) then return false end
  for key in pairs(value) do
    if type(key) ~= "string" then return false end
  end
  return true
end

local function nonnegative_integer(value)
  return math.type(value) == "integer" and value >= 0
end

local function finite_number(value)
  return type(value) == "number" and value == value and value ~= math.huge and value ~= -math.huge
end

local function compiled_patterns()
  return setmetatable({}, { __mode = "k" })
end

local PATTERNS = compiled_patterns()

local function check_node(schema, where)
  if not is_map(schema) then return nil, where .. " must be a schema object" end
  for _, key in ipairs(sorted_keys(schema)) do
    if not KEYWORDS[key] then
      return nil, where .. " uses unsupported keyword `" .. tostring(key) .. "`"
    end
  end
  local schema_type = schema.type
  if schema_type ~= nil then
    if type(schema_type) ~= "string" or not TYPES[schema_type] then
      return nil, where .. ".type must be one of array, boolean, integer, number, object, string"
    end
  elseif schema.oneOf == nil and schema.enum == nil then
    return nil, where .. " must declare type, enum, or oneOf"
  end
  for keyword, applicable in pairs(KEYWORD_TYPES) do
    if schema[keyword] ~= nil and not applicable[schema_type] then
      local names = sorted_keys(applicable)
      return nil, where .. "." .. keyword .. " requires type " .. table.concat(names, " or ")
    end
  end
  if schema.description ~= nil and type(schema.description) ~= "string" then
    return nil, where .. ".description must be a string"
  end
  if schema.properties ~= nil then
    if not is_map(schema.properties) then
      return nil, where .. ".properties must be an object keyed by argument name"
    end
    for _, name in ipairs(sorted_keys(schema.properties)) do
      if name == "" then return nil, where .. ".properties has an empty argument name" end
      local ok, err = check_node(schema.properties[name], where .. ".properties." .. name)
      if not ok then return nil, err end
    end
  end
  if schema.required ~= nil then
    if type(schema.required) ~= "table" or (not is_list(schema.required)
        and next(schema.required) ~= nil) then
      return nil, where .. ".required must be an array of argument names"
    end
    local seen = {}
    for _, name in ipairs(schema.required) do
      if type(name) ~= "string" then return nil, where .. ".required must contain only strings" end
      if seen[name] then return nil, where .. ".required lists `" .. name .. "` twice" end
      seen[name] = true
      if schema.properties == nil or schema.properties[name] == nil then
        return nil, where .. ".required names undeclared argument `" .. name .. "`"
      end
    end
  end
  if schema.additionalProperties ~= nil and schema.additionalProperties ~= false then
    return nil, where .. ".additionalProperties may only be false; every tool schema is closed"
  end
  if schema.enum ~= nil then
    if not is_list(schema.enum) then return nil, where .. ".enum must be a non-empty array" end
    for _, value in ipairs(schema.enum) do
      local kind = type(value)
      if kind ~= "string" and kind ~= "number" and kind ~= "boolean" then
        return nil, where .. ".enum may contain only strings, numbers, and booleans"
      end
    end
  end
  if schema.oneOf ~= nil then
    if not is_list(schema.oneOf) then return nil, where .. ".oneOf must be a non-empty array" end
    for index, alternative in ipairs(schema.oneOf) do
      local ok, err = check_node(alternative, where .. ".oneOf[" .. (index - 1) .. "]")
      if not ok then return nil, err end
    end
  end
  if schema.items ~= nil then
    local ok, err = check_node(schema.items, where .. ".items")
    if not ok then return nil, err end
  end
  for _, keyword in ipairs({ "minItems", "minLength" }) do
    if schema[keyword] ~= nil and not nonnegative_integer(schema[keyword]) then
      return nil, where .. "." .. keyword .. " must be a non-negative integer"
    end
  end
  for _, keyword in ipairs({ "minimum", "maximum" }) do
    if schema[keyword] ~= nil and not finite_number(schema[keyword]) then
      return nil, where .. "." .. keyword .. " must be a finite number"
    end
  end
  if schema.minimum ~= nil and schema.maximum ~= nil and schema.minimum > schema.maximum then
    return nil, where .. ".minimum exceeds maximum"
  end
  if schema.pattern ~= nil then
    local matcher, err = pattern.compile(schema.pattern)
    if not matcher then return nil, where .. ".pattern is not supported: " .. err end
    PATTERNS[schema] = matcher
  end
  return true
end

-- check(parameters) -> true | nil, error
-- A tool's root schema must be an object schema.
function M.check(parameters)
  if not is_map(parameters) then return nil, "parameters must be a schema object" end
  if parameters.type ~= "object" then return nil, "parameters.type must be \"object\"" end
  return check_node(parameters, "parameters")
end

-- Value classification ------------------------------------------------------

local function value_kind(value)
  local kind = type(value)
  if kind == "string" or kind == "boolean" then return kind end
  if kind == "number" then
    if math.type(value) == "integer" then return "integer" end
    if finite_number(value) and value % 1 == 0 then return "integer" end
    return "number"
  end
  if kind == "userdata" then return "null" end
  if kind == "table" then
    if json_data.is_array(value) then return "array" end
    if next(value) == nil then return "empty" end
    if dense_count(value) then return "array" end
    if is_map(value) then return "object" end
    return "table"
  end
  return kind
end

local function matches_type(expected, value)
  local kind = value_kind(value)
  if expected == kind then return true end
  if expected == "number" and kind == "integer" then return true end
  if kind == "empty" and (expected == "object" or expected == "array") then return true end
  return false
end

local ARTICLES = { array = "an", integer = "an", object = "an" }

local function with_article(type_name)
  return (ARTICLES[type_name] or "a") .. " " .. type_name
end

local function render(value)
  if type(value) == "string" then return string.format("%q", value) end
  return tostring(value)
end

local function shown_kind(value)
  local kind = value_kind(value)
  if kind == "empty" then return "object" end
  return kind
end

local function describe(schema)
  if schema.enum ~= nil then
    local values = {}
    for _, value in ipairs(schema.enum) do values[#values + 1] = render(value) end
    return "one of " .. table.concat(values, ", ")
  end
  if schema.oneOf ~= nil then
    local parts = {}
    for _, alternative in ipairs(schema.oneOf) do parts[#parts + 1] = describe(alternative) end
    return table.concat(parts, " or ")
  end
  if schema.type == "array" and schema.items ~= nil then
    return "array of " .. describe(schema.items)
  end
  return schema.type or "value"
end

local function child(path, name)
  if path == nil then return name end
  return path .. "." .. name
end

local function subject(path)
  if path == nil then return "arguments" end
  return "argument `" .. path .. "`"
end

-- Ordering for the accepted-argument list: required names in declared order,
-- then the optional names alphabetically.
local function accepted_names(schema)
  local names, listed = {}, {}
  for _, name in ipairs(schema.required or {}) do
    names[#names + 1] = name
    listed[name] = true
  end
  for _, name in ipairs(sorted_keys(schema.properties or {})) do
    if not listed[name] then names[#names + 1] = name end
  end
  return names
end

local validate_node

local function validate_object(schema, value, path)
  local properties = schema.properties or {}
  for _, key in ipairs(sorted_keys(value)) do
    if properties[key] == nil then
      local accepted = accepted_names(schema)
      local where = path == nil and "this tool" or ("`" .. path .. "`")
      if #accepted == 0 then
        return "unknown argument `" .. child(path, tostring(key)) .. "`; "
          .. where .. " accepts no arguments"
      end
      local scope = path == nil and "accepted arguments: " or ("accepted in `" .. path .. "`: ")
      return "unknown argument `" .. child(path, tostring(key)) .. "`; "
        .. scope .. table.concat(accepted, ", ")
    end
  end
  for _, name in ipairs(schema.required or {}) do
    if value[name] == nil then
      return "missing required argument `" .. child(path, name) .. "`"
    end
  end
  for _, name in ipairs(accepted_names(schema)) do
    if value[name] ~= nil then
      local err = validate_node(properties[name], value[name], child(path, name))
      if err then return err end
    end
  end
  return nil
end

local function validate_array(schema, value, path)
  local count = #value
  if schema.minItems ~= nil and count < schema.minItems then
    return subject(path) .. " must contain at least " .. schema.minItems
      .. (schema.minItems == 1 and " item" or " items") .. ", got " .. count
  end
  if schema.items ~= nil then
    for index = 1, count do
      local err = validate_node(schema.items, value[index], path .. "[" .. (index - 1) .. "]")
      if err then return err end
    end
  end
  return nil
end

local function validate_string(schema, value, path)
  if schema.minLength ~= nil then
    local length = utf8.len(value) or #value
    if length < schema.minLength then
      return subject(path) .. " must be at least " .. schema.minLength
        .. (schema.minLength == 1 and " character" or " characters") .. " long"
    end
  end
  if schema.pattern ~= nil then
    local matcher = PATTERNS[schema] or assert(pattern.compile(schema.pattern))
    if not matcher(value) then
      return subject(path) .. " must match pattern `" .. schema.pattern .. "`"
    end
  end
  return nil
end

local function validate_number(schema, value, path)
  if schema.minimum ~= nil and value < schema.minimum then
    return subject(path) .. " must be at least " .. render(schema.minimum) .. ", got " .. render(value)
  end
  if schema.maximum ~= nil and value > schema.maximum then
    return subject(path) .. " must be at most " .. render(schema.maximum) .. ", got " .. render(value)
  end
  return nil
end

validate_node = function(schema, value, path)
  if schema.oneOf ~= nil then
    local matched = 0
    for _, alternative in ipairs(schema.oneOf) do
      if validate_node(alternative, value, path) == nil then matched = matched + 1 end
    end
    if matched ~= 1 then
      return subject(path) .. " must be exactly one of: " .. describe(schema)
        .. ", got " .. shown_kind(value)
    end
  end
  local schema_type = schema.type
  if schema_type ~= nil and not matches_type(schema_type, value) then
    if path == nil then
      return "arguments must be an object, got " .. shown_kind(value)
    end
    return subject(path) .. " must be " .. with_article(schema_type) .. ", got " .. shown_kind(value)
  end
  if schema.enum ~= nil then
    local found = false
    for _, allowed in ipairs(schema.enum) do
      if allowed == value then found = true; break end
    end
    if not found then
      return subject(path) .. " must be " .. describe({ enum = schema.enum }) .. ", got " .. render(value)
    end
  end
  if schema_type == "object" then return validate_object(schema, value, path) end
  if schema_type == "array" then return validate_array(schema, value, path) end
  if schema_type == "string" then return validate_string(schema, value, path) end
  if schema_type == "integer" or schema_type == "number" then
    return validate_number(schema, value, path)
  end
  return nil
end

-- validate(parameters, args) -> nil | message
-- `parameters` must already have passed `check`. The message names the
-- offending argument path but not the tool; `libs.tool` prefixes the tool.
function M.validate(parameters, args)
  return validate_node(parameters, args, nil)
end

return M
