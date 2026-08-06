local M = {}

M.DOUBLE_PRESS_DELAY_MS = 600
M.WARNING_TTL_MS = 3000
M.WARNING_ID = "ctrl-c-exit-warning"
M.WARNING_TEXT = "Press Ctrl+C again to exit"

local function copy_table(source)
  local out = {}
  for key, value in pairs(source or {}) do out[key] = value end
  return out
end

local function without_warning(toasts)
  local out = {}
  for _, toast in ipairs(toasts or {}) do
    if toast.id ~= M.WARNING_ID then out[#out + 1] = toast end
  end
  return out
end

local function has_warning(toasts)
  for _, toast in ipairs(toasts or {}) do
    if toast.id == M.WARNING_ID then return true end
  end
  return false
end

function M.reset(state)
  if state.exit_token == nil and not has_warning(state.toasts) then return state end
  local next_state = copy_table(state)
  next_state.exit_token = nil
  next_state.last_ctrl_c_ms = nil
  next_state.toasts = without_warning(state.toasts)
  return next_state
end

function M.press(state, now_ms)
  if state.exit_token ~= nil and state.last_ctrl_c_ms ~= nil
      and now_ms - state.last_ctrl_c_ms <= M.DOUBLE_PRESS_DELAY_MS then
    local next_state = M.reset(state)
    next_state.last_ctrl_c_ms = nil
    return next_state, { { kind = "exit" } }
  end

  local next_state = copy_table(state)
  local token = (state.exit_token_seq or 0) + 1
  local toasts = without_warning(state.toasts)
  toasts[#toasts + 1] = {
    id = M.WARNING_ID,
    text = M.WARNING_TEXT,
    level = "warn",
    started_at_ms = now_ms,
    ttl_ms = M.WARNING_TTL_MS,
  }
  next_state.exit_token = token
  next_state.exit_token_seq = token
  next_state.last_ctrl_c_ms = now_ms
  next_state.toasts = toasts
  return next_state, {
    { kind = "schedule_exit_timeout", token = token, delay_ms = M.DOUBLE_PRESS_DELAY_MS },
  }
end

function M.timeout(state, token)
  if token ~= state.exit_token then return state end
  local next_state = copy_table(state)
  next_state.exit_token = nil
  next_state.last_ctrl_c_ms = nil
  return next_state
end

return M
