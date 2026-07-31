-- Trusted typed arrivals. Only the routing boundary constructs these values;
-- factory payload fields are copied as data and never become semantic
-- authority.

local M = {}
local trusted = setmetatable({}, { __mode = "k" })

local function build(fields)
  local arrival = {
    arrival_id = assert(fields.arrival_id, "typed arrival needs arrival_id"),
    from = fields.from,
    edge_id = fields.edge_id,
    type_id = assert(fields.type_id, "typed arrival needs type_id"),
    type = assert(fields.type, "typed arrival needs descriptor"),
    declared_type_id = fields.declared_type_id or fields.type_id,
    declared_type = fields.declared_type or fields.type,
    constructor_id = fields.constructor_id,
    protocol_wire = assert(fields.protocol_wire, "typed arrival needs protocol_wire"),
    product_position = fields.product_position or -1,
    payload = fields.payload,
    control_metadata = fields.control_metadata,
  }
  trusted[arrival] = true
  return arrival
end

function M.initial(fields)
  fields.edge_id = fields.edge_id or ("initial:" .. tostring(fields.arrival_id))
  return build(fields)
end

function M.factory(fields)
  return build(fields)
end

function M.routed(source, destination, declared_type)
  assert(trusted[source], "routed arrival must come from a trusted boundary")
  return build({
    arrival_id = source.arrival_id,
    from = source.from,
    edge_id = destination.edge_id or
      ("legacy:" .. tostring(source.from) .. ":" .. tostring(destination.actor)),
    type_id = source.type_id,
    type = source.type,
    declared_type_id = destination.destination_type_id,
    declared_type = declared_type,
    constructor_id = source.constructor_id,
    protocol_wire = assert(destination.wire, "compiled route needs destination wire"),
    product_position = destination.product_position,
    payload = source.payload,
    control_metadata = source.control_metadata,
  })
end

function M.is_trusted(value)
  return trusted[value] == true
end

return M
