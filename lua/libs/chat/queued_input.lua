local Entry = require("libs.chat.entry")

local M = {}

local function copy_table(source)
  local out = {}
  for key, value in pairs(source or {}) do out[key] = value end
  return out
end

local function copy_entries(entries)
  local out = {}
  for idx, entry in ipairs(entries or {}) do out[idx] = entry end
  return out
end

local function push_entry(state, entry)
  local entries = copy_entries(state.entries)
  entries[#entries + 1] = entry
  local next_state = copy_table(state)
  next_state.entries = entries
  return next_state
end

local function owned_index(state, owner_id)
  if type(owner_id) ~= "string" then return nil end
  for idx, entry in ipairs(state.entries or {}) do
    if type(entry) == "table" and entry.local_id == owner_id then return idx end
  end
  return nil
end

local function join_with_one_space(left, right)
  left = type(left) == "string" and left or ""
  right = type(right) == "string" and right or ""
  if left == "" then return right end
  if right == "" then return left .. " " end
  return left .. " " .. right
end

function M.is_queued_entry(state, entry)
  return type(entry) == "table" and entry.local_id == state.queued_entry_id
end

function M.submit(state, text, busy)
  if busy then
    local idx = owned_index(state, state.queued_entry_id)
    local old = idx and state.entries[idx] or nil
    if old ~= nil then
      local entries = copy_entries(state.entries)
      entries[idx] = Entry.set_text(old, old.text .. "\n" .. text)
      local next_state = copy_table(state)
      next_state.entries = entries
      return next_state
    end

    local next_state = push_entry(state, Entry.user(text))
    next_state.queued_entry_id = next_state.entries[#next_state.entries].local_id
    return next_state
  end

  local next_state = push_entry(state, Entry.user(text))
  local entry = next_state.entries[#next_state.entries]
  next_state.pending_user_echo = text
  next_state.pending_user_echo_id = entry.local_id
  return next_state
end

-- The backend has promoted the queue into the next canonical turn. Reposition
-- its optimistic row at the causal boundary and transfer ownership to the
-- durable projection acknowledgement. The row remains visible throughout.
function M.accept_steered(state)
  local idx = owned_index(state, state.queued_entry_id)
  if idx == nil then return state, false end

  local owned = state.entries[idx]
  local entries = {}
  for entry_idx, entry in ipairs(state.entries or {}) do
    if entry_idx ~= idx then entries[#entries + 1] = entry end
  end
  entries[#entries + 1] = owned

  local next_state = copy_table(state)
  next_state.entries = entries
  next_state.queued_entry_id = nil
  next_state.pending_user_echo = owned.text
  next_state.pending_user_echo_id = owned.local_id
  if type(state.in_flight) == "number" then
    if state.in_flight > idx then
      next_state.in_flight = state.in_flight - 1
    elseif state.in_flight == idx then
      next_state.in_flight = nil
    end
  end
  return next_state, true
end

function M.observe_external_submit(state, text)
  if state.pending_user_echo ~= nil and state.pending_user_echo == text then
    return state, true
  end
  local next_state = push_entry(state, Entry.user(text))
  local entry = next_state.entries[#next_state.entries]
  next_state.pending_user_echo = text
  next_state.pending_user_echo_id = entry.local_id
  return next_state, true
end

function M.reconcile_echo(state, text)
  if state.pending_user_echo == nil or state.pending_user_echo ~= text then
    return state, false
  end

  local idx = owned_index(state, state.pending_user_echo_id)
  local owned = idx and state.entries[idx] or nil
  local next_state = copy_table(state)
  next_state.pending_user_echo = nil
  next_state.pending_user_echo_id = nil
  if owned and owned.role == "user" and owned.text == text then
    return next_state, true
  end
  return push_entry(next_state, Entry.user(text)), true
end

function M.restore(state)
  local idx = owned_index(state, state.queued_entry_id)
  if idx == nil then return copy_table(state), false end

  local queued = state.entries[idx]
  local entries = {}
  for entry_idx, entry in ipairs(state.entries or {}) do
    if entry_idx ~= idx then entries[#entries + 1] = entry end
  end
  local next_state = copy_table(state)
  next_state.entries = entries
  next_state.queued_entry_id = nil
  next_state.pending_user_echo = nil
  next_state.pending_user_echo_id = nil
  if type(state.in_flight) == "number" and state.in_flight > idx then
    next_state.in_flight = state.in_flight - 1
  end
  next_state.input_value = join_with_one_space(queued.text, state.input_value)
  return next_state, true
end

return M
