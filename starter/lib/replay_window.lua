-- starter/lib/replay_window.lua — per-wrapper replay-window filter.
--
-- The framework no longer suppresses bus→peer delivery globally during
-- a session replay (sessions emits `sessions.replay.start` /
-- `sessions.replay.end` framing). Each wrapper that wants to skip
-- envelopes during the window adds the check itself, typically at the
-- top of its `to_plugin` callback:
--
--   local replay = require("lib.replay_window")
--   to_plugin = function(env)
--     if replay.active() then return end
--     ...
--   end
--
-- ## Toggling
--
-- The flag is flipped two ways on purpose:
--
--   1. `sessions` calls `set(true)` synchronously before emitting
--      replayed envelopes and `set(false)` after the burst (see
--      `do_resume` in starter/sessions/init.lua). This is the load-
--      bearing path: `drain_pending_dispatch` runs the whole batch's
--      `to_plugin` callbacks BEFORE any `dispatch_subscriptions`
--      handler fires, so a bus.on_event subscriber alone would always
--      see the flag flip TOO LATE for the wrappers' to_plugin to skip
--      the replayed envelopes (Bug 5 root cause).
--   2. A `nefor.bus.on_event` subscriber on the framing markers as a
--      defense-in-depth fallback for any non-sessions emitter that
--      might bracket a replay window. In normal operation this fires
--      after sessions has already toggled the flag and is a no-op.
--
-- The guard against missing `nefor.bus` keeps the module load-safe in
-- test harnesses that don't install the bus binding.

local M = {}

local in_replay = false

function M.active() return in_replay end

-- Synchronously set the replay-window flag. Used by `sessions` to
-- bracket the replay-envelope burst and by tests as an escape hatch.
function M.set(flag) in_replay = flag and true or false end

-- Backwards-compat alias for the prior test escape hatch name.
M._set = M.set

if nefor.bus and nefor.bus.on_event then
  nefor.bus.on_event("sessions.replay.start", function(_entry)
    in_replay = true
  end)
  nefor.bus.on_event("sessions.replay.end", function(_entry)
    in_replay = false
  end)
end

return M
