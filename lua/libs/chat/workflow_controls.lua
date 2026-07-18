local M = {}
local queued_input = require("libs.chat.queued_input")

M.ESCAPE_DELAY_MS = 600

local function copy_table(source)
  local out = {}
  for key, value in pairs(source or {}) do out[key] = value end
  return out
end

local function clear_escape(state)
  local next_state = copy_table(state)
  next_state.last_esc_ms = nil
  next_state.escape_token = nil
  return next_state
end

function M.hard_stop_lead(state)
  local next_state, restored_queue = queued_input.restore(state)
  next_state = clear_escape(next_state)
  return next_state, { { kind = "hard_stop_lead" } }, { restored_queue = restored_queue }
end

function M.escape(state, now_ms)
  if state.escape_token ~= nil and state.last_esc_ms ~= nil
      and now_ms - state.last_esc_ms <= M.ESCAPE_DELAY_MS then
    return M.hard_stop_lead(state)
  end

  local next_state = copy_table(state)
  local token = (state.escape_token_seq or 0) + 1
  next_state.last_esc_ms = now_ms
  next_state.escape_token = token
  next_state.escape_token_seq = token
  return next_state, {
    { kind = "schedule_escape_timeout", token = token, delay_ms = M.ESCAPE_DELAY_MS },
  }, { restored_queue = false }
end

function M.escape_timeout(state, token)
  if token ~= state.escape_token then
    return state, {}, { restored_queue = false }
  end

  local next_state = clear_escape(state)
  if state.queued_entry_idx ~= nil then
    return next_state, { { kind = "steer_queued" } }, { restored_queue = false }
  end
  return next_state, {}, { restored_queue = false }
end

local function active(run)
  return type(run) == "table" and run.completed_at_ms == nil
end

local function active_lead(state)
  if state.pending == true or state.in_flight ~= nil then return true end
  for _, run in pairs(state.runs or {}) do
    if active(run) and run.principal == "lead" then return true end
  end
  return false
end

function M.confirm_termination(state, request)
  if type(request) ~= "table" then return copy_table(state), {}, { restored_queue = false } end

  if request.scope == "one" then
    local run = (state.runs or {})[request.run_id]
    if not active(run) then return copy_table(state), {}, { restored_queue = false } end
    if run.principal == "lead" then return M.hard_stop_lead(state) end
    return copy_table(state), {
      { kind = "terminate_workflow", run_id = request.run_id },
    }, { restored_queue = false }
  end

  if request.scope ~= "all" then return copy_table(state), {}, { restored_queue = false } end

  local next_state = copy_table(state)
  local decisions = {}
  local metadata = { restored_queue = false }
  if active_lead(state) then
    next_state, decisions, metadata = M.hard_stop_lead(next_state)
  end
  local has_active_non_lead = false
  for _, run in pairs(state.runs or {}) do
    if active(run) and run.principal ~= "lead" then
      has_active_non_lead = true
      break
    end
  end
  if has_active_non_lead then
    decisions[#decisions + 1] = { kind = "terminate_all_non_lead_workflows" }
  end
  return next_state, decisions, metadata
end

return M
