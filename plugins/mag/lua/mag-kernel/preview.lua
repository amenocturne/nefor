-- Serializable, domain-neutral actor preview descriptions and telemetry validation.
local M = {}

local function node(kind, fields)
  local out = { kind = kind }
  for key, value in pairs(fields or {}) do out[key] = value end
  return out
end

function M.column(spec) return node("column", spec) end
function M.row(spec) return node("row", spec) end
function M.padding(spec) return node("padding", spec) end
function M.block(spec) return node("block", spec) end
function M.spacer(spec) return node("spacer", spec) end
function M.text(spec) return node("text", spec) end
function M.spans(spec) return node("spans", spec) end
function M.markdown(spec) return node("markdown", spec) end
function M.value(spec) return node("value", spec) end
function M.stream(spec) return node("stream", spec) end
function M.list(spec) return node("list", spec) end
function M.cases(values) return node("cases", { values = values }) end

local function binding(kind, name, schema)
  return { binding = kind, name = name, schema = schema }
end
function M.param(name) return binding("param", name) end
function M.input(name) return binding("input", name) end
function M.output(name) return binding("output", name) end
function M.state(name, schema) return binding("state", name, schema) end
function M.stream_ref(name, schema) return binding("stream", name, schema) end
function M.lifecycle(name) return binding("lifecycle", name) end
function M.item(name) return binding("item", name) end

local NODE_FIELDS = {
  column={children=true,gap=true,key=true}, row={children=true,gap=true,key=true},
  padding={child=true,top=true,right=true,bottom=true,left=true,key=true},
  block={child=true,style=true,key=true}, spacer={flex=true,key=true},
  text={value=true,style=true,wrap=true,key=true}, spans={value=true,wrap=true,key=true},
  markdown={value=true,theme=true,wrap=true,key=true},
  value={value=true,format=true,style=true,wrap=true,key=true},
  stream={source=true,item=true,empty=true,follow=true,key=true},
  list={source=true,item=true,empty=true,key=true}, cases={values=true,key=true},
}
local BINDING_FIELDS = { binding=true,name=true,schema=true }
local LIFECYCLE = {
  run_id=true,run_name=true,actor_id=true,factory=true,status=true,
  started_at_ms=true,finished_at_ms=true,last_activity_ms=true,
}

local function fail(path, message) return nil, path .. ": " .. message end

local function is_dense_array(value)
  local count, maximum = 0, 0
  for key in pairs(value) do
    if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then return false end
    count, maximum = count + 1, math.max(maximum, key)
  end
  return count == maximum
end

local function validate_serializable(value, path, visiting)
  local kind = type(value)
  if kind == "function" or kind == "thread" or kind == "userdata" then
    return fail(path, kind .. " values are not serializable")
  end
  if kind == "number" and (value ~= value or value == math.huge or value == -math.huge) then
    return fail(path, "non-finite numbers are not serializable")
  end
  if kind ~= "table" then return true end
  if getmetatable(value) ~= nil then return fail(path, "metatables are not serializable declaration data") end
  visiting = visiting or {}
  if visiting[value] then return fail(path, "cycle in declaration data") end
  visiting[value] = true
  for key, child in pairs(value) do
    if type(key) ~= "string" and type(key) ~= "number" then
      visiting[value] = nil; return fail(path, "table keys must be strings or positive array indices")
    end
    if type(key) == "number" and (key < 1 or key % 1 ~= 0) then
      visiting[value] = nil; return fail(path, "numeric table keys must be positive array indices")
    end
    local ok, err = validate_serializable(child, path .. "." .. tostring(key), visiting)
    if not ok then visiting[value] = nil; return nil, err end
  end
  visiting[value] = nil
  return true
end

local function schema_field(schema, field)
  if type(schema) ~= "table" or schema.kind ~= "record" or type(schema.fields) ~= "table" then return nil end
  return schema.fields[field]
end

local function validate_schema(schema, path, visiting)
  if schema == nil then return fail(path, "schema is required") end
  if type(schema) == "string" then return true end
  if type(schema) ~= "table" then return fail(path, "schema must be a string or table") end
  visiting = visiting or {}
  if visiting[schema] then return fail(path, "cycle in schema") end
  visiting[schema] = true
  local kind = schema.kind
  if kind == "record" then
    if type(schema.fields) ~= "table" then visiting[schema]=nil; return fail(path..".fields", "must be a map") end
    for name, child in pairs(schema.fields) do
      if type(name) ~= "string" or name == "" then visiting[schema]=nil; return fail(path..".fields", "field names must be non-empty strings") end
      local ok, err = validate_schema(child, path..".fields."..name, visiting)
      if not ok then visiting[schema]=nil; return nil,err end
    end
  elseif kind == "variant" then
    if type(schema.tag) ~= "string" or schema.tag == "" then visiting[schema]=nil; return fail(path..".tag", "must be a non-empty string") end
    if type(schema.cases) ~= "table" or next(schema.cases) == nil then visiting[schema]=nil; return fail(path..".cases", "must be a non-empty map") end
    for tag, child in pairs(schema.cases) do
      if type(tag) ~= "string" or tag == "" then visiting[schema]=nil; return fail(path..".cases", "case tags must be non-empty strings") end
      local ok, err = validate_schema(child, path..".cases."..tag, visiting)
      if not ok then visiting[schema]=nil; return nil,err end
    end
  elseif kind == "list" then
    local ok, err = validate_schema(schema.item, path..".item", visiting)
    if not ok then visiting[schema]=nil; return nil,err end
  else
    visiting[schema]=nil; return fail(path..".kind", "unknown schema kind "..tostring(kind))
  end
  visiting[schema] = nil
  return true
end

local function validate_binding(ref, declaration, bindings, path, item_schema)
  for field in pairs(ref) do if not BINDING_FIELDS[field] then return fail(path.."."..tostring(field), "unknown binding field") end end
  local kind, name = ref.binding, ref.name
  if type(name) ~= "string" or name == "" then return fail(path .. ".name", "must be a non-empty string") end
  if kind == "param" then
    if (declaration.params or {})[name] == nil then return fail(path, "unknown param binding " .. string.format("%q", name)) end
  elseif kind == "input" then
    if name ~= "last" and (declaration.inputs or {})[name] == nil then return fail(path, "unknown input binding " .. string.format("%q", name)) end
  elseif kind == "output" then
    local found = name == "last"
    for _, wire in ipairs(declaration.outputs or {}) do if wire == name then found = true end end
    if not found then return fail(path, "unknown output binding " .. string.format("%q", name)) end
  elseif kind == "lifecycle" then
    if not LIFECYCLE[name] then return fail(path, "unknown lifecycle binding " .. string.format("%q", name)) end
  elseif kind == "item" then
    if not item_schema then return fail(path, "item binding outside a collection template") end
    if not schema_field(item_schema, name) then return fail(path, "unknown item field " .. string.format("%q", name)) end
  elseif kind == "state" or kind == "stream" then
    local ok, err = validate_schema(ref.schema, path..".schema")
    if not ok then return nil, err end
    local prior = bindings[name]
    if prior then return fail(path, "duplicate preview binding " .. string.format("%q", name)) end
    bindings[name] = { kind = kind, schema = ref.schema }
  else
    return fail(path, "unknown binding kind " .. tostring(kind))
  end
  return true
end

local function validate_tree(value, declaration, bindings, keys, path, item_schema)
  if type(value) ~= "table" then return fail(path, "preview node must be a table") end
  if value.binding then return validate_binding(value, declaration, bindings, path, item_schema) end
  local allowed = NODE_FIELDS[value.kind]
  if not allowed then return fail(path .. ".kind", "unknown preview primitive " .. tostring(value.kind)) end
  for field in pairs(value) do
    if field ~= "kind" and not allowed[field] then return fail(path.."."..tostring(field), "unknown field for "..value.kind) end
  end
  if value.key ~= nil then
    if type(value.key) ~= "string" or value.key == "" then return fail(path .. ".key", "must be a non-empty string") end
    if keys[value.key] then return fail(path .. ".key", "stable-key collision " .. string.format("%q", value.key)) end
    keys[value.key] = true
  end
  local function child(field, required, scoped_schema)
    local current = value[field]
    if current == nil then if required then return fail(path.."."..field, "is required") end; return true end
    return validate_tree(current, declaration, bindings, keys, path.."."..field, scoped_schema)
  end
  if value.kind == "column" or value.kind == "row" then
    if type(value.children) ~= "table" or not is_dense_array(value.children) then return fail(path..".children", "must be a dense list") end
    for index = 1, #value.children do
      local ok, err = validate_tree(value.children[index], declaration, bindings, keys, path..".children["..index.."]", item_schema)
      if not ok then return nil,err end
    end
  elseif value.kind == "padding" or value.kind == "block" then
    return child("child", true, item_schema)
  elseif value.kind == "text" or value.kind == "spans" or value.kind == "markdown" or value.kind == "value" then
    if value.value == nil then return fail(path..".value", "is required") end
    if type(value.value) == "table" and value.value.binding then return validate_binding(value.value,declaration,bindings,path..".value",item_schema) end
  elseif value.kind == "stream" or value.kind == "list" then
    local expected = value.kind == "stream" and "stream" or "state"
    if type(value.source) ~= "table" or value.source.binding ~= expected then return fail(path..".source", "must be a matching declared binding reference") end
    local ok,err=validate_binding(value.source,declaration,bindings,path..".source",item_schema); if not ok then return nil,err end
    local source_schema=value.source.schema
    ok,err=child("item",true,source_schema); if not ok then return nil,err end
    if value.empty ~= nil then return child("empty",false,item_schema) end
  elseif value.kind == "cases" then
    if not item_schema or item_schema.kind ~= "variant" then return fail(path, "cases require a variant item schema") end
    if type(value.values) ~= "table" or next(value.values) == nil then return fail(path..".values", "must be a non-empty map") end
    for tag in pairs(value.values) do if not item_schema.cases[tag] then return fail(path..".values."..tostring(tag), "unknown case tag") end end
    for tag, case_schema in pairs(item_schema.cases) do
      if value.values[tag] == nil then return fail(path..".values", "missing case tag "..string.format("%q",tag)) end
      local ok,err=validate_tree(value.values[tag],declaration,bindings,keys,path..".values."..tag,case_schema)
      if not ok then return nil,err end
    end
  end
  return true
end

local function deep_copy(value, seen)
  if type(value) ~= "table" then return value end
  seen = seen or {}; if seen[value] then error("cycle copied after validation") end; seen[value]=true
  local out={}; for key,child in pairs(value) do out[deep_copy(key,seen)]=deep_copy(child,seen) end
  seen[value]=nil; return out
end

function M.validate(description, declaration)
  local ok,err=validate_serializable(description,"preview"); if not ok then return nil,err end
  local bindings={}; ok,err=validate_tree(description,declaration or {},bindings,{},"preview",nil)
  if not ok then return nil,err end
  return {description=deep_copy(description),bindings=deep_copy(bindings)}
end

local function value_matches(schema, value)
  if schema == "data" then return true end
  if type(schema) == "string" then
    local optional=schema:sub(-1)=="?"; local expected=optional and schema:sub(1,-2) or schema
    if value==nil then return optional end
    if expected=="table" or expected=="object" then return type(value)=="table" end
    if expected=="array" then return type(value)=="table" and is_dense_array(value) end
    if expected=="number" or expected=="string" or expected=="boolean" then return type(value)==expected end
    return true
  end
  if type(schema)~="table" or type(value)~="table" then return false end
  if schema.kind=="record" then
    for name,child in pairs(schema.fields) do if value[name]==nil or not value_matches(child,value[name]) then return false end end
    for name in pairs(value) do if schema.fields[name]==nil then return false end end
    return true
  elseif schema.kind=="variant" then
    local case=schema.cases[value[schema.tag]]
    if case==nil then return false end
    local payload={}
    for name,child in pairs(value) do if name~=schema.tag then payload[name]=child end end
    return value_matches(case,payload)
  elseif schema.kind=="list" then
    if not is_dense_array(value) then return false end
    for i=1,#value do if not value_matches(schema.item,value[i]) then return false end end
    return true
  end
  return false
end

local function patch_matches(schema, value)
  if type(value) ~= "table" or is_dense_array(value) then return false end
  if type(schema) ~= "table" or schema.kind ~= "record" then return false end
  for name, child in pairs(value) do
    local child_schema = schema.fields[name]
    if child_schema == nil or not value_matches(child_schema, child) then return false end
  end
  return true
end

function M.validate_update(validated, operation, name, value)
  local binding=validated and validated.bindings and validated.bindings[name]
  if not binding then return nil,"unknown preview binding "..string.format("%q",tostring(name)) end
  if operation=="set" and binding.kind~="state" then return nil,"set requires a state binding" end
  if operation=="update" and binding.kind~="state" then return nil,"update requires a state binding" end
  if operation=="append" and binding.kind~="stream" then return nil,"append requires a stream binding" end
  if operation~="set" and operation~="update" and operation~="append" then return nil,"unknown preview operation "..tostring(operation) end
  local ok=validate_serializable(value,"preview update"); if not ok then return nil,"preview value is not serializable" end
  if operation == "update" then
    if not patch_matches(binding.schema, value) then return nil,"preview patch does not conform to binding schema" end
  elseif not value_matches(binding.schema,value) then
    return nil,"preview value does not conform to binding schema"
  end
  return true
end

function M.binding_contract(validated) return deep_copy(validated.bindings or {}) end
function M.owned(value, path)
  local ok, err = validate_serializable(value, path or "declaration")
  if not ok then return nil, err end
  return deep_copy(value)
end
M.deep_copy=deep_copy
return M
