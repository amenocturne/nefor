local controls = require("libs.chat.exit_controls")

local function eq(actual, expected, message)
  if actual ~= expected then
    error((message or "values differ") .. ": expected " .. tostring(expected)
      .. ", got " .. tostring(actual), 2)
  end
end

local function state(extra)
  local out = { toasts = {}, exit_token_seq = 0 }
  for key, value in pairs(extra or {}) do out[key] = value end
  return out
end

local armed, decisions = controls.press(state(), 100)
eq(#decisions, 1)
eq(decisions[1].kind, "schedule_exit_timeout")
eq(decisions[1].delay_ms, controls.DOUBLE_PRESS_DELAY_MS)
eq(armed.exit_token, 1)
eq(armed.last_ctrl_c_ms, 100)
eq(armed.toasts[1].text, "Press Ctrl+C again to exit")

local exited, exit_decisions = controls.press(armed, 650)
eq(exit_decisions[1].kind, "exit")
eq(exited.exit_token, nil)
eq(#exited.toasts, 0)

local stale = controls.timeout(armed, 999)
eq(stale, armed, "stale timeout preserves state identity")

local timed_out = controls.timeout(armed, 1)
eq(timed_out.exit_token, nil)
eq(timed_out.last_ctrl_c_ms, nil)
eq(#timed_out.toasts, 0)

local reset = controls.reset(armed)
eq(reset.exit_token, nil)
eq(reset.last_ctrl_c_ms, nil)
eq(#reset.toasts, 0)

local rearmed, rearm_decisions = controls.press(reset, 200)
eq(rearm_decisions[1].kind, "schedule_exit_timeout")
eq(rearmed.exit_token, 2, "an intervening action makes the next Ctrl+C a fresh first press")

local late, late_decisions = controls.press(armed, 701)
eq(late_decisions[1].kind, "schedule_exit_timeout")
eq(late.exit_token, 2, "a press after the deadline starts a fresh sequence")

print("exit_controls_test: all assertions passed")
