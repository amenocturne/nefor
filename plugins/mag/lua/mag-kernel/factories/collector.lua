-- Deterministic typed fan-in. Sender identity comes from the kernel activation
-- triple, never from the payload. One value is retained per expected sender.
local kinds = require("kinds")
local M = {}
local preview_components = require("preview-components")

local ITEM = "nefor.dynamic.Item"
local COLLECTED = "nefor.dynamic.Collected"

M.declaration = {
  preview = preview_components.input_output(),
  name = "collector",
  type_variables = { "T" },
  semantic = {
    input={kind="variable",name="T"},
    output={kind="list",item={kind="variable",name="T"}},
    inputs={{wire=ITEM,type={kind="variable",name="T"}}},
    outputs={{wire=COLLECTED,type={kind="list",item={kind="variable",name="T"}}}},
  },
  params = { expected_senders = "table" },
  inputs = { item = ITEM },
  outputs = { COLLECTED },
  signals = { "kill", "drain" },
}

function M.construct(id, params, emit)
  local expected = params and params.expected_senders
  if type(expected) ~= "table" or #expected == 0 then
    return nil, string.format("collector '%s': expected_senders must be a non-empty list", tostring(id))
  end
  local positions = {}
  for index, sender in ipairs(expected) do
    if type(sender) ~= "string" or sender == "" then
      return nil, string.format("collector '%s': expected_senders[%d] must be an id", tostring(id), index)
    end
    if positions[sender] then
      return nil, string.format("collector '%s': duplicate expected sender '%s'", tostring(id), sender)
    end
    positions[sender] = index
  end

  local values, semantic_values, received, finished = {}, {}, 0, false
  local instance = { id = id }

  local function failure(code, sender)
    finished = true
    values = {}
    semantic_values = {}
    return {
      status = "failed",
      failure = kinds.Failed,
      value = { kind = code, collector = id, sender = sender },
    }
  end

  function instance.deliver(activation)
    local one = activation and activation.messages and activation.messages[1] or {}
    local sender = one.from
    local index = positions[sender]
    if finished then return failure("collector_already_finished", sender) end
    if not index then return failure("collector_unexpected_sender", sender) end
    if values[index] ~= nil then return failure("collector_duplicate_sender", sender) end

    -- The route wrapper is transport-only. Its `value` is the semantic T.
    values[index] = type(one.message) == "table" and one.message.value or nil
    if values[index] == nil then return failure("collector_missing_value", sender) end
    semantic_values[index] = values[index]
    local arrival = one.arrival
    if type(arrival) == "table" and type(arrival.declared_type) == "table"
        and arrival.declared_type.kind == "union" then
      semantic_values[index] = {
        type = arrival.constructor_id,
        value = values[index],
      }
    end
    received = received + 1
    if received == #expected then
      finished = true
      local ordered = {}
      local semantic_ordered = {}
      for i = 1, #expected do
        ordered[i] = values[i]
        semantic_ordered[i] = semantic_values[i]
      end
      values = {}
      semantic_values = {}
      emit({ kind = COLLECTED, from = id,
        value = ordered, semantic_value = semantic_ordered })
    end
    return { status = "ok" }
  end

  function instance.handle_kill()
    finished = true
    values = {}
    semantic_values = {}
  end

  function instance.handle_drain()
    if not finished then
      emit({ kind = kinds.failed, from = id, failure = kinds.Failed,
        value = { kind = "collector_drained_incomplete", collector = id } })
    end
    finished = true
    values = {}
    semantic_values = {}
  end

  emit({ kind = kinds.ready, from = id })
  return instance
end

return M
