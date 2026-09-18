-- Immutable structural topology for one MAG run. Actors retain lifecycle in
-- inventory; junctions and routes are plain run-scoped data and execute here.
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

local function deep_equal(left, right)
  if left == right then return true end
  if type(left) ~= "table" or type(right) ~= "table" then return false end
  for key, value in pairs(left) do
    if not deep_equal(value, right[key]) then return false end
  end
  for key in pairs(right) do if left[key] == nil then return false end end
  return true
end

local function exact_keys(value, allowed)
  if type(value) ~= "table" then return false end
  for key in pairs(value) do if not allowed[key] then return false end end
  for key in pairs(allowed) do if value[key] == nil then return false end end
  return true
end

local function endpoint(port)
  local tagged = type(port) == "table" and port.endpoint
  local value = type(tagged) == "table" and tagged.value
  if not exact_keys(tagged, {constructor = true, value = true})
      or not exact_keys(value, {id = true})
      or type(value.id) ~= "string" or value.id == "" then return nil end
  if tagged.constructor == "ActorEndpoint" then return "actor", value.id end
  if tagged.constructor == "JunctionEndpoint" then return "junction", value.id end
  return nil
end

local function address(port)
  local kind, id = endpoint(port)
  return kind and (kind .. ":" .. #id .. ":" .. id .. ":" .. tostring(port.wire)) or nil
end

local function actor_spec(actor)
  return {
    factory = actor.factory,
    type_arguments = actor.type_arguments,
    input = actor.input,
    outputs = actor.outputs,
    params = actor.params or {},
  }
end

local function semantic_id(semantic, descriptor, label)
  if type(descriptor) ~= "table" then return nil, label .. " has no semantic type" end
  local ok, value = pcall(semantic.id, descriptor)
  if not ok or type(value) ~= "string" or value == "" then
    return nil, label .. " has an invalid semantic type"
  end
  return value
end

local function validate_port(port, owner_kind, owner_id, label, semantic)
  local kind, id = endpoint(port)
  if kind ~= owner_kind or id ~= owner_id then
    return nil, label .. " endpoint does not belong to its definition"
  end
  if not exact_keys(port, {endpoint = true, type = true, type_id = true, wire = true})
      or type(port.wire) ~= "string" or port.wire == ""
      or type(port.type_id) ~= "string" or port.type_id == "" then
    return nil, label .. " is malformed"
  end
  local actual, err = semantic_id(semantic, port.type, label)
  if not actual then return nil, err end
  if actual ~= port.type_id then return nil, label .. " semantic type identity is mismatched" end
  return true
end

local function type_is(descriptor, kind, name)
  return type(descriptor) == "table" and descriptor.kind == kind
    and (name == nil or descriptor.name == name)
end

local function same_type(left, right)
  return left.type_id == right.type_id and deep_equal(left.type, right.type)
end

local function constructor_evidence(semantic, owner, name)
  if type(owner) ~= "table" or type(name) ~= "string" or name == "" then return nil end
  local ok, evidence = pcall(semantic.constructor, owner, name)
  if not ok or type(evidence) ~= "table" or type(evidence.id) ~= "string"
      or type(evidence.payload) ~= "table" or type(evidence.payload_id) ~= "string" then
    return nil
  end
  return evidence
end

local function validate_operation(junction, semantic)
  local operation = junction.operation
  if type(operation) ~= "table" or type(operation.constructor) ~= "string" then
    return nil, "junction " .. junction.id .. " has malformed operation wrapper"
  end
  for key in pairs(operation) do
    if key ~= "constructor" and key ~= "value" then
      return nil, "junction " .. junction.id .. " has malformed operation wrapper"
    end
  end
  local constructor = operation.constructor
  local inputs, outputs = junction.inputs, junction.outputs
  local arity = {
    Pass = {1, 1}, Unit = {1, 1},
    ProductSplit = {1, 2}, ProductFirst = {1, 1}, ProductJoin = {2, 1},
    Collect = {-1, 1}, AdtPack = {1, 1}, AdtUnpack = {1, -1},
  }
  local expected = arity[constructor]
  if not expected then
    return nil, "junction " .. junction.id .. " has unknown operation " .. constructor
  end
  if (expected[1] >= 0 and #inputs ~= expected[1])
      or (expected[2] >= 0 and #outputs ~= expected[2])
      or (constructor == "Collect" and #inputs == 0)
      or (constructor == "AdtUnpack" and #outputs == 0) then
    return nil, "junction " .. junction.id .. " has invalid " .. constructor .. " signature"
  end
  if constructor ~= "AdtPack" and constructor ~= "AdtUnpack" and operation.value ~= nil then
    local json = nefor and nefor.json
    if type(json) ~= "table" or type(json.is_null) ~= "function" or not json.is_null(operation.value) then
      return nil, "junction " .. junction.id .. " has malformed operation wrapper"
    end
  end

  local signature_error = "junction " .. junction.id .. " has invalid " .. constructor .. " signature"
  if constructor == "Pass" then
    if not same_type(inputs[1], outputs[1]) then return nil, signature_error end
  elseif constructor == "Unit" then
    if not type_is(outputs[1].type, "primitive", "Unit") then return nil, signature_error end
  elseif constructor == "ProductSplit" or constructor == "ProductFirst" then
    local product = inputs[1].type
    if not type_is(product, "product") or not dense(product.items) or #product.items ~= 2 then
      return nil, signature_error
    end
    local count = constructor == "ProductSplit" and 2 or 1
    for index = 1, count do
      local item_id = semantic_id(semantic, product.items[index], signature_error)
      if not item_id or item_id ~= outputs[index].type_id
          or not deep_equal(product.items[index], outputs[index].type) then return nil, signature_error end
    end
  elseif constructor == "ProductJoin" then
    local product = outputs[1].type
    if not type_is(product, "product") or not dense(product.items) or #product.items ~= 2 then
      return nil, signature_error
    end
    for index = 1, 2 do
      if not deep_equal(inputs[index].type, product.items[index]) then return nil, signature_error end
    end
  elseif constructor == "Collect" then
    local list = outputs[1].type
    if not type_is(list, "list") or type(list.item) ~= "table" then return nil, signature_error end
    for _, input in ipairs(inputs) do
      if not deep_equal(input.type, list.item) then return nil, signature_error end
    end
  elseif constructor == "AdtPack" then
    local spec = operation.value
    if not exact_keys(spec, {owner = true, payload = true, constructor = true})
        or type(spec.owner) ~= "table" or type(spec.payload) ~= "table"
        or type(spec.constructor) ~= "string" or spec.constructor == "" then
      return nil, "junction " .. junction.id .. " ADT pack spec is malformed"
    end
    local evidence = constructor_evidence(semantic, spec.owner, spec.constructor)
    if not evidence or not deep_equal(evidence.payload, spec.payload)
        or not deep_equal(inputs[1].type, spec.payload)
        or not deep_equal(outputs[1].type, spec.owner) then return nil, signature_error end
  elseif constructor == "AdtUnpack" then
    local spec = operation.value
    if not exact_keys(spec, {owner = true, branches = true})
        or type(spec.owner) ~= "table" or not dense(spec.branches)
        or #spec.branches == 0 or not deep_equal(inputs[1].type, spec.owner)
        or #outputs ~= #spec.branches then
      return nil, "junction " .. junction.id .. " ADT unpack spec is malformed"
    end
    local constructors, wires = {}, {}
    for index, branch in ipairs(spec.branches) do
      if not exact_keys(branch, {constructor = true, payload = true, wire = true})
          or type(branch.constructor) ~= "string"
          or branch.constructor == "" or type(branch.payload) ~= "table"
          or type(branch.wire) ~= "string" or branch.wire == ""
          or constructors[branch.constructor] or wires[branch.wire] then
        return nil, "junction " .. junction.id .. " ADT unpack spec is malformed"
      end
      constructors[branch.constructor], wires[branch.wire] = true, true
      local evidence = constructor_evidence(semantic, spec.owner, branch.constructor)
      local output
      for _, candidate in ipairs(outputs) do
        if candidate.wire == branch.wire then output = candidate break end
      end
      if not evidence or not deep_equal(evidence.payload, branch.payload) or not output
          or not deep_equal(output.type, branch.payload) then return nil, signature_error end
    end
  end
  return true
end

local function owner_for(kind, id, actors, junctions)
  if kind == "actor" then return actors[id] end
  if kind == "junction" then return junctions[id] end
  return nil
end

local function find_port(ports, candidate)
  for _, port in ipairs(ports or {}) do
    if port.wire == candidate.wire then return port end
  end
end

local function validate_actor(actor, index, semantic)
  local label = string.format("actors[%d]", index)
  if type(actor) ~= "table" or type(actor.id) ~= "string" or actor.id == ""
      or not dense(actor.outputs) then return nil, label .. " is malformed" end
  local ok, err = validate_port(actor.input, "actor", actor.id, label .. ".input", semantic)
  if not ok then return nil, err end
  local wires = {}
  for output_index, output in ipairs(actor.outputs) do
    ok, err = validate_port(output, "actor", actor.id,
      string.format("%s.outputs[%d]", label, output_index), semantic)
    if not ok then return nil, err end
    if wires[output.wire] then return nil, label .. " has duplicate output wire " .. output.wire end
    wires[output.wire] = true
  end
  return true
end

local function prospective(self, mod)
  local actors, junctions = {}, {}
  for id, record in self.inventory.pairs() do
    actors[id] = record
  end
  for id, definition in pairs(self.junctions) do junctions[id] = definition end

  for index, actor in ipairs(mod.actors or {}) do
    local ok, err = validate_actor(actor, index, self.semantic)
    if not ok then return nil, err end
    if actors[actor.id] and not deep_equal(actor_spec(actors[actor.id]), actor_spec(actor)) then
      return nil, "conflicting actor definition " .. actor.id
    end
    actors[actor.id] = actor
  end

  for index, junction in ipairs(mod.junctions or {}) do
    if type(junction) ~= "table" or type(junction.id) ~= "string" or junction.id == ""
        or not dense(junction.inputs) or not dense(junction.outputs) then
      return nil, string.format("junctions[%d] is malformed", index)
    end
    if junctions[junction.id] and not deep_equal(junctions[junction.id], junction) then
      return nil, "conflicting junction definition " .. junction.id
    end
    local seen = {}
    for port_index, port in ipairs(junction.inputs) do
      local ok, err = validate_port(port, "junction", junction.id,
        string.format("junctions[%d].inputs[%d]", index, port_index), self.semantic)
      if not ok then return nil, err end
      if seen[port.wire] then return nil, "duplicate junction input " .. junction.id .. "/" .. port.wire end
      seen[port.wire] = true
    end
    seen = {}
    for port_index, port in ipairs(junction.outputs) do
      local ok, err = validate_port(port, "junction", junction.id,
        string.format("junctions[%d].outputs[%d]", index, port_index), self.semantic)
      if not ok then return nil, err end
      if seen[port.wire] then return nil, "duplicate junction output " .. junction.id .. "/" .. port.wire end
      seen[port.wire] = true
    end
    local ok, err = validate_operation(junction, self.semantic)
    if not ok then return nil, err end
    junctions[junction.id] = junction
  end

  local routes, route_ids = {}, {}
  for _, route in ipairs(self.routes) do
    routes[#routes + 1] = route
    route_ids[route.id] = route
  end
  for index, route in ipairs(mod.routes or {}) do
    if type(route) ~= "table" or type(route.id) ~= "string" or route.id == ""
        or type(route.product_position) ~= "number" or route.product_position % 1 ~= 0 then
      return nil, string.format("routes[%d] is malformed", index)
    end
    if route_ids[route.id] then
      if not deep_equal(route_ids[route.id], route) then return nil, "conflicting route " .. route.id end
    else
      routes[#routes + 1] = route
      route_ids[route.id] = route
    end
  end

  local outgoing, incoming, junction_edges = {}, {}, {}
  for _, route in ipairs(routes) do
    local from_kind, from_id = endpoint(route.from)
    local to_kind, to_id = endpoint(route.to)
    local from_owner = owner_for(from_kind, from_id, actors, junctions)
    local to_owner = owner_for(to_kind, to_id, actors, junctions)
    if not from_owner or not to_owner then
      return nil, string.format("route %q references an unknown endpoint", route.id)
    end
    local declared_source = find_port(from_owner.outputs, route.from)
    local declared_destination = to_kind == "actor"
      and find_port({to_owner.input}, route.to) or find_port(to_owner.inputs, route.to)
    if not declared_source or not deep_equal(declared_source, route.from) then
      return nil, "route " .. route.id .. " source port is not declared"
    end
    if not declared_destination or not deep_equal(declared_destination, route.to) then
      return nil, "route " .. route.id .. " destination port is not declared"
    end
    local accepts_ok, accepts = pcall(self.semantic.accepts,
      declared_destination.type, declared_source.type)
    if not accepts_ok or accepts ~= true then
      return nil, "route " .. route.id .. " semantic types are incompatible"
    end
    if to_kind == "junction" and route.product_position ~= -1 then
      return nil, "route " .. route.id .. " to junction requires product_position -1"
    end
    if to_kind == "actor" then
      local destination_type = declared_destination.type
      if route.product_position < -1 then return nil, "route " .. route.id .. " has invalid actor product_position" end
      if route.product_position >= 0 then
        local component = destination_type.kind == "product" and destination_type.items[route.product_position+1]
        if not component or not deep_equal(component, declared_source.type) then
          return nil, "route " .. route.id .. " actor product_position does not match source type"
        end
      elseif not deep_equal(destination_type, declared_source.type) then
        return nil, "route " .. route.id .. " whole actor input requires exact type evidence"
      end
    elseif not deep_equal(declared_destination.type, declared_source.type) then
      return nil, "route " .. route.id .. " junction slot requires whole-value type evidence"
    end
    local source_address, destination_address = address(route.from), address(route.to)
    outgoing[source_address] = outgoing[source_address] or {}
    outgoing[source_address][#outgoing[source_address] + 1] = route
    incoming[destination_address] = incoming[destination_address] or {}
    incoming[destination_address][#incoming[destination_address] + 1] = route
    if from_kind == "junction" and to_kind == "junction" then
      junction_edges[from_id] = junction_edges[from_id] or {}
      junction_edges[from_id][to_id] = true
    end
  end

  for _, actor in pairs(actors) do
    local routes_for_input = incoming[address(actor.input)] or {}
    local positions, component_count = {}, 0
    for _, route in ipairs(routes_for_input) do
      if route.product_position >= 0 then
        if positions[route.product_position] then return nil, "duplicate actor product position at " .. actor.id end
        positions[route.product_position] = true; component_count=component_count+1
      end
    end
    if component_count > 0 and component_count ~= #(actor.input.type.items or {}) then
      return nil, "incomplete actor product input at " .. actor.id
    end
  end

  local visiting, visited = {}, {}
  local function visit(id)
    if visiting[id] then return nil, "junction-only cycle at " .. id end
    if visited[id] then return true end
    visiting[id] = true
    for next_id in pairs(junction_edges[id] or {}) do
      local ok, err = visit(next_id)
      if not ok then return nil, err end
    end
    visiting[id], visited[id] = nil, true
    return true
  end
  for id in pairs(junctions) do
    local ok, err = visit(id)
    if not ok then return nil, err end
  end

  for index, message in ipairs(mod.messages or {}) do
    if type(message) ~= "table" or type(message.semantic_type_id) ~= "string"
        or type(message.semantic_type) ~= "table" or type(message.content) ~= "table" then
      return nil, string.format("messages[%d] is malformed", index)
    end
    local actual = semantic_id(self.semantic, message.semantic_type,
      string.format("messages[%d]", index))
    if not actual or actual ~= message.semantic_type_id then
      return nil, string.format("messages[%d] semantic type identity is mismatched", index)
    end
    local kind, id = endpoint(message.to)
    local owner = owner_for(kind, id, actors, junctions)
    local input = owner and (kind == "actor"
      and find_port({owner.input}, message.to) or find_port(owner.inputs, message.to))
    -- Approval replies are a reserved actor control input, not its ordinary
    -- subject port. Inventory still proves an outstanding constructed gate.
    local approval = kind == "actor" and owner and message.content.kind == "mag.ApprovalReply"
      and (owner.factory == "human" or owner.factory == "nefor.factory.human")
    if approval then
      local descriptor = require("factories.human").approval_reply_type()
      input = {endpoint=message.to.endpoint,wire="mag.ApprovalReply",type=descriptor,
        type_id=self.semantic.id(descriptor)}
    end
    if not input or not deep_equal(input, message.to) then
      return nil, string.format("messages[%d] references an unknown input", index)
    end
    local accepts_ok, accepts = pcall(self.semantic.accepts, input.type, message.semantic_type)
    if not accepts_ok or accepts ~= true then
      return nil, string.format("messages[%d] semantic type is incompatible", index)
    end
    local semantic_value = message.content.semantic_value
    if approval then
      semantic_value = message.content
    else
      local raw_ok, raw_validation = pcall(self.semantic.validate_value,
        message.semantic_type, message.content.value)
      if kind == "junction"
          and (not raw_ok or type(raw_validation) ~= "table" or raw_validation.ok ~= true) then
        return nil, string.format("messages[%d] has malformed raw value", index)
      end
      if semantic_value == nil then semantic_value = message.content.value end
    end
    local valid_ok, validation = pcall(self.semantic.validate_value,
      message.semantic_type, semantic_value)
    if not valid_ok or type(validation) ~= "table" or validation.ok ~= true then
      return nil, string.format("messages[%d] has malformed semantic value", index)
    end
  end

  if mod.result ~= nil then
    local boundary = type(mod.result)=="table" and mod.result.from
    local kind,id = endpoint(boundary)
    local owner = owner_for(kind,id,actors,junctions)
    local output = owner and type(boundary)=="table" and find_port(owner.outputs,boundary)
    if not output or not deep_equal(output,boundary) then return nil,"result boundary is not a declared output" end
  end

  for index, kill in ipairs(mod.kills or {}) do
    if type(kill) ~= "string" or kill == "" or not actors[kill] then
      return nil, string.format("kills[%d] is not a live actor", index)
    end
  end
  for index, node in ipairs(mod.nodes or {}) do
    if type(node) ~= "table" or not dense(node.members or {}) then
      return nil, string.format("nodes[%d] is malformed", index)
    end
    for _, member in ipairs(node.members) do
      if type(member) ~= "string" or not actors[member] then
        return nil, string.format("nodes[%d] member %q is not an actor", index, tostring(member))
      end
    end
  end
  return {
    actors = actors,
    junctions = junctions,
    routes = routes,
    outgoing = outgoing,
    incoming = incoming,
  }
end

function M:preflight(mod)
  if type(mod) ~= "table" or not dense(mod.actors or {}) or not dense(mod.junctions or {})
      or not dense(mod.routes or {}) or not dense(mod.messages or {})
      or not dense(mod.kills or {}) or not dense(mod.nodes or {}) then
    return nil, "topology modification collections must be dense lists"
  end
  return prospective(self, mod)
end

local function copy_map(source)
  local result = {}
  for key, value in pairs(source) do result[key] = value end
  return result
end

function M:snapshot()
  return {
    junctions = self.junctions,
    routes = self.routes,
    outgoing = self.outgoing,
    incoming = self.incoming,
    slots = copy_map(self.slots),
    queue = {table.unpack(self.queue)},
  }
end

function M:restore(snapshot)
  self.junctions, self.routes = snapshot.junctions, snapshot.routes
  self.outgoing, self.incoming = snapshot.outgoing, snapshot.incoming
  self.slots, self.queue = snapshot.slots, snapshot.queue
  self.draining = false
end

function M:clear()
  self.junctions, self.routes, self.outgoing, self.incoming = {}, {}, {}, {}
  self.slots, self.queue, self.draining, self.initial_seq = {}, {}, false, 0
end

function M:install(mod, state)
  self.junctions, self.routes = state.junctions, state.routes
  self.outgoing, self.incoming = state.outgoing, state.incoming
  for id in pairs(state.junctions) do self.slots[id] = self.slots[id] or {} end

end

function M:initial(message)
  local kind, id = endpoint(message.to)
  self.initial_seq = self.initial_seq + 1
  local arrival = typed_value.initial({
    arrival_id = "initial:" .. tostring(self.initial_seq),
    from = "mag.control",
    type_id = message.semantic_type_id,
    type = message.semantic_type,
    declared_type_id = message.to.type_id,
    declared_type = message.to.type,
    constructor_id = message.semantic_type_id,
    protocol_wire = message.to.wire,
    product_position = -1,
    payload = plain_data.copy(message.content),
  })
  if kind == "junction" then self:enqueue(id, message.to, arrival)
  else self.dispatch(kind, id, message.to, arrival) end
end

function M:enqueue(id, port, arrival)
  self.queue[#self.queue + 1] = {id = id, port = port, arrival = arrival}
end

local function preserve_transport(source, target)
  for _, field in ipairs({"messages", "calls", "dynamic", "output_path"}) do
    if source[field] ~= nil then target[field] = plain_data.copy(source[field]) end
  end
end

local function json_null()
  local json = nefor and nefor.json
  if type(json) == "table" and type(json.decode) == "function" then
    return json.decode("null")
  end
  return nil
end

local function transformed_payload(arrival, wire, id, value, semantic_value, preserve)
  local source = type(arrival.payload) == "table" and arrival.payload or {}
  local payload = {kind = wire, from = id}
  if preserve then preserve_transport(source, payload) end
  if value ~= nil then payload.value = plain_data.copy(value) end
  if semantic_value ~= nil then payload.semantic_value = plain_data.copy(semantic_value) end
  return payload
end

local function semantic_value(arrival)
  local payload = type(arrival.payload) == "table" and arrival.payload or {}
  if payload.semantic_value ~= nil then return payload.semantic_value end
  return payload.value
end

local function dynamic_item_type(descriptor)
  if type(descriptor) == "table" and descriptor.kind == "named"
      and descriptor.name == "nefor.dynamic.DynamicList"
      and type(descriptor.arguments) == "table" and #descriptor.arguments == 1 then
    return descriptor.arguments[1]
  end
  return nil
end

function M:validate_value(descriptor, value, label, arrival)
  local protocol = arrival and type(arrival.payload) == "table" and arrival.payload.dynamic
  if type(protocol) == "table" then
    if protocol.kind == "complete" then return end
    descriptor = dynamic_item_type(descriptor)
    if descriptor == nil then return end
  end
  local ok, validation = pcall(self.semantic.validate_value, descriptor, value)
  if not ok or type(validation) ~= "table" or validation.ok ~= true then
    error(label .. " has malformed semantic value", 0)
  end
end

function M:emit(junction, output, arrival, value, semantic, constructor_id, preserve)
  local payload = transformed_payload(arrival, output.wire, junction.id,
    value, semantic, preserve)
  local emitted = typed_value.factory({
    arrival_id = arrival.arrival_id,
    from = junction.id,
    edge_id = "junction:" .. junction.id .. ":" .. output.wire,
    type_id = output.type_id,
    type = output.type,
    constructor_id = constructor_id or output.type_id,
    protocol_wire = output.wire,
    product_position = -1,
    payload = payload,
    control_metadata = arrival.control_metadata,
  })
  if self.observe(junction.id, output.wire, emitted, payload) == false then return false end
  for _, route in ipairs(self.outgoing[address(output)] or {}) do
    local kind, id = endpoint(route.to)
    local routed = typed_value.routed(emitted, {
      endpoint = route.to.endpoint,
      wire = route.to.wire,
      edge_id = route.id,
      destination_type_id = route.to.type_id,
      product_position = route.product_position,
    }, route.to.type)
    self.dispatch(kind, id, route.to, routed)
  end
  return true
end

function M:evaluate(item)
  local junction = self.junctions[item.id]
  if not junction then error("unknown junction " .. tostring(item.id), 0) end
  local operation, arrival = junction.operation, item.arrival
  local constructor, spec = operation.constructor, operation.value
  local input_index
  for index, input in ipairs(junction.inputs) do
    if input.wire == item.port.wire then input_index = index break end
  end
  if not input_index then error("junction input vanished: " .. item.id .. "/" .. item.port.wire, 0) end
  local slots = self.slots[item.id]
  slots[input_index] = slots[input_index] or {}
  slots[input_index][#slots[input_index] + 1] = arrival
  local required = (constructor == "ProductJoin" or constructor == "Collect")
    and #junction.inputs or 1
  for index = 1, required do
    if not slots[index] or #slots[index] == 0 then return end
  end

  local values, semantics, arrivals = {}, {}, {}
  for index = 1, required do
    arrivals[index] = table.remove(slots[index], 1)
    local payload = type(arrivals[index].payload) == "table" and arrivals[index].payload or {}
    values[index] = payload.value
    semantics[index] = semantic_value(arrivals[index])
  end
  local first = arrivals[1]

  if constructor == "Pass" then
    self:validate_value(junction.inputs[1].type, values[1], "Pass raw value at " .. junction.id, first)
    self:validate_value(junction.inputs[1].type, semantics[1], "Pass semantic value at " .. junction.id, first)
    self:emit(junction, junction.outputs[1], first, values[1], semantics[1],
      first.constructor_id, true)
  elseif constructor == "Unit" then
    self:validate_value(junction.inputs[1].type, values[1], "Unit raw value at " .. junction.id, first)
    self:validate_value(junction.inputs[1].type, semantics[1], "Unit semantic value at " .. junction.id, first)
    local unit = json_null()
    self:emit(junction, junction.outputs[1], first, unit, unit,
      junction.outputs[1].type_id, false)
  elseif constructor == "ProductSplit" or constructor == "ProductFirst" then
    self:validate_value(junction.inputs[1].type, values[1], constructor .. " raw value at " .. junction.id, first)
    self:validate_value(junction.inputs[1].type, semantics[1], constructor .. " semantic value at " .. junction.id, first)
    local count = constructor == "ProductSplit" and 2 or 1
    for index = 1, count do
      self:emit(junction, junction.outputs[index], first, values[1][index], semantics[1][index],
        junction.outputs[index].type_id, true)
    end
  elseif constructor == "ProductJoin" or constructor == "Collect" then
    for index, input in ipairs(junction.inputs) do
      local label = constructor .. " slot " .. tostring(index) .. " at " .. junction.id
      self:validate_value(input.type, values[index], label .. " raw value", arrivals[index])
      self:validate_value(input.type, semantics[index], label .. " semantic value", arrivals[index])
    end
    local joined_values = nefor.json.mark_array(values)
    local joined_semantics = nefor.json.mark_array(semantics)
    self:validate_value(junction.outputs[1].type, joined_semantics, constructor .. " at " .. junction.id)
    self:emit(junction, junction.outputs[1], first, joined_values, joined_semantics,
      junction.outputs[1].type_id, false)
  elseif constructor == "AdtPack" then
    local evidence = assert(constructor_evidence(self.semantic, spec.owner, spec.constructor))
    self:validate_value(spec.payload, values[1], "AdtPack raw payload at " .. junction.id, first)
    self:validate_value(spec.payload, semantics[1], "AdtPack semantic payload at " .. junction.id, first)
    local wrapped_value = {constructor = spec.constructor, value = values[1]}
    local dynamic = type(first.payload) == "table" and first.payload.dynamic
    local wrapped_semantic = dynamic ~= nil and semantics[1]
      or {constructor = spec.constructor, value = semantics[1]}
    if dynamic == nil then self:validate_value(spec.owner, wrapped_semantic, "AdtPack at " .. junction.id) end
    self:emit(junction, junction.outputs[1], first, wrapped_value, wrapped_semantic,
      evidence.id, true)
  elseif constructor == "AdtUnpack" then
    local wrapped = values[1]
    local dynamic = type(first.payload) == "table" and first.payload.dynamic
    local branch, evidence, payload_value, payload_semantic
    if dynamic ~= nil then
      for _, candidate in ipairs(spec.branches) do
        local candidate_evidence = constructor_evidence(self.semantic, spec.owner, candidate.constructor)
        if candidate_evidence and candidate_evidence.id == first.constructor_id then
          branch, evidence = candidate, candidate_evidence
          break
        end
      end
      if not branch then
        error("unexpected ADT constructor identity at junction " .. junction.id, 0)
      end
      payload_value, payload_semantic = wrapped, semantics[1]
    else
      if type(wrapped) ~= "table" or type(wrapped.constructor) ~= "string"
          or wrapped.value == nil then error("malformed ADT at junction " .. junction.id, 0) end
      for _, candidate in ipairs(spec.branches) do
        if candidate.constructor == wrapped.constructor then branch = candidate break end
      end
      if not branch then
        error("unexpected ADT constructor at junction " .. junction.id .. ": " .. wrapped.constructor, 0)
      end
      evidence = assert(constructor_evidence(self.semantic, spec.owner, branch.constructor))
      if type(semantics[1]) ~= "table" or semantics[1].constructor ~= wrapped.constructor
          or semantics[1].value == nil then
        error("malformed ADT semantic wrapper at junction " .. junction.id, 0)
      end
      self:validate_value(spec.owner, wrapped, "AdtUnpack raw value at " .. junction.id)
      self:validate_value(spec.owner, semantics[1], "AdtUnpack semantic value at " .. junction.id)
      payload_value, payload_semantic = wrapped.value, semantics[1].value
    end
    if dynamic == nil then
      self:validate_value(branch.payload, payload_value, "AdtUnpack raw payload at " .. junction.id, first)
    end
    self:validate_value(branch.payload, payload_semantic, "AdtUnpack semantic payload at " .. junction.id, first)
    local output
    for _, candidate in ipairs(junction.outputs) do
      if candidate.wire == branch.wire then output = candidate break end
    end
    self:emit(junction, output, first, payload_value, payload_semantic,
      evidence.payload_id, true)
  end
end

function M:drain()
  if self.draining then return true end
  self.draining = true
  local current
  local ok, err = pcall(function()
    local index = 1
    while index <= #self.queue do
      local item = self.queue[index]
      current = item
      index = index + 1
      self:evaluate(item)
    end
    self.queue = {}
  end)
  self.draining = false
  if not ok then
    self.queue = {}
    return nil, string.format("junction %s input %s via route %s: %s",
      tostring(current and current.id), tostring(current and current.port.wire),
      tostring(current and current.arrival.edge_id), tostring(err))
  end
  return true
end

function M.new(opts)
  local self = {
    inventory = assert(opts.inventory),
    semantic = assert(opts.semantic),
    dispatch = assert(opts.dispatch),
    observe = assert(opts.observe),
    junctions = {},
    routes = {},
    outgoing = {},
    incoming = {},
    slots = {},
    queue = {},
    draining = false,
    initial_seq = 0,
  }
  return setmetatable(self, {__index = M})
end

M.endpoint = endpoint
M.address = address
return M
