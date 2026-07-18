local Entry = require("libs.chat.entry")

local M = {}

local function copy_table(source)
  local out = {}
  for key, value in pairs(source or {}) do out[key] = value end
  return out
end

local function push_entry(state, entry)
  local entries = {}
  for idx, current in ipairs(state.entries or {}) do entries[idx] = current end
  entries[#entries + 1] = entry
  local next_state = copy_table(state)
  next_state.entries = entries
  return next_state
end

local function join_with_one_space(left, right)
  left = type(left) == "string" and left or ""
  right = type(right) == "string" and right or ""
  if left == "" then return right end
  if right == "" then return left .. " " end
  return left .. " " .. right
end

function M.submit(state, text, busy)
  if busy then
    if type(state.queued_entry_idx) == "number" then
      local old = state.entries and state.entries[state.queued_entry_idx]
      if old ~= nil then
        local entries = {}
        local combined = Entry.set_text(old, old.text .. "\n" .. text)
        for idx, entry in ipairs(state.entries or {}) do
          entries[idx] = idx == state.queued_entry_idx and combined or entry
        end
        local next_state = copy_table(state)
        next_state.entries = entries
        return next_state
      end
    end

    local next_state = push_entry(state, Entry.user(text))
    next_state.queued_entry_idx = #next_state.entries
    return next_state
  end

  local next_state = push_entry(state, Entry.user(text))
  next_state.pending_user_echo = text
  next_state.pending_user_echo_idx = #next_state.entries
  return next_state
end

function M.accept_steered(state)
  local idx = state.queued_entry_idx
  if type(idx) ~= "number" then return state, false end

  local entries = {}
  for entry_idx, entry in ipairs(state.entries or {}) do
    if entry_idx ~= idx then entries[#entries + 1] = entry end
  end
  local next_state = copy_table(state)
  next_state.entries = entries
  next_state.queued_entry_idx = nil
  next_state.pending_user_echo = nil
  next_state.pending_user_echo_idx = nil
  if type(state.in_flight) == "number" and state.in_flight > idx then
    next_state.in_flight = state.in_flight - 1
  end
  return next_state, true
end

function M.reconcile_echo(state, text)
  if state.pending_user_echo == nil or state.pending_user_echo ~= text then
    return state, false
  end

  local idx = state.pending_user_echo_idx
  local owned = type(idx) == "number" and state.entries and state.entries[idx] or nil
  local next_state = copy_table(state)
  next_state.pending_user_echo = nil
  next_state.pending_user_echo_idx = nil
  if owned and owned.role == "user" and owned.text == text then
    return next_state, true
  end
  return push_entry(next_state, Entry.user(text)), true
end

function M.restore(state)
  local idx = state.queued_entry_idx
  if type(idx) ~= "number" then return copy_table(state), false end

  local queued = state.entries and state.entries[idx]
  local next_state, accepted = M.accept_steered(state)
  if not accepted then return next_state, false end
  next_state.input_value = join_with_one_space(queued and queued.text, state.input_value)
  return next_state, true
end

return M
