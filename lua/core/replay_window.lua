-- Process-wide session replay gate.
--
-- Sessions toggle this synchronously around replay delivery so consumers can
-- suppress live-only side effects while every persisted envelope still fans
-- out through the normal bus. The subscription is a defense-in-depth path for
-- replay windows opened by something other than sessions.

local M = {}
local active = false

---@return boolean
function M.active()
  return active
end

---@param flag boolean
function M.set(flag)
  active = flag and true or false
end

function M.install()
  if nefor.bus and nefor.bus.on_event then
    nefor.bus.on_event("sessions.replay.start", function()
      active = true
    end)
    nefor.bus.on_event("sessions.replay.end", function()
      active = false
    end)
  end
end

return M
