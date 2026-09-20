-- Ordered actor routes and anonymous value transformations for one MAG run.
-- Actors remain the only endpoint inventory. Assembly state belongs to the
-- concrete destination and structural Assembly.path, never to a synthetic node.
local plain_data = require("plain-data")
local typed_value = require("typed-value")

local M = {}

local function dense(value)
  if type(value) ~= "table" then return false end
  local count = 0
  for key in pairs(value) do
    if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then return false end
    count = count + 1
  end
  for index = 1, count do if value[index] == nil then return false end end
  return true
end

local function endpoint(port)
  local tagged = type(port) == "table" and port.endpoint
  local value = type(tagged) == "table" and tagged.value
  if type(tagged) ~= "table" or tagged.constructor ~= "ActorEndpoint"
      or type(value) ~= "table" or type(value.id) ~= "string" or value.id == "" then
    return nil
  end
  return "actor", value.id
end

local function address(port)
  local _, id = endpoint(port)
  return id and ("actor:" .. #id .. ":" .. id .. ":" .. tostring(port.wire)) or nil
end

local function same_address(left, right)
  return address(left) ~= nil and address(left) == address(right)
end

local function descriptor_id(semantic, descriptor)
  local ok, value = pcall(semantic.id, descriptor)
  return ok and value or nil
end

local function validate_port(self, port, actors, label, output)
  local _, id = endpoint(port)
  if not id or type(port.wire) ~= "string" or port.wire == ""
      or type(port.type) ~= "table" or type(port.type_id) ~= "string"
      or descriptor_id(self.semantic, port.type) ~= port.type_id then
    return nil, label .. " is malformed"
  end
  local actor = actors[id]
  if not actor then return nil, label .. " references unknown actor " .. tostring(id) end
  local candidates = output and (actor.outputs or {}) or {actor.input}
  for _, candidate in ipairs(candidates) do
    if candidate and same_address(candidate, port) and candidate.type_id == port.type_id then return true end
  end
  return nil, label .. " is not a declared actor " .. (output and "output" or "input")
end

local function validate_transforms(self, input, transforms, label)
  if not dense(transforms) then return nil, label .. " transforms must be a dense list" end
  local current = input
  for index, step in ipairs(transforms) do
    local constructor, spec = type(step)=="table" and step.constructor, type(step)=="table" and step.value
    local step_label = string.format("%s transforms[%d]", label, index)
    if constructor == "Unit" then
      if type(spec)~="table" or not self.semantic.accepts(spec,current) then return nil,step_label.." has invalid Unit input" end
      current={kind="primitive",name="Unit"}
    elseif constructor == "Project" then
      if type(spec)~="table" or type(spec.index)~="number" or spec.index<0 or spec.index%1~=0
          or type(spec.input)~="table" or type(spec.output)~="table"
          or not self.semantic.accepts(spec.input,current) then return nil,step_label.." has invalid projection" end
      current=spec.output
    elseif constructor == "Pack" then
      if type(spec)~="table" or type(spec.owner)~="table" or type(spec.payload)~="table"
          or type(spec.constructor)~="string" or not self.semantic.accepts(spec.payload,current) then return nil,step_label.." has invalid pack" end
      local ok=pcall(self.semantic.constructor,spec.owner,spec.constructor); if not ok then return nil,step_label.." names an unknown constructor" end
      current=spec.owner
    elseif constructor == "Unpack" then
      if type(spec)~="table" or type(spec.owner)~="table" or type(spec.payload)~="table"
          or type(spec.constructor)~="string" or not self.semantic.accepts(spec.owner,current) then return nil,step_label.." has invalid unpack" end
      local ok=pcall(self.semantic.constructor,spec.owner,spec.constructor); if not ok then return nil,step_label.." names an unknown constructor" end
      current=spec.payload
    elseif constructor == "Assemble" then
      if type(spec)~="table" or not dense(spec.inputs) or not dense(spec.path)
          or type(spec.output)~="table" or type(spec.slot)~="number" or spec.slot<0
          or spec.slot%1~=0 or spec.slot>=#spec.inputs
          or not self.semantic.accepts(spec.inputs[spec.slot+1],current) then return nil,step_label.." has invalid assembly" end
      current=spec.output
    elseif constructor == "EmptyList" then
      if type(spec)~="table" then return nil,step_label.." has invalid list item" end
      current={kind="list",item=spec}
    else return nil,step_label.." has unknown constructor "..tostring(constructor) end
    if not descriptor_id(self.semantic,current) then return nil,step_label.." has invalid semantic evidence" end
  end
  return current
end

local function add_input_source(sources, port, descriptor)
  local destination = address(port)
  sources[destination] = sources[destination] or {port=port,types={}}
  local types = sources[destination].types
  types[#types + 1] = descriptor
end

local function validate_input_coverage(self, sources)
  for destination, input in pairs(sources) do
    local ok, covered = pcall(self.semantic.input_covered_by, input.port.type, input.types)
    if not ok or covered ~= true then
      return nil, "incomplete actor input coverage at " .. destination
    end
  end
  return true
end

local function copy_map(source)
  local result = {}
  for key, value in pairs(source) do result[key] = value end
  return result
end

local function payload_values(arrival)
  local payload = type(arrival.payload) == "table" and arrival.payload or {}
  local semantic = payload.semantic_value
  if semantic == nil then semantic = payload.value end
  return payload.value, semantic, payload
end

local function array_value(value)
  if value ~= nil then return value end
  -- Unit is Lua nil, but a product/list slot must remain present.
  return nefor.json.decode("null")
end

local function preserve_transport(source, target)
  for _, field in ipairs({"messages", "calls", "dynamic", "output_path"}) do
    if source[field] ~= nil then target[field] = plain_data.copy(source[field]) end
  end
end

function M:validate_value(descriptor, value, label, arrival)
  local dynamic = arrival and type(arrival.payload) == "table" and arrival.payload.dynamic
  if type(dynamic) == "table" and dynamic.kind == "complete" then return end
  local ok, validation = pcall(self.semantic.validate_value, descriptor, value)
  if not ok or type(validation) ~= "table" or validation.ok ~= true then
    error(label .. " has malformed semantic value", 0)
  end
end

function M:derived(arrival, descriptor, wire, value, semantic_value, constructor_id, preserve)
  local _, _, source = payload_values(arrival)
  local payload = {kind = wire}
  if preserve then preserve_transport(source, payload) end
  if value ~= nil then payload.value = plain_data.copy(value) end
  if semantic_value ~= nil then payload.semantic_value = plain_data.copy(semantic_value) end
  local id = assert(descriptor_id(self.semantic, descriptor), "transform descriptor has no semantic id")
  return typed_value.factory({
    arrival_id = arrival.arrival_id,
    from = arrival.from,
    edge_id = arrival.edge_id,
    type_id = id,
    type = descriptor,
    declared_type_id = id,
    declared_type = descriptor,
    constructor_id = constructor_id or id,
    protocol_wire = wire,
    product_position = -1,
    payload = payload,
    control_metadata = arrival.control_metadata,
  })
end

local function assembly_key(destination, path)
  local parts = {destination, "#"}
  for _, value in ipairs(path or {}) do parts[#parts + 1] = tostring(value) .. "." end
  return table.concat(parts)
end

function M:assemble(destination, spec, arrival, remaining)
  local key = assembly_key(destination, spec.path)
  local cohort = self.assemblies[key]
  if not cohort then
    cohort = {inputs = spec.inputs, output = spec.output, queues = {}}
    self.assemblies[key] = cohort
  end
  local slot = spec.slot + 1
  cohort.queues[slot] = cohort.queues[slot] or {}
  cohort.queues[slot][#cohort.queues[slot] + 1] = {arrival=arrival, remaining=remaining}
  for index = 1, #cohort.inputs do
    if not cohort.queues[index] or #cohort.queues[index] == 0 then return nil end
  end
  local values, semantics, entries = {}, {}, {}
  for index = 1, #cohort.inputs do
    entries[index] = table.remove(cohort.queues[index], 1)
    local value, semantic = payload_values(entries[index].arrival)
    values[index], semantics[index] = array_value(value), array_value(semantic)
    self:validate_value(cohort.inputs[index], semantics[index], "assembly slot " .. tostring(index - 1), entries[index].arrival)
  end
  if nefor.json and nefor.json.mark_array then
    values, semantics = nefor.json.mark_array(values), nefor.json.mark_array(semantics)
  end
  self:validate_value(cohort.output, semantics, "assembly output")
  local first = entries[1]
  return self:derived(first.arrival, cohort.output, first.arrival.protocol_wire,
    values, semantics, nil, false), first.remaining
end

function M:transform(destination, arrival, transforms, start)
  local current = arrival
  local index = start or 1
  while current and index <= #(transforms or {}) do
    local step = transforms[index]
    local constructor, spec = step.constructor, step.value
    local raw, semantic = payload_values(current)
    if constructor == "Unit" then
      self:validate_value(spec, semantic, "Unit input", current)
      current = self:derived(current, {kind="primitive",name="Unit"}, current.protocol_wire,
        nil, nil, nil, false)
    elseif constructor == "Project" then
      self:validate_value(spec.input, semantic, "Project input", current)
      local position = spec.index + 1
      current = self:derived(current, spec.output, current.protocol_wire,
        type(raw)=="table" and raw[position] or nil,
        type(semantic)=="table" and semantic[position] or nil, nil, true)
    elseif constructor == "Pack" then
      local evidence = self.semantic.constructor(spec.owner, spec.constructor)
      local dynamic = type(current.payload)=="table" and current.payload.dynamic
      if dynamic then
        current = self:derived(current, spec.owner, current.protocol_wire,
          raw, semantic, evidence.id, true)
      else
        current = self:derived(current, spec.owner, current.protocol_wire,
          {constructor=spec.constructor,value=raw},
          {constructor=spec.constructor,value=semantic}, evidence.id, true)
      end
    elseif constructor == "Unpack" then
      local dynamic = type(current.payload)=="table" and current.payload.dynamic
      if dynamic then
        local evidence = self.semantic.constructor(spec.owner, spec.constructor)
        if current.constructor_id ~= evidence.id then return nil end
        current = self:derived(current, spec.payload, current.protocol_wire,
          raw, semantic, evidence.payload_id, true)
      else
        if type(raw) ~= "table" or raw.constructor ~= spec.constructor then return nil end
        if type(semantic) ~= "table" or semantic.constructor ~= spec.constructor then
          error("Unpack semantic constructor differs from raw value", 0)
        end
        current = self:derived(current, spec.payload, current.protocol_wire,
          raw.value, semantic.value, nil, true)
      end
    elseif constructor == "Assemble" then
      local remaining = {}
      for rest = index + 1, #transforms do remaining[#remaining + 1] = transforms[rest] end
      current, remaining = self:assemble(destination, spec, current, remaining)
      if not current then return nil end
      transforms, index = remaining, 1
      goto continue
    elseif constructor == "EmptyList" then
      local empty = nefor.json and nefor.json.mark_array and nefor.json.mark_array({}) or {}
      current = self:derived(current, {kind="list",item=spec}, current.protocol_wire,
        empty, empty, nil, false)
    else
      error("unknown transform " .. tostring(constructor), 0)
    end
    index = index + 1
    ::continue::
  end
  return current
end

function M:preflight(mod)
  if type(mod) ~= "table" or not dense(mod.actors or {}) or not dense(mod.routes or {})
      or not dense(mod.messages or {}) or not dense(mod.kills or {}) or not dense(mod.nodes or {}) then
    return nil, "topology modification collections must be dense lists"
  end
  local actors = {}
  for id, actor in self.inventory.pairs() do actors[id] = actor end
  for index, actor in ipairs(mod.actors or {}) do
    if type(actor) ~= "table" or type(actor.id) ~= "string" or actor.id == "" then
      return nil, string.format("actors[%d] is malformed", index)
    end
    actors[actor.id] = actor
  end
  local routes = {table.unpack(self.routes)}
  local input_sources = {}
  for index, route in ipairs(routes) do
    local transformed, err = validate_transforms(self, route.from.type, route.transforms,
      string.format("existing routes[%d]", index))
    if not transformed then return nil, err end
    add_input_source(input_sources, route.to, transformed)
  end
  for index, route in ipairs(mod.routes or {}) do
    if type(route) ~= "table" or type(route.id) ~= "string" then return nil, string.format("routes[%d] is malformed", index) end
    local ok, err = validate_port(self, route.from, actors, "route source", true)
    if not ok then return nil, err end
    ok, err = validate_port(self, route.to, actors, "route destination", false)
    if not ok then return nil, err end
    local transformed; transformed,err=validate_transforms(self,route.from.type,route.transforms,string.format("routes[%d]",index))
    if not transformed then return nil,err end
    if not self.semantic.accepts(route.to.type,transformed) then return nil,string.format("routes[%d] transformed type is incompatible",index) end
    add_input_source(input_sources, route.to, transformed)
    routes[#routes + 1] = route
  end
  for index, message in ipairs(mod.messages or {}) do
    local ok, err = validate_port(self, message.to, actors, "message target", false)
    if not ok then return nil, err end
    if descriptor_id(self.semantic, message.semantic_type) ~= message.semantic_type_id then return nil,string.format("messages[%d] is malformed",index) end
    local transformed; transformed,err=validate_transforms(self,message.semantic_type,message.transforms,string.format("messages[%d]",index))
    if not transformed then return nil,err end
    if not self.semantic.accepts(message.to.type,transformed) then return nil,string.format("messages[%d] transformed type is incompatible",index) end
    add_input_source(input_sources, message.to, transformed)
  end
  local covered, coverage_error = validate_input_coverage(self, input_sources)
  if not covered then return nil, coverage_error end
  local boundary=mod.result and mod.result.from
  if boundary then
    if type(boundary.type)~="table" or descriptor_id(self.semantic,boundary.type)~=boundary.type_id
        or not dense(boundary.leaves) or not dense(boundary.through) then return nil,"result.from is malformed" end
    for index,leaf in ipairs(boundary.leaves) do
      local ok,err=validate_port(self,leaf.port,actors,"result leaf",true); if not ok then return nil,err end
      local transformed; transformed,err=validate_transforms(self,leaf.port.type,leaf.steps,string.format("result leaves[%d]",index))
      if not transformed then return nil,err end
      if not self.semantic.accepts(boundary.type,transformed) then return nil,"result leaf transformed type is incompatible" end
    end
    local unit={kind="primitive",name="Unit"}
    for index,flow in ipairs(boundary.through) do
      local transformed,err=validate_transforms(self,unit,flow.steps,string.format("result through[%d]",index))
      if not transformed then return nil,err end
      if not self.semantic.accepts(boundary.type,transformed) then return nil,"result through transformed type is incompatible" end
    end
  end
  return {routes=routes}
end

function M:snapshot()
  return {routes=self.routes, assemblies=copy_map(self.assemblies), initial_seq=self.initial_seq}
end

function M:restore(snapshot)
  self.routes, self.assemblies, self.initial_seq = snapshot.routes, snapshot.assemblies, snapshot.initial_seq
end

function M:install(_, state) self.routes = state.routes end
function M:clear() self.routes, self.assemblies, self.initial_seq = {}, {}, 0 end
function M:drain() return true end

function M:initial(message)
  self.initial_seq = self.initial_seq + 1
  local _, id = endpoint(message.to)
  local arrival = typed_value.initial({
    arrival_id = "initial:" .. tostring(self.initial_seq), from = "mag.control",
    type_id = message.semantic_type_id, type = message.semantic_type,
    declared_type_id = message.semantic_type_id, declared_type = message.semantic_type,
    constructor_id = message.semantic_type_id, protocol_wire = message.to.wire,
    product_position = -1, payload = plain_data.copy(message.content),
  })
  local transformed = self:transform(address(message.to), arrival, message.transforms or {})
  if transformed then
    transformed.protocol_wire = message.to.wire
    transformed.declared_type, transformed.declared_type_id = message.to.type, message.to.type_id
    self.dispatch("actor", id, message.to, transformed)
  end
end

function M:route(sender_id, wire, arrival)
  for _, route in ipairs(self.routes) do
    local _, source_id = endpoint(route.from)
    if source_id == sender_id and route.from.wire == wire then
      local transformed = self:transform(address(route.to), arrival, route.transforms or {})
      if transformed then
        local _, destination_id = endpoint(route.to)
        transformed.edge_id = route.id
        transformed.protocol_wire = route.to.wire
        transformed.declared_type, transformed.declared_type_id = route.to.type, route.to.type_id
        self.dispatch("actor", destination_id, route.to, transformed)
      end
    end
  end
end

-- Installed consumers and the terminal boundary own output reachability and
-- semantic evidence, including status outputs synthesized by the kernel.
function M:output_port(actor_id, wire)
  for _, route in ipairs(self.routes) do
    if route.from.endpoint.value.id == actor_id and route.from.wire == wire then
      return route.from
    end
  end
  for _, leaf in ipairs((self.result and self.result.leaves) or {}) do
    if leaf.port.endpoint.value.id == actor_id and leaf.port.wire == wire then
      return leaf.port
    end
  end
end

function M:set_result(boundary) self.result = boundary end

function M:observe_result(endpoint_value, wire, arrival)
  local boundary = self.result
  if not boundary then return false end
  local terminal = false
  for _, leaf in ipairs(boundary.leaves or {}) do
    if leaf.port.endpoint.constructor == endpoint_value.constructor
        and leaf.port.endpoint.value.id == endpoint_value.value.id and leaf.port.wire == wire then
      local transformed = self:transform("result", arrival, leaf.steps or {})
      local dynamic = transformed and type(transformed.payload)=="table" and transformed.payload.dynamic
      if transformed and (dynamic==nil or dynamic.kind=="complete") then
        local observed=plain_data.copy(transformed.payload)
        observed.kind="out"
        observed.semantic_type,observed.semantic_type_id=transformed.type,transformed.type_id
        observed.constructor_id,observed.arrival_id=transformed.constructor_id or transformed.type_id,transformed.arrival_id
        self.settle_result(endpoint_value.value.id, observed, observed)
        terminal = true
      end
    end
  end
  return terminal
end

function M:bootstrap_result()
  local boundary = self.result
  if not boundary then return false end
  local unit = {kind="primitive",name="Unit"}
  local settled = false
  for index, flow in ipairs(boundary.through or {}) do
    local arrival = typed_value.initial({arrival_id="result:"..tostring(index),from="mag.control",
      type_id=descriptor_id(self.semantic,unit),type=unit,constructor_id=descriptor_id(self.semantic,unit),
      protocol_wire="mag.Unit",product_position=-1,payload={kind="mag.Unit"}})
    local transformed = self:transform("result", arrival, flow.steps or {})
    if transformed then
      local observed=plain_data.copy(transformed.payload)
      observed.semantic_type,observed.semantic_type_id=transformed.type,transformed.type_id
      observed.constructor_id,observed.arrival_id=transformed.constructor_id,transformed.arrival_id
      self.settle_result("mag.result", observed, observed)
      settled = true
    end
  end
  return settled
end

function M.new(opts)
  return setmetatable({inventory=assert(opts.inventory),semantic=assert(opts.semantic),
    dispatch=assert(opts.dispatch),settle_result=assert(opts.settle_result),routes={},assemblies={},initial_seq=0}, {__index=M})
end

M.endpoint = endpoint
M.address = address
return M
