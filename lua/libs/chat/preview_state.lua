-- TUI-local projection of generic MAG and capability facts.
-- MAG owns canonical actor/arrival/firing data; this consumer chooses aliases,
-- transcript groupings, and display-oriented state without emitting anything.
local common = require("libs.chat.common")
local shallow_merge = common.shallow_merge

local M = { STALE_MS = 30000 }

local function is_json_null(value)
  if type(value) ~= "userdata" then return false end
  if type(nefor) ~= "table" or type(nefor.json) ~= "table"
      or type(nefor.json.is_null) ~= "function" then return true end
  local ok, result = pcall(nefor.json.is_null, value)
  return ok and result == true
end

local function present(value) return value ~= nil and not is_json_null(value) end

local function copy_map(values)
  local out = {}
  for k, v in pairs(values or {}) do out[k] = v end
  return out
end

local function copy_array(values)
  local out = {}
  for i, v in ipairs(values or {}) do out[i] = v end
  return out
end

local function put_node(state, run_id, actor_id, fn)
  local all = copy_map(state.node_previews)
  local run = copy_map(all[run_id])
  run[actor_id] = fn(run[actor_id] or { states = {}, streams = {}, inputs = {}, outputs = {} })
  all[run_id] = run
  return shallow_merge(state, { node_previews = all })
end

local function append_stream(state, run_id, actor_id, binding, value, now_ms)
  local seq = (state.projection_seq or 0) + 1
  local next_state = put_node(state, run_id, actor_id, function(prev)
    local next = copy_map(prev)
    next.last_activity_ms, next.last_activity_kind = now_ms, binding
    next.streams = copy_map(prev.streams)
    local values = copy_array(next.streams[binding])
    local previous = values[#values]
    local previous_value = previous and previous.value
    if type(previous_value) == "table" and type(value) == "table"
        and previous_value.kind == value.kind
        and (value.kind == "reasoning" or value.kind == "assistant")
        and type(previous_value.text) == "string" and type(value.text) == "string" then
      local joined = copy_map(previous_value)
      joined.text = previous_value.text .. value.text
      values[#values] = { value = joined, seq = previous.seq, at_ms = now_ms }
    else
      values[#values + 1] = { value = value, seq = seq, at_ms = now_ms }
    end
    next.streams[binding] = values
    return next
  end)
  return shallow_merge(next_state, { projection_seq = seq })
end

function M.set_scope(state, scope, run_id)
  if type(scope) ~= "string" or scope == "" or type(run_id) ~= "string" then return state end
  local scopes = copy_map(state.scope_to_run)
  scopes[scope] = run_id
  return shallow_merge(state, { scope_to_run = scopes })
end

function M.spawn(state, run_id, actor_id, factory, spec, now_ms)
  if type(run_id) ~= "string" or type(actor_id) ~= "string" then return state end
  return put_node(state, run_id, actor_id, function(prev)
    local next = copy_map(prev)
    next.factory, next.status = factory, "pending"
    next.spec = type(spec) == "table" and spec or {}
    next.params = next.spec.params or {}
    next.started_at_ms, next.last_activity_ms = now_ms, now_ms
    return next
  end)
end

function M.lifecycle(state, run_id, actor_id, status, now_ms, detail)
  if not (((state.node_previews or {})[run_id] or {})[actor_id]) then return state end
  return put_node(state, run_id, actor_id, function(prev)
    local next = copy_map(prev)
    next.status, next.last_activity_ms = status, now_ms
    if detail ~= nil then next.lifecycle_detail = detail end
    if status == "working" then next.activation_started_at_ms = now_ms end
    if status == "done" or status == "failed" or status == "killed" then next.finished_at_ms = now_ms end
    return next
  end)
end

function M.finish_run(state, run_id, status, now_ms)
  local existing = (state.node_previews or {})[run_id]
  if not existing then return state end
  local all = copy_map(state.node_previews)
  local run = {}
  for id, node in pairs(existing) do
    local next = copy_map(node)
    if next.status ~= "killed" and next.status ~= "failed" then next.status = status end
    next.finished_at_ms = next.finished_at_ms or now_ms
    run[id] = next
  end
  all[run_id] = run
  return shallow_merge(state, { node_previews = all })
end

function M.arrival(state, msg, now_ms)
  local run_id, arrival_id = msg.run_id, msg.arrival_id
  if type(run_id) ~= "string" or type(arrival_id) ~= "string" then return state end
  local all = copy_map(state.mag_arrivals)
  local run = copy_map(all[run_id])
  run[arrival_id] = msg
  all[run_id] = run
  local next = shallow_merge(state, { mag_arrivals = all })
  local actor_id = msg.from
  if not (((next.node_previews or {})[run_id] or {})[actor_id]) then return next end
  return put_node(next, run_id, actor_id, function(prev)
    local node = copy_map(prev)
    node.outputs = copy_map(prev.outputs)
    local binding = type(msg.wire) == "string" and msg.wire or msg.semantic_type_id or "output"
    node.outputs[binding], node.outputs.last = msg.value, msg.value
    node.last_activity_ms, node.last_activity_kind = now_ms, "output"
    return node
  end)
end

function M.firing(state, msg, now_ms)
  local run_id, actor_id = msg.run_id, msg.id
  local arrivals = (state.mag_arrivals or {})[run_id] or {}
  if type(run_id) ~= "string" or type(actor_id) ~= "string"
      or not (((state.node_previews or {})[run_id] or {})[actor_id]) then return state end
  return put_node(state, run_id, actor_id, function(prev)
    local node, values = copy_map(prev), {}
    node.inputs = copy_map(prev.inputs)
    for _, ref in ipairs(msg.arrivals or {}) do
      local arrival = arrivals[ref.arrival_id]
      if arrival then
        local binding = type(arrival.wire) == "string" and arrival.wire
          or arrival.semantic_type_id or msg.port or "input"
        node.inputs[binding] = arrival.value
        values[#values + 1] = arrival.value
      end
    end
    if #values == 1 then node.inputs.last = values[1]
    elseif #values > 1 then node.inputs.last = values end
    node.last_activity_ms, node.last_activity_kind = now_ms, "firing"
    return node
  end)
end

function M.diagnostic(state, msg, now_ms)
  if type(msg.run_id) ~= "string" or type(msg.from) ~= "string"
      or type(msg.diagnostic) ~= "table" then return state end
  return append_stream(state, msg.run_id, msg.from, "diagnostic",
    { kind = "diagnostic", value = msg.diagnostic }, now_ms)
end

local function owner_from_invocation(state, msg)
  local invocation = msg.invocation
  if type(invocation) ~= "table" or type(invocation.run_id) ~= "string"
      or type(invocation.actor_id) ~= "string" then return nil end
  if not (((state.node_previews or {})[invocation.run_id] or {})[invocation.actor_id]) then return nil end
  return { run_id = invocation.run_id, actor_id = invocation.actor_id,
    name = msg.name or msg.tool }
end

function M.observe_capability(state, msg, now_ms)
  local kind = msg.kind or ""
  local owners = state.capability_owners or {}
  local owner = owners[msg.request_id or msg.id]

  if kind:match("%.completion%.request$") or kind:match("%.tool%.invoke$")
      or kind == "tool.invoke" then
    owner = owner_from_invocation(state, msg)
    local id = msg.request_id or msg.id
    if owner and type(id) == "string" then
      local next_owners = copy_map(owners)
      next_owners[id] = owner
      state = shallow_merge(state, { capability_owners = next_owners })
      if kind:match("%.tool%.invoke$") or kind == "tool.invoke" then
        state = append_stream(state, owner.run_id, owner.actor_id, "capability",
          { kind = "tool_call", value = { id = id, name = msg.name or msg.tool,
            arguments = msg.args or msg.arguments } }, now_ms)
      end
    end
    return state
  end

  if not owner then return state end
  if kind:match("%.completion%.event$") then
    local event = msg.event
    if event == "text_delta" and type(msg.text) == "string" then
      return append_stream(state, owner.run_id, owner.actor_id, "provider",
        { kind = "assistant", text = msg.text }, now_ms)
    elseif event == "reasoning_delta" and type(msg.text) == "string" then
      return append_stream(state, owner.run_id, owner.actor_id, "provider",
        { kind = "reasoning", text = msg.text }, now_ms)
    elseif event == "tool_call" then
      return append_stream(state, owner.run_id, owner.actor_id, "provider",
        { kind = "tool_call", value = { id = msg.id, name = msg.name,
          arguments = msg.arguments } }, now_ms)
    end
  elseif kind == "tool.stream" then
    return append_stream(state, owner.run_id, owner.actor_id, "capability",
      { kind = msg.stream or msg.channel or "stdout", text = msg.text or msg.delta or "" }, now_ms)
  elseif kind == "tool.result" or kind:match("%.tool%.result$") then
    local failed = present(msg.error)
    local result = { id = msg.id, name = owner.name }
    if failed then result.error = msg.error
    else result.output = present(msg.result) and msg.result or msg.output end
    return append_stream(state, owner.run_id, owner.actor_id, "capability",
      { kind = failed and "error" or "tool_result", value = result }, now_ms)
  end
  return state
end

function M.node(state, run_id, actor_id)
  return (((state.node_previews or {})[run_id] or {})[actor_id])
end

function M.merged(state, run_id, predicate)
  local items, last_ms, last_kind = {}, nil, nil
  for actor_id, node in pairs((state.node_previews or {})[run_id] or {}) do
    if not predicate or predicate(actor_id) then
      for binding, values in pairs(node.streams or {}) do
        for _, item in ipairs(values) do
          items[#items + 1] = { actor_id = actor_id, binding = binding, item = item }
        end
      end
      if node.last_activity_ms and (not last_ms or node.last_activity_ms > last_ms) then
        last_ms, last_kind = node.last_activity_ms, node.last_activity_kind
      end
    end
  end
  table.sort(items, function(a, b)
    local x, y = a.item.seq or math.huge, b.item.seq or math.huge
    if x ~= y then return x < y end
    local ax, ay = a.item.at_ms or 0, b.item.at_ms or 0
    if ax ~= ay then return ax < ay end
    return a.actor_id < b.actor_id
  end)
  return items, last_ms, last_kind
end

function M.prune(state, runs)
  local all, arrivals, changed = {}, {}, false
  for run_id, value in pairs(state.node_previews or {}) do
    if (runs or {})[run_id] then
      all[run_id], arrivals[run_id] = value, (state.mag_arrivals or {})[run_id]
    else changed = true end
  end
  if not changed then return state end
  local scopes = {}
  for scope, run_id in pairs(state.scope_to_run or {}) do if (runs or {})[run_id] then scopes[scope] = run_id end end
  local owners = {}
  for id, owner in pairs(state.capability_owners or {}) do
    if (runs or {})[owner.run_id] then owners[id] = owner end
  end
  return shallow_merge(state, { node_previews = all, mag_arrivals = arrivals,
    scope_to_run = scopes, capability_owners = owners })
end

return M
