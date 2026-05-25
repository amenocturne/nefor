local M = {}
local enabled = os.getenv("NEFOR_CHAT_DEBUG") == "1"

function M.is_enabled() return enabled end
function M.enable()  enabled = true end
function M.disable() enabled = false end

function M.log(category, fmt, ...)
  if not enabled then return end
  local msg = string.format("[chat:%s] " .. fmt, category, ...)
  if type(tui) == "table" and type(tui.log) == "function" then
    tui.log(msg)
  else
    io.stderr:write(msg .. "\n")
  end
end

return M
