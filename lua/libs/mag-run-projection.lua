-- Pure logical MAG run projection shared by the sidebar and model receipts.

local M = {}

local LIVE = { running = true, working = true }
local TERMINAL = { done = true, error = true, skipped = true, killed = true, failed = true }

local function path_key(path)
  local parts = {}
  for _, name in ipairs(path or {}) do parts[#parts + 1] = tostring(#name) .. ":" .. name end
  return table.concat(parts, "/")
end
M.path_key = path_key

local function parent_path(path)
  local parent = {}
  for index = 1, #path - 1 do parent[index] = path[index] end
  return parent
end

local function group_of(actor_id) return actor_id:match("^([^.]+)") or actor_id end
M.group_of = group_of

local function member_label(parent_id, actor_id)
  local prefix = parent_id .. "."
  if actor_id:sub(1, #prefix) == prefix then return actor_id:sub(#prefix + 1) end
  return actor_id
end
M.member_label = member_label

local function fallback_descriptors(nodes)
  local buckets, order = {}, {}
  for actor_id, node in pairs(nodes or {}) do
    local name = group_of(actor_id)
    local bucket = buckets[name]
    if not bucket then
      bucket = { name = name, entries = {}, min_seq = math.huge }
      buckets[name], order[#order + 1] = bucket, bucket
    end
    bucket.entries[#bucket.entries + 1] = { id = actor_id, node = node }
    bucket.min_seq = math.min(bucket.min_seq, node.seq or math.huge)
  end
  table.sort(order, function(left, right)
    if left.min_seq ~= right.min_seq then return left.min_seq < right.min_seq end
    return left.name < right.name
  end)
  local out = {}
  for _, bucket in ipairs(order) do
    table.sort(bucket.entries, function(left, right)
      local ls, rs = left.node.seq or math.huge, right.node.seq or math.huge
      if ls ~= rs then return ls < rs end
      return left.id < right.id
    end)
    if #bucket.entries == 1 and bucket.entries[1].id == bucket.name then
      out[#out + 1] = { path = { bucket.name }, members = { bucket.name } }
    else
      out[#out + 1] = { path = { bucket.name }, members = {} }
      for _, entry in ipairs(bucket.entries) do
        out[#out + 1] = {
          path = { bucket.name, member_label(bucket.name, entry.id) },
          members = { entry.id },
        }
      end
    end
  end
  return out
end

local function descriptors_for(run, nodes)
  if type(run.logical_nodes) == "table" and #run.logical_nodes > 0 then return run.logical_nodes end
  return fallback_descriptors(nodes or run.nodes)
end

local function actor_ancestor_keys(run, nodes)
  local out = {}
  for _, descriptor in ipairs(descriptors_for(run, nodes)) do
    for _, actor_id in ipairs(descriptor.members or {}) do
      local keys = out[actor_id] or {}
      for length = 1, #(descriptor.path or {}) do
        local prefix = {}
        for index = 1, length do prefix[index] = descriptor.path[index] end
        keys[path_key(prefix)] = true
      end
      out[actor_id] = keys
    end
  end
  return out
end

function M.active_groups(run, nodes)
  local out, ancestors = {}, actor_ancestor_keys(run, nodes)
  for id, node in pairs(nodes or run.nodes or {}) do
    if LIVE[node.status] then for key in pairs(ancestors[id] or {}) do out[key] = true end end
  end
  return out
end

function M.advance_group_activity(prev, nodes, now_ms)
  local current, next_activity, groups = prev.group_activity or {}, {}, M.active_groups(prev, nodes)
  for name, value in pairs(current) do
    next_activity[name] = { active_ms = value.active_ms or 0, active_since_ms = value.active_since_ms }
    groups[name] = groups[name] or false
  end
  for name, is_active in pairs(groups) do
    local item = next_activity[name] or { active_ms = 0 }
    if is_active and item.active_since_ms == nil then item.active_since_ms = now_ms
    elseif not is_active and item.active_since_ms ~= nil then
      item.active_ms, item.active_since_ms = item.active_ms + now_ms - item.active_since_ms, nil
    end
    next_activity[name] = item
  end
  return next_activity
end

local function empty_leaf_status(run)
  if run.completed_at_ms == nil then return "pending" end
  if run.status == "failed" or run.status == "error" then return "failed" end
  if run.status == "killed" or run.status == "reaped" then return "killed" end
  return "done"
end

local function leaf_status(members, run)
  local running, failed, killed, settled = false, false, false, false
  for _, member in ipairs(members) do
    local status = member.node.status
    if LIVE[status] then running = true
    elseif status == "failed" or status == "error" then failed = true
    elseif status == "killed" then killed = true
    elseif status == "done" or status == "skipped"
        or (status == "idle" and (member.node.started_at_ms ~= nil
          or member.node.settled_at_ms ~= nil)) then settled = true end
  end
  if running then return "running" end
  if failed then return "failed" end
  if killed then return "killed" end
  if settled then return "done" end
  if #members == 0 then return empty_leaf_status(run) end
  return "pending"
end

local function composite_status(children)
  local flags = {}
  for _, child in ipairs(children) do flags[child.status] = true end
  if flags.running then return "running" end
  if flags.pending then return "pending" end
  if flags.failed then return "failed" end
  if flags.killed then return "killed" end
  return "done"
end

local function linearize(children, run)
  if #children < 2 then return children end
  local owner, edges = {}, {}
  for index, child in ipairs(children) do
    edges[index] = {}
    for _, member in ipairs(child.members or {}) do owner[member.id] = index end
  end
  local function endpoint_key(endpoint)
    if type(endpoint) ~= "table" or type(endpoint.value) ~= "table" then return nil end
    local kind = endpoint.constructor == "ActorEndpoint" and "actor"
        or endpoint.constructor == "JunctionEndpoint" and "junction" or nil
    if kind == nil or type(endpoint.value.id) ~= "string" then return nil end
    return kind .. ":" .. endpoint.value.id
  end
  local outgoing = {}
  for _, route in ipairs(run.routes or {}) do
    local from = endpoint_key(type(route.from) == "table" and route.from.endpoint or nil)
    local to = endpoint_key(type(route.to) == "table" and route.to.endpoint or nil)
    if from and to then
      outgoing[from] = outgoing[from] or {}
      outgoing[from][#outgoing[from] + 1] = to
    end
  end
  for actor_id, from in pairs(owner) do
    local frontier, seen = { "actor:" .. actor_id }, {}
    while #frontier > 0 do
      local current = table.remove(frontier, 1)
      if not seen[current] then
        seen[current] = true
        for _, destination in ipairs(outgoing[current] or {}) do
          local target_actor = destination:match("^actor:(.*)$")
          local to = target_actor and owner[target_actor] or nil
          if to and to ~= from then edges[from][to] = true end
          if destination:match("^junction:") then frontier[#frontier + 1] = destination end
        end
      end
    end
  end
  local remaining, placed, ordered = {}, {}, {}
  for index = 1, #children do remaining[index] = true end
  while #ordered < #children do
    local zero, reached
    for candidate = 1, #children do if remaining[candidate] then
      local incoming, from_placed = false, false
      for from = 1, #children do if edges[from][candidate] then
        if remaining[from] then incoming = true elseif placed[from] then from_placed = true end
      end end
      if not incoming and zero == nil then zero = candidate end
      if from_placed and reached == nil then reached = candidate end
    end end
    local selected = zero or reached
    if not selected then for index = 1, #children do if remaining[index] then selected = index; break end end end
    remaining[selected], placed[selected] = nil, true
    ordered[#ordered + 1] = children[selected]
  end
  return ordered
end

function M.build_nodes(run)
  local by_key, roots = {}, {}
  for index, descriptor in ipairs(descriptors_for(run)) do
    local path = descriptor.path or {}
    by_key[path_key(path)] = { name = path[#path], path = path, key = path_key(path),
      own_actors = descriptor.members or {}, children = {}, declaration_index = index }
  end
  for _, logical in pairs(by_key) do
    if #logical.path == 1 then roots[#roots + 1] = logical
    else local parent = by_key[path_key(parent_path(logical.path))]; if parent then parent.children[#parent.children + 1] = logical end end
  end
  local function order(left, right) return left.declaration_index < right.declaration_index end
  local function finish(logical)
    table.sort(logical.children, order)
    local members, seen = {}, {}
    for _, actor_id in ipairs(logical.own_actors) do
      local node = (run.nodes or {})[actor_id]
      if node and not seen[actor_id] then members[#members + 1], seen[actor_id] = { id = actor_id, node = node }, true end
    end
    for _, child in ipairs(logical.children) do
      finish(child)
      for _, member in ipairs(child.members) do if not seen[member.id] then members[#members + 1], seen[member.id] = member, true end end
    end
    logical.members, logical.children = members, linearize(logical.children, run)
    logical.status = #logical.children == 0 and leaf_status(members, run) or composite_status(logical.children)
    local activity = (run.group_activity or {})[logical.key] or {}
    logical.active_ms, logical.active_since_ms = activity.active_ms or 0, activity.active_since_ms
    for _, member in ipairs(members) do
      local started = member.node.activation_started_at_ms or member.node.started_at_ms or member.node.started_at
      if started and (not logical.first_start or started < logical.first_start) then logical.first_start = started end
      local finished = member.node.settled_at_ms or member.node.completed_at or member.node.finished_at_ms
      if finished and (not logical.last_finish or finished > logical.last_finish) then logical.last_finish = finished end
    end
  end
  table.sort(roots, order)
  for _, root in ipairs(roots) do finish(root) end
  return linearize(roots, run)
end

function M.group_elapsed_ms(group, now_ms)
  if #group.children == 0 and #group.members == 1 then
    local node = group.members[1].node
    local started = node.activation_started_at_ms or node.started_at_ms or node.started_at
    if group.status == "running" then return started and math.max(0, now_ms - started) or nil end
    local finished = node.settled_at_ms or node.completed_at or node.finished_at_ms
    if group.status == "done" and started and finished then return math.max(0, finished - started) end
  end
  if group.status == "running" then
    return group.active_ms + math.max(0, now_ms - (group.active_since_ms or now_ms))
  end
  if TERMINAL[group.status] and group.first_start then return group.active_ms end
  return nil
end

local GLYPH = { pending = "○", running = "●", done = "✓", failed = "✗", error = "✗", killed = "⊗" }
function M.format_tree(run, now_ms, terminal)
  local roots, lines, active = M.build_nodes(run), {}, run.active_at_terminal or {}
  local function append(node, depth, include_children)
    local elapsed = M.group_elapsed_ms(node, now_ms)
    local suffix = elapsed and string.format(" (%.1fs active)", elapsed / 1000) or ""
    lines[#lines + 1] = string.rep("  ", depth) .. (GLYPH[node.status] or "·") .. " " .. tostring(node.name) .. suffix
    if include_children then for _, child in ipairs(node.children) do
      if not terminal or active[child.key] then append(child, depth + 1, true) end
    end end
  end
  for _, root in ipairs(roots) do append(root, 0, terminal and run.status ~= "completed") end
  return #lines > 0 and table.concat(lines, "\n") or "(empty workflow)"
end

return M
