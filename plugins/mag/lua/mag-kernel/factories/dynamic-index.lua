local kinds = require("kinds")
local node_approval = require("node-approval")

local M = {}
local INPUT = "nefor.dynamic.IndexInput"
local OUTPUT = "nefor.dynamic.Event"
local item = { kind = "variable", name = "T" }
local indexed = { kind = "named", name = "nefor.dynamic.Indexed", arguments = { item } }

M.declaration = {
  name = "dynamic-index",
  -- Tags one instance's result with its position; plumbing.
  approval = node_approval.inert("dynamic index"),
  type_variables = { "T", "R" },
  semantic = {
    input = item,
    output = { kind = "variable", name = "R" },
    inputs = { { wire = INPUT, type = item } },
    outputs = { { wire = "nefor.dynamic.Event", type = { kind = "variable", name = "R" } } },
  },
  params = { collection = "string", index = "number" },
  template = { relocations = {} },
  inputs = { value = INPUT },
  outputs = { OUTPUT },
  signals = {},
}

function M.construct(id, params, emit)
  if type(params) ~= "table" or type(params.collection) ~= "string"
      or type(params.index) ~= "number" or params.index < 0 or params.index % 1 ~= 0 then
    return nil, string.format("dynamic-index '%s': invalid collection/index", tostring(id))
  end
  local instance = { id = id }
  function instance.deliver(activation)
    local envelope = ((activation or {}).messages or {})[1] or {}
    local message = envelope.message or {}
    local value = message.value
    if value == nil then value = message end
    local indexed_value = {
      collection = params.collection, index = params.index, value = value,
    }
    local semantic_indexed_value = {
      collection = params.collection, index = params.index,
      value = message.semantic_value or value,
    }
    emit({ kind = OUTPUT, from = id,
      value = { constructor = "Item", value = indexed_value },
      semantic_value = { constructor = "Item", value = semantic_indexed_value } })
    return { status = "ok" }
  end
  emit({ kind = kinds.ready, from = id })
  return instance
end

return M
