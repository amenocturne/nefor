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
-- The flag flips on `sessions.replay.start` / `sessions.replay.end`
-- envelopes seen on the bus. We subscribe via `nefor.bus.on_event` so
-- the toggle is independent of any wrapper's own dispatch path. The
-- guard against missing `nefor.bus` keeps the module load-safe in test
-- harnesses that don't install the bus binding.

local M = {}

local in_replay = false

function M.active() return in_replay end

-- Test escape hatch.
function M._set(flag) in_replay = flag and true or false end

if nefor.bus and nefor.bus.on_event then
  nefor.bus.on_event("sessions.replay.start", function(_entry)
    in_replay = true
  end)
  nefor.bus.on_event("sessions.replay.end", function(_entry)
    in_replay = false
  end)
end

return M
