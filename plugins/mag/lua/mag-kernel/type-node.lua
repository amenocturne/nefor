local M = {}

local PRIMITIVES = { Data=true, Unit=true, Bool=true, Int=true, Float=true, String=true }
local KEYS = {
  primitive={kind=true,name=true}, variable={kind=true,name=true},
  named={kind=true,name=true,arguments=true}, list={kind=true,item=true},
  map={kind=true,key=true,value=true}, record={kind=true,fields=true},
  union={kind=true,items=true}, product={kind=true,items=true},
}

local function dense_array(value)
  if type(value) ~= "table" then return false end
  local count=0
  for key in pairs(value) do
    if type(key)~="number" or key<1 or key%1~=0 then return false end
    count=count+1
  end
  return count==#value
end

local function exact_keys(node, allowed)
  for key in pairs(node) do if not allowed[key] then return false end end
  for key in pairs(allowed) do if node[key]==nil then return false end end
  return true
end

local function validate(node, variables, path)
  path=path or "type"
  if type(node)~="table" or type(node.kind)~="string" or not KEYS[node.kind] then
    return nil,path.." has an illegal or missing kind"
  end
  if not exact_keys(node,KEYS[node.kind]) then return nil,path.." has missing or unknown fields" end
  if node.kind=="primitive" then
    if not PRIMITIVES[node.name] then return nil,path.." has an illegal primitive" end
  elseif node.kind=="variable" then
    if type(node.name)~="string" or not variables or not variables[node.name] then
      return nil,path.." has an unresolved variable"
    end
  elseif node.kind=="named" then
    if type(node.name)~="string" or node.name=="" or not node.name:find("%.") then
      return nil,path.." nominal name must be qualified"
    end
    if not dense_array(node.arguments) then return nil,path..".arguments must be a dense list" end
    for index,argument in ipairs(node.arguments) do
      local ok,err=validate(argument,variables,path..".arguments["..index.."]"); if not ok then return nil,err end
    end
  elseif node.kind=="list" then
    local ok,err=validate(node.item,variables,path..".item"); if not ok then return nil,err end
  elseif node.kind=="map" then
    local ok,err=validate(node.key,variables,path..".key"); if not ok then return nil,err end
    ok,err=validate(node.value,variables,path..".value"); if not ok then return nil,err end
  elseif node.kind=="record" then
    if not dense_array(node.fields) then return nil,path..".fields must be a dense list" end
    local previous=nil
    for index,field in ipairs(node.fields) do
      if type(field)~="table" or not exact_keys(field,{name=true,type=true}) or
          type(field.name)~="string" or field.name=="" or (previous and field.name<=previous) then
        return nil,path..".fields must have unique, sorted {name,type} entries"
      end
      previous=field.name
      local ok,err=validate(field.type,variables,path..".fields["..index.."].type"); if not ok then return nil,err end
    end
  else
    if not dense_array(node.items) or #node.items<2 then return nil,path..".items needs at least two types" end
    for index,item in ipairs(node.items) do
      local ok,err=validate(item,variables,path..".items["..index.."]"); if not ok then return nil,err end
    end
  end
  return true
end

function M.validate(node, variable_names)
  local variables=nil
  if variable_names then variables={}; for _,name in ipairs(variable_names) do variables[name]=true end end
  return validate(node,variables,"type")
end

function M.equal(left,right)
  if type(left)~=type(right) then return false end
  if type(left)~="table" then return left==right end
  for key,value in pairs(left) do if not M.equal(value,right[key]) then return false end end
  for key in pairs(right) do if left[key]==nil then return false end end
  return true
end

function M.substitute(node,bindings)
  if node.kind=="variable" then return bindings[node.name] end
  local copy={}
  for key,value in pairs(node) do
    if type(value)=="table" then
      if value.kind then copy[key]=M.substitute(value,bindings)
      else copy[key]={}; for index,item in ipairs(value) do
        if node.kind=="record" and key=="fields" then
          copy[key][index]={name=item.name,type=M.substitute(item.type,bindings)}
        else copy[key][index]=M.substitute(item,bindings) end
      end end
    else copy[key]=value end
  end
  return copy
end

return M
