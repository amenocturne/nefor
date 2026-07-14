local M = {}
local function nonempty(v) return type(v) == "string" and v ~= "" end
local function selector(v, where)
  if type(v) ~= "table" or not nonempty(v.arg) then return nil, where .. " must be { arg = <non-empty string> }" end
  for k, _ in pairs(v) do if k ~= "arg" then return nil, where .. " has unknown field `" .. tostring(k) .. "`" end end
  return true
end
function M.validate(c)
  if type(c) ~= "table" then return nil, "display must be a table" end
  for k, _ in pairs(c) do if k ~= "label" and k ~= "primary" and k ~= "arguments" and k ~= "result" then return nil, "display has unknown field `" .. tostring(k) .. "`" end end
  if not nonempty(c.label) then local ok, err = selector(c.label, "display.label"); if not ok then return nil, err end end
  if c.primary ~= nil then local ok, err = selector(c.primary, "display.primary"); if not ok then return nil, err end end
  if c.arguments ~= nil then
    if type(c.arguments) ~= "table" then return nil, "display.arguments must be a JSON array" end
    -- JSON-decoded arrays carry mlua's array metatable. This is the only
    -- exact distinction between wire `[]` and `{}`; non-empty config-owned
    -- Lua lists remain accepted after strict dense-list validation below.
    local is_json_array = type(nefor) == "table"
      and type(nefor.json) == "table"
      and type(nefor.json.is_array) == "function"
      and nefor.json.is_array(c.arguments)
    local count = 0
    for key, _ in pairs(c.arguments) do
      if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then return nil, "display.arguments must contain only list entries" end
      count = count + 1
    end
    if count == 0 and not is_json_array then return nil, "display.arguments must be a JSON array" end
    for i = 1, count do if c.arguments[i] == nil then return nil, "display.arguments must not contain holes" end end
    for i, f in ipairs(c.arguments) do
      if type(f) ~= "table" or not nonempty(f.label) or not nonempty(f.arg) then return nil, "display.arguments[" .. i .. "] needs label and arg strings" end
      for k, _ in pairs(f) do if k ~= "label" and k ~= "arg" then return nil, "display.arguments[" .. i .. "] has unknown field `" .. tostring(k) .. "`" end end
    end
  end
  if type(c.result) ~= "table" then return nil, "display.result must be a table" end
  for k, _ in pairs(c.result) do if k ~= "kind" and k ~= "text" then return nil, "display.result has unknown field `" .. tostring(k) .. "`" end end
  if c.result.kind ~= "content" and c.result.kind ~= "receipt" then return nil, "display.result.kind must be `content` or `receipt`" end
  if c.result.kind == "receipt" and not nonempty(c.result.text) then return nil, "display.result.text must be non-empty for a receipt" end
  if c.result.kind == "content" and c.result.text ~= nil then return nil, "display.result.text is only valid for a receipt" end
  return true
end
local function pick(s, args)
  if type(s) == "string" then return s end
  if type(s) ~= "table" or type(args) ~= "table" then return nil end
  local v = args[s.arg]
  if type(v) == "string" then return v end
  if type(v) == "number" or type(v) == "boolean" then return tostring(v) end
end
function M.project(c, args, output, is_error)
  local ok, err = M.validate(c); if not ok then return nil, err end
  args = type(args) == "table" and args or {}
  local p = { label = pick(c.label, args), primary = pick(c.primary, args), arguments = {} }
  if not nonempty(p.label) then return nil, "display label could not be derived from invocation" end
  for _, f in ipairs(c.arguments or {}) do local v = args[f.arg]; if v ~= nil and type(v) ~= "table" then p.arguments[#p.arguments + 1] = { label = f.label, value = tostring(v) } end end
  if is_error then p.result = { kind = "content", text = tostring(output or ""), error = true }
  elseif output == nil then p.result = { kind = "running" }
  elseif c.result.kind == "content" then p.result = { kind = "content", text = tostring(output or "") }
  else p.result = { kind = "receipt", text = c.result.text } end
  return p
end
return M
