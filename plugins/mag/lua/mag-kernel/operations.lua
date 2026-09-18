-- Declarative operation validation and v3 delta-template materialization.
local plain_data = require("plain-data")
local M = {}
local Topology = require("topology")

local function dense(value)
  if type(value) ~= "table" then return false end
  local n=0; for key in pairs(value) do if type(key)~="number" or key<1 or key%1~=0 then return false end; n=n+1 end
  for index=1,n do if value[index]==nil then return false end end
  return true
end
local function nonempty(value) return type(value)=="string" and value~="" end
local function exact(value, fields, label)
  if type(value)~="table" then return nil,label.." must be an object" end
  for key in pairs(value) do if fields[key]==nil then return nil,label.." has unknown field "..tostring(key) end end
  for key,required in pairs(fields) do if required and value[key]==nil then return nil,label.." requires "..key end end
  return true
end
local function type_id(descriptor)
  local host=nefor and nefor.semantic_type
  if type(host)~="table" or type(host.id)~="function" then return nil end
  local ok,result=pcall(host.id,descriptor); return ok and result or nil
end
local function validate_typed(descriptor,id,label)
  if not nonempty(id) or type_id(descriptor)~=id then return nil,label.." semantic descriptor identity is absent or mismatched" end
  return true
end
local function endpoint(ref)
  if type(ref)~="table" or type(ref.value)~="table" then return nil end
  local constructor=ref.constructor
  if constructor=="LocalActorRef" then return "actor","local",ref.value.slot end
  if constructor=="ExistingActorRef" then return "actor","existing",ref.value.id end
  if constructor=="LocalJunctionRef" then return "junction","local",ref.value.slot end
  if constructor=="ExistingJunctionRef" then return "junction","existing",ref.value.id end
  return nil
end
local function endpoint_value(kind,id)
  return {constructor=kind=="actor" and "ActorEndpoint" or "JunctionEndpoint",value={id=id}}
end
local function port_value(port,ids)
  local kind,scope,name=endpoint(port.endpoint)
  local id=scope=="local" and ids[kind..":"..name] or name
  return {endpoint=endpoint_value(kind,id),type=plain_data.copy(port.type),type_id=port.type_id,wire=port.wire}
end
local function same_endpoint(a,b)
  return nefor.json.encode(a)==nefor.json.encode(b)
end
local function edge_id(from_port,to_port) return nefor.json.encode({from=from_port,to=to_port}) end
M.edge_id=edge_id

local function initial_outputs(initial)
  local values={}
  for _,actor in ipairs((initial and initial.actors) or {}) do
    for _,port in ipairs(actor.outputs or {}) do values[nefor.json.encode(port.endpoint).."/"..port.wire]=port end
  end
  for _,junction in ipairs((initial and initial.junctions) or {}) do
    for _,port in ipairs(junction.outputs or {}) do values[nefor.json.encode(port.endpoint).."/"..port.wire]=port end
  end
  return values
end

local function path_value(root,path)
  if not dense(path) or #path==0 then return nil,false end
  local value=root
  for _,key in ipairs(path) do
    if type(key)~="string" or key=="" or type(value)~="table" then return nil,false end
    value=value[key]; if value==nil then return nil,false end
  end
  return value,true
end
local function overlapping(a,b)
  for index=1,math.min(#a,#b) do if a[index]~=b[index] then return false end end
  return true
end

local function preflight(initial,operations,registry)
  if not dense(operations) then return nil,"program.operations must be a dense list" end
  local outputs=initial_outputs(initial); local ids={}; local checked={}
  for index,operation in ipairs(operations) do
    local label=string.format("program.operations[%d]",index)
    local ok,err=exact(operation,{id=true,on=true,captures=true,expressions=true,template=true},label); if not ok then return nil,err end
    if not nonempty(operation.id) or ids[operation.id] then return nil,label.." id is absent or duplicate" end
    ids[operation.id]=true
    ok,err=exact(operation.on,{endpoint=true,type=true,type_id=true,wire=true},label..".on"); if not ok then return nil,err end
    local source=outputs[nefor.json.encode(operation.on.endpoint).."/"..operation.on.wire]
    if not source or source.type_id~=operation.on.type_id then return nil,label.." trigger is not an initial endpoint output" end
    ok,err=validate_typed(operation.on.type,operation.on.type_id,label..".on"); if not ok then return nil,err end
    local boundary=initial and initial.result and initial.result.from
    if type(boundary)=="table" and boundary.wire==operation.on.wire and same_endpoint(boundary.endpoint,operation.on.endpoint) then
      return nil,label.." may not bind the result boundary"
    end
    if type(operation.captures)~="table" or not dense(operation.expressions) or #operation.expressions==0 then
      return nil,label.." captures/expressions are malformed"
    end
    local expression_types={}
    for capture_id,capture in pairs(operation.captures) do
      if not nonempty(capture_id) or type(capture)~="table" then return nil,label.." has malformed capture" end
      ok,err=validate_typed(capture.semantic_type,capture.semantic_type_id,label..".captures."..capture_id); if not ok then return nil,err end
      local validation=nefor.semantic_type.validate_value(capture.semantic_type,capture.value)
      if type(validation)~="table" or validation.ok~=true then return nil,label.." capture value is malformed" end
    end
    for expression_index,envelope in ipairs(operation.expressions) do
      if type(envelope)~="table" then return nil,label.." malformed expression envelope" end
      local expression=envelope.value; local constructor=envelope.constructor
      local fields = constructor=="Trigger" and {id=true,result_type=true}
        or constructor=="Capture" and {id=true,result_type=true,capture=true}
        or constructor=="Field" and {id=true,result_type=true,record=true,field=true}
        or constructor=="IntToDecimalString" and {id=true,result_type=true,value=true}
        or constructor=="ConcatStrings" and {id=true,result_type=true,values=true}
      if not fields then return nil,label.." unknown expression constructor" end
      ok,err=exact(expression,fields,label.." expression"); if not ok then return nil,err end
      if type(expression)~="table" or not nonempty(expression.id) or not nonempty(expression.result_type)
          or expression_types[expression.id] then return nil,label.." has malformed expression" end
      local descriptor
      if constructor=="Trigger" then descriptor=operation.on.type
      elseif constructor=="Capture" then descriptor=(operation.captures[expression.capture] or {}).semantic_type
      elseif constructor=="Field" then
        local record=expression_types[expression.record]; local body=record and (record.kind=="named" and record.body or record)
        for _,field in ipairs((body and body.fields) or {}) do if field.name==expression.field then descriptor=field.type break end end
      elseif constructor=="IntToDecimalString" then
        local input=expression_types[expression.value]
        if not input or input.kind~="primitive" or input.name~="Int" then return nil,label.." decimal conversion requires an earlier Int expression" end
        descriptor={kind="primitive",name="String"}
      elseif constructor=="ConcatStrings" then
        if not dense(expression.values) then return nil,label.." concatenation requires an expression list" end
        for _,id in ipairs(expression.values) do
          local input=expression_types[id]
          if not input or input.kind~="primitive" or input.name~="String" then return nil,label.." concatenation requires earlier String expressions" end
        end
        descriptor={kind="primitive",name="String"}
      end
      if type_id(descriptor)~=expression.result_type then return nil,string.format("%s.expressions[%d] result semantic type is incorrect",label,expression_index) end
      expression_types[expression.id]=descriptor
    end
    local template=operation.template
    ok,err=exact(template,{types=true,actors=true,junctions=true,routes=true,messages=true,nodes=true,actor_reference_relocations=true},label..".template"); if not ok then return nil,err end
    for _,field in ipairs({"actors","junctions","routes","messages","nodes","actor_reference_relocations"}) do
      if not dense(template[field]) then return nil,label..".template."..field.." must be a dense list" end
    end
    local declarations_ok,declarations=pcall(nefor.semantic_type.validate_declarations,template.types)
    if not declarations_ok or declarations~=true then return nil,label..".template.types are invalid" end
    local slots={}
    local function add_slot(kind,value,slot_label)
      if type(value)~="table" then return nil,slot_label.." is not an object" end
      if not nonempty(value.slot) or slots[kind..":"..value.slot] then return nil,slot_label.." slot is invalid or duplicate" end
      if not expression_types[value.id] or type_id(expression_types[value.id])~=type_id({kind="primitive",name="String"}) then
        return nil,slot_label.." id expression must produce String"
      end
      slots[kind..":"..value.slot]=value; return true
    end
    for actor_index,actor in ipairs(template.actors) do
      ok,err=add_slot("actor",actor,string.format("%s.template.actors[%d]",label,actor_index)); if not ok then return nil,err end
      local declaration=registry:declaration(actor.factory)
      if not declaration then return nil,"unknown factory "..tostring(actor.factory) end
      if type(declaration.template)~="table" then return nil,label.." actor factory does not support templates" end
      if not dense(actor.outputs) or not dense(actor.parameter_bindings) or not dense(actor.type_arguments)
          or type(actor.params)~="table" then return nil,label.." malformed template actor collections" end
      for key,value in pairs(declaration.template.parameter_equals or {}) do
        if actor.params[key]~=value then return nil,label.." factory template parameter constraint differs" end
      end
      local paths={}
      for _,binding in ipairs(actor.parameter_bindings) do
        local prior,present=path_value(actor.params,binding.path)
        local descriptor=expression_types[binding.value]
        if not present or not descriptor or descriptor.kind~="primitive"
            or (descriptor.name~="String" and descriptor.name~="Int") then return nil,label.." invalid scalar parameter binding" end
        if (descriptor.name=="String" and type(prior)~="string")
            or (descriptor.name=="Int" and (type(prior)~="number" or prior%1~=0)) then return nil,label.." scalar parameter binding type differs" end
        if (declaration.template.parameter_equals or {})[binding.path[1]]~=nil then return nil,label.." binding overrides factory constraint" end
        for _,path in ipairs(paths) do if overlapping(path,binding.path) then return nil,label.." parameter bindings overlap" end end
        paths[#paths+1]=binding.path
      end
    end
    for junction_index,junction in ipairs(template.junctions) do
      ok,err=add_slot("junction",junction,string.format("%s.template.junctions[%d]",label,junction_index)); if not ok then return nil,err end
    end
    local relocation_seen={}
    for _,relocation in ipairs(template.actor_reference_relocations) do
      local actor=type(relocation.actor)=="table" and slots["actor:"..tostring(relocation.actor.slot)]
      if not actor then return nil,label.." relocation actor is absent" end
      local declaration=registry:declaration(actor.factory)
      local key=nefor.json.encode({path=relocation.path,shape=relocation.shape})
      local allowed=false
      for _,candidate in ipairs(declaration.template.relocations or {}) do
        if key==nefor.json.encode({path=candidate.path,shape=candidate.shape}) then allowed=true end
      end
      relocation_seen[actor.slot]=relocation_seen[actor.slot] or {}
      if not allowed or relocation_seen[actor.slot][key] then return nil,label.." undeclared or duplicate relocation" end
      local referenced,present=path_value(actor.params,relocation.path)
      if not present then return nil,label.." relocation path is absent" end
      for _,binding in ipairs(actor.parameter_bindings) do
        if overlapping(binding.path,relocation.path) then return nil,label.." relocation overlaps scalar binding" end
      end
      local references=relocation.shape=="actor_id" and {referenced} or referenced
      if not dense(references) then return nil,label.." relocation references are malformed" end
      for _,slot in ipairs(references) do if not slots["actor:"..tostring(slot)] then return nil,label.." relocation references unknown local actor" end end
      relocation_seen[actor.slot][key]=true
    end
    for _,actor in ipairs(template.actors) do
      for _,relocation in ipairs(registry:declaration(actor.factory).template.relocations or {}) do
        local key=nefor.json.encode({path=relocation.path,shape=relocation.shape})
        if not (relocation_seen[actor.slot] or {})[key] then return nil,label.." missing required factory relocation" end
      end
    end
    local function validate_port(port,port_label)
      local kind,scope,name=endpoint(port and port.endpoint)
      local existing=false
      if scope=="existing" then
        for _,definition in ipairs((initial and initial[kind.."s"]) or {}) do
          if definition.id==name then existing=true; break end
        end
      end
      if not kind or (scope=="local" and not slots[kind..":"..tostring(name)]) or (scope=="existing" and (not nonempty(name) or not existing)) then
        return nil,port_label.." has invalid endpoint reference"
      end
      if not nonempty(port.wire) then return nil,port_label.." wire is absent" end
      return validate_typed(port.type,port.type_id,port_label)
    end
    for _,actor in ipairs(template.actors) do
      ok,err=validate_port(actor.input,label.." actor input"); if not ok then return nil,err end
      local kind,scope,name=endpoint(actor.input.endpoint); if kind~="actor" or scope~="local" or name~=actor.slot then return nil,label.." actor input ownership differs" end
      for _,output in ipairs(actor.outputs or {}) do ok,err=validate_port(output,label.." actor output"); if not ok then return nil,err end end
    end
    for _,junction in ipairs(template.junctions) do
      for _,input in ipairs(junction.inputs or {}) do ok,err=validate_port(input,label.." junction input"); if not ok then return nil,err end end
      for _,output in ipairs(junction.outputs or {}) do ok,err=validate_port(output,label.." junction output"); if not ok then return nil,err end end
    end
    for _,route in ipairs(template.routes) do
      ok,err=validate_port(route.from,label.." route source"); if not ok then return nil,err end
      ok,err=validate_port(route.to,label.." route destination"); if not ok then return nil,err end
      local to_kind=endpoint(route.to.endpoint)
      if to_kind=="junction" and route.product_position~=-1 then return nil,label.." route to junction requires product_position -1" end
    end
    for _,message in ipairs(template.messages) do
      ok,err=validate_port(message.to,label.." message target"); if not ok then return nil,err end
      ok,err=validate_typed(message.semantic_type,message.semantic_type_id,label.." message"); if not ok then return nil,err end
      if message.semantic_type_id~=message.to.type_id then return nil,label.." message type differs from its target" end
      if type(message.content)~="table" then return nil,label.." malformed message content" end
      if message.content.constructor=="Expression" then
        if type_id(expression_types[message.content.value])~=message.semantic_type_id then return nil,label.." message expression type differs" end
      elseif message.content.constructor=="Static" then
        local content=message.content.value
        local validation=nefor.semantic_type.validate_value(message.semantic_type,type(content)=="table" and content.value)
        if not validation or not validation.ok then return nil,label.." static message value is malformed" end
      else return nil,label.." unknown message content constructor" end
    end
    local normalized=plain_data.copy(operation)
    local trigger_endpoint=normalized.on.endpoint
    local trigger_kind=trigger_endpoint.constructor=="ActorEndpoint" and "actor" or "junction"
    local trigger_id=trigger_endpoint.value.id
    for _,node in ipairs(normalized.template.nodes) do
      if not dense(node.path) or #node.path==0 or not dense(node.members) then return nil,label.." malformed template logical node" end
      for _,member in ipairs(node.members) do
        if type(member)~="table" or not slots["actor:"..tostring(member.slot)] then return nil,label.." logical member is not a local actor" end
      end
      for index,segment in ipairs(node.path) do
        if type(segment)~="table" or type(segment.value)~="table" then return nil,label.." malformed template path segment" end
        if segment.constructor=="FixedPathSegment" then
          if not nonempty(segment.value.value) then return nil,label.." empty fixed path segment" end
        elseif segment.constructor=="BoundPathSegment" then
          local descriptor=expression_types[segment.value.value]
          if not descriptor or descriptor.kind~="primitive" or descriptor.name~="String" then return nil,label.." path binding requires String expression" end
        elseif segment.constructor~="TriggerPathSegment" or index~=1 then return nil,label.." invalid trigger path segment" end
      end
      if node.path[1] and node.path[1].constructor=="TriggerPathSegment" then
        if trigger_kind~="actor" then return nil,label.." trigger junction has no logical actor path" end
        local owner
        for _,initial_node in ipairs((initial and initial.nodes) or {}) do
          for _,member in ipairs(initial_node.members or {}) do if member==trigger_id then if owner then return nil,label.." trigger actor has multiple logical owners" end; owner=initial_node.path end end
        end
        if not owner then return nil,label.." trigger actor has no logical owner" end
        table.remove(node.path,1); local prefixed={}
        for _,part in ipairs(owner) do prefixed[#prefixed+1]={constructor="FixedPathSegment",value={value=part}} end
        for _,part in ipairs(node.path) do prefixed[#prefixed+1]=part end
        node.path=prefixed
      end
    end
    -- Validate template topology with deterministic placeholder identities. IDs
    -- are the only dynamic topology fields; signature and route rules are the
    -- same rules used by static apply, including actor-free workers.
    local placeholder_ids, reserved = {}, {}
    for _,kind in ipairs({"actors","junctions"}) do
      for _,definition in ipairs(initial[kind] or {}) do reserved[definition.id]=true end
    end
    for key in pairs(slots) do
      local id="template:"..key
      while reserved[id] do id=id..":" end
      reserved[id]=true; placeholder_ids[key]=id
    end
    local topology_mod={actors={},junctions={},routes={},messages={},nodes={},kills={},types=plain_data.copy(template.types)}
    for _,kind in ipairs({"actors","junctions"}) do
      for _,definition in ipairs(initial[kind] or {}) do topology_mod[kind][#topology_mod[kind]+1]=plain_data.copy(definition) end
    end
    for _,route in ipairs(initial.routes or {}) do topology_mod.routes[#topology_mod.routes+1]=plain_data.copy(route) end
    local local_actors={}
    for _,actor in ipairs(template.actors) do
      local value=plain_data.copy(actor)
      value.id=placeholder_ids["actor:"..actor.slot]; value.input=port_value(actor.input,placeholder_ids); value.outputs={}
      for i,port in ipairs(actor.outputs or {}) do value.outputs[i]=port_value(port,placeholder_ids) end
      topology_mod.actors[#topology_mod.actors+1]=value; local_actors[#local_actors+1]=value
    end
    for _,junction in ipairs(template.junctions) do
      local value=plain_data.copy(junction)
      value.id=placeholder_ids["junction:"..junction.slot]; value.inputs={}; value.outputs={}
      for i,port in ipairs(junction.inputs or {}) do value.inputs[i]=port_value(port,placeholder_ids) end
      for i,port in ipairs(junction.outputs or {}) do value.outputs[i]=port_value(port,placeholder_ids) end
      topology_mod.junctions[#topology_mod.junctions+1]=value
    end
    for i,route in ipairs(template.routes) do
      topology_mod.routes[i]={id="template-route:"..i,from=port_value(route.from,placeholder_ids),
        to=port_value(route.to,placeholder_ids),product_position=route.product_position}
    end
    local topology=Topology.new({inventory={pairs=function() return pairs({}) end},semantic=nefor.semantic_type,
      dispatch=function() end,observe=function() return true end})
    local state,topology_error=topology:preflight(topology_mod)
    if not state then return nil,label..": "..tostring(topology_error) end
    local actor_check=registry:validate_modification({actors=local_actors})
    if not actor_check.ok then return nil,label..": "..table.concat(actor_check.errors or {},"; ") end
    checked[#checked+1]=normalized
  end
  return checked
end

function M.preflight(initial,operations,registry)
  local ok,result,err=pcall(preflight,initial,operations,registry)
  if not ok then return nil,"malformed program operation: "..tostring(result) end
  return result,err
end

local function get_path(root,path)
  local current=root
  for _,part in ipairs(path or {}) do if type(current)~="table" then return nil,false end; current=current[part]; if current==nil then return nil,false end end
  return current,true
end
local function set_path(root,path,value)
  local current=root
  for index=1,#path-1 do current=current[path[index]]; if type(current)~="table" then return nil,"parameter path is absent" end end
  current[path[#path]]=plain_data.copy(value); return true
end

function M.materialize(operation,trigger_value)
  local values={}
  for _,envelope in ipairs(operation.expressions) do
    local expression=envelope.value; local value
    if envelope.constructor=="Capture" then value=operation.captures[expression.capture].value
    elseif envelope.constructor=="Field" then value=values[expression.record][expression.field]
    elseif envelope.constructor=="ConcatStrings" then local parts={}; for i,id in ipairs(expression.values) do parts[i]=values[id] end; value=table.concat(parts)
    elseif envelope.constructor=="IntToDecimalString" then value=tostring(values[expression.value])
    else value=trigger_value end
    values[expression.id]=plain_data.copy(value)
  end
  local template=operation.template; local ids={}; local used={}
  local function bind(kind,definition)
    local id=values[definition.id]
    if not nonempty(id) or used[kind..":"..id] then return nil,"materialized endpoint id is absent or duplicate "..tostring(id) end
    ids[kind..":"..definition.slot]=id; used[kind..":"..id]=true; return true
  end
  for _,actor in ipairs(template.actors) do local ok,err=bind("actor",actor); if not ok then return nil,err end end
  for _,junction in ipairs(template.junctions) do local ok,err=bind("junction",junction); if not ok then return nil,err end end
  local types=plain_data.copy(template.types)
  local function declare(port) types[port.type_id]=plain_data.copy(port.type) end
  local actors={}; local actor_by_slot={}
  for _,source in ipairs(template.actors) do
    local actor={id=ids["actor:"..source.slot],factory=source.factory,type_arguments=plain_data.copy(source.type_arguments),params=plain_data.copy(source.params),input=port_value(source.input,ids),outputs={}}
    declare(actor.input); for index,output in ipairs(source.outputs) do actor.outputs[index]=port_value(output,ids); declare(actor.outputs[index]) end
    for _,binding in ipairs(source.parameter_bindings or {}) do local ok,err=set_path(actor.params,binding.path,values[binding.value]); if not ok then return nil,err end end
    actors[#actors+1]=actor; actor_by_slot[source.slot]=actor
  end
  for _,relocation in ipairs(template.actor_reference_relocations or {}) do
    local actor=actor_by_slot[relocation.actor.slot]; local current,present=get_path(actor.params,relocation.path)
    if not present then return nil,"actor reference relocation path is absent" end
    if relocation.shape=="actor_id" then
      local id=ids["actor:"..current]; if not id then return nil,"actor relocation names unknown slot" end; set_path(actor.params,relocation.path,id)
    else
      local result={}; for index,slot in ipairs(current) do result[index]=ids["actor:"..slot]; if not result[index] then return nil,"actor relocation names unknown slot" end end
      set_path(actor.params,relocation.path,result)
    end
  end
  local junctions={}
  for _,source in ipairs(template.junctions) do
    local junction={id=ids["junction:"..source.slot],operation=plain_data.copy(source.operation),inputs={},outputs={}}
    for index,input in ipairs(source.inputs) do junction.inputs[index]=port_value(input,ids); declare(junction.inputs[index]) end
    for index,output in ipairs(source.outputs) do junction.outputs[index]=port_value(output,ids); declare(junction.outputs[index]) end
    junctions[#junctions+1]=junction
  end
  local routes={}
  for index,source in ipairs(template.routes) do
    local from,to=port_value(source.from,ids),port_value(source.to,ids); declare(from); declare(to)
    routes[index]={id=edge_id(from,to),from=from,to=to,product_position=source.product_position}
  end
  local messages={}
  for index,source in ipairs(template.messages) do
    local to=port_value(source.to,ids); declare(to)
    local value=source.content.constructor=="Static" and plain_data.copy(source.content.value)
      or {kind=to.wire,value=plain_data.copy(values[source.content.value])}
    messages[index]={to=to,semantic_type=plain_data.copy(source.semantic_type),semantic_type_id=source.semantic_type_id,content=value}
  end
  local nodes={}
  for index,source in ipairs(template.nodes) do
    local path={}; for part,segment in ipairs(source.path) do local raw=segment.value.value; path[part]=segment.constructor=="BoundPathSegment" and values[raw] or raw end
    local members={}; for member,ref in ipairs(source.members) do members[member]=ids["actor:"..ref.slot] end
    nodes[index]={path=path,members=members}
  end
  return {types=types,actors=actors,junctions=junctions,routes=routes,messages=messages,nodes=nodes,kills={}}
end

return M
