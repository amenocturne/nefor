-- Current-session materialization of factory-declared node previews.
-- This module owns observation only: it never emits onto the bus.
local common = require("libs.chat.common")
local shallow_merge = common.shallow_merge

local M = { STALE_MS = 30000 }

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

function M.set_scope(state, scope, run_id)
  if type(scope) ~= "string" or scope == "" or type(run_id) ~= "string" then return state end
  local scopes = copy_map(state.scope_to_run)
  scopes[scope] = run_id
  return shallow_merge(state, { scope_to_run = scopes })
end

function M.advertise(state, contracts)
  if type(contracts) ~= "table" then return state end
  local registry = copy_map(state.preview_registry)
  for _, contract in ipairs(contracts) do
    if type(contract) == "table" and type(contract.preview) == "table" then
      if type(contract.implementation) == "string" then registry[contract.implementation] = contract end
      if type(contract.identity) == "string" then registry[contract.identity] = contract end
    end
  end
  return shallow_merge(state, { preview_registry = registry })
end

function M.spawn(state, run_id, actor_id, factory, now_ms)
  if type(run_id) ~= "string" or type(actor_id) ~= "string" then return state end
  return put_node(state, run_id, actor_id, function(prev)
    local next = copy_map(prev)
    next.factory, next.status = factory, "pending"
    next.started_at_ms, next.last_activity_ms = now_ms, now_ms
    return next
  end)
end

function M.lifecycle(state, run_id, actor_id, status, now_ms)
  if not (((state.node_previews or {})[run_id] or {})[actor_id]) then return state end
  return put_node(state, run_id, actor_id, function(prev)
    local next = copy_map(prev)
    next.status, next.last_activity_ms = status, now_ms
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

local function binding_kind(state, node, name)
  local contract = (state.preview_registry or {})[node.factory]
  return contract and contract.preview_bindings and contract.preview_bindings[name]
end

function M.observe(state, msg, now_ms)
  local run_id, actor_id = msg.run_id, msg.id
  local node = (((state.node_previews or {})[run_id] or {})[actor_id])
  if not node or type(msg.binding) ~= "string" then return state end
  local declared = binding_kind(state, node, msg.binding)
  if not declared then return state end
  local op = msg.operation
  if (op == "append" and declared.kind ~= "stream") or
      ((op == "set" or op == "update") and declared.kind ~= "state") then return state end
  return put_node(state, run_id, actor_id, function(prev)
    local next = copy_map(prev)
    next.last_activity_ms, next.last_activity_kind = now_ms, "preview." .. tostring(op)
    if op == "append" then
      next.streams = copy_map(prev.streams)
      local values = copy_array(next.streams[msg.binding])
      local previous = values[#values]
      local previous_value = previous and previous.value
      if type(previous_value) == "table" and type(msg.value) == "table"
          and previous_value.kind == msg.value.kind
          and (msg.value.kind == "reasoning" or msg.value.kind == "assistant")
          and type(previous_value.text) == "string" and type(msg.value.text) == "string" then
        local joined = copy_map(previous_value)
        joined.text = previous_value.text .. msg.value.text
        values[#values] = { value = joined, seq = previous.seq,
          at_ms = msg.at_ms or now_ms, last_seq = msg.observation_seq or previous.seq }
      else
        values[#values + 1] = { value = msg.value, seq = msg.observation_seq or math.huge, at_ms = msg.at_ms or now_ms }
      end
      next.streams[msg.binding] = values
    else
      next.states = copy_map(prev.states)
      if op == "update" and type(next.states[msg.binding]) == "table" and type(msg.value) == "table" then
        local merged = copy_map(next.states[msg.binding])
        for k, v in pairs(msg.value) do merged[k] = v end
        next.states[msg.binding] = merged
      else
        next.states[msg.binding] = msg.value
      end
    end
    return next
  end)
end

function M.apply_modification(state, msg, now_ms)
  local run_id = msg.run_id
  local modification = msg.modification
  if type(run_id) ~= "string" or type(modification) ~= "table" then return state end
  local next = state
  for _, actor in ipairs(modification.actors or {}) do
    if type(actor) == "table" and type(actor.id) == "string" then
      next = put_node(next, run_id, actor.id, function(prev)
        local node = copy_map(prev)
        node.params = actor.params or node.params or {}
        node.last_activity_ms = now_ms
        return node
      end)
    end
  end
  for _, message in ipairs(modification.messages or {}) do
    if type(message) == "table" and type(message.to) == "string" then
      next = put_node(next, run_id, message.to, function(prev)
        local node = copy_map(prev)
        node.inputs = copy_map(prev.inputs)
        node.inputs.last = message.message or message.content or message.value
        node.last_activity_ms = now_ms
        return node
      end)
    end
  end
  return next
end

function M.node(state, run_id, actor_id)
  return (((state.node_previews or {})[run_id] or {})[actor_id])
end

function M.contract(state, node)
  return node and (state.preview_registry or {})[node.factory] or nil
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
  local all, changed = {}, false
  for run_id, value in pairs(state.node_previews or {}) do
    if (runs or {})[run_id] then all[run_id] = value else changed = true end
  end
  if not changed then return state end
  local scopes = {}
  for scope, run_id in pairs(state.scope_to_run or {}) do if (runs or {})[run_id] then scopes[scope] = run_id end end
  return shallow_merge(state, { node_previews = all, scope_to_run = scopes })
end

return M
