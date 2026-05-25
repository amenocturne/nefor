local M = {}
local enabled = os.getenv("NEFOR_CHAT_DEBUG") == "1"
local log_path = os.getenv("NEFOR_CHAT_DEBUG_LOG") or "/tmp/nefor-chat-debug.log"
local log_fh = nil

local function get_fh()
  if log_fh then return log_fh end
  log_fh = io.open(log_path, "w")
  return log_fh
end

function M.is_enabled() return enabled end
function M.enable()  enabled = true end
function M.disable() enabled = false end

function M.log(category, fmt, ...)
  if not enabled then return end
  local fh = get_fh()
  if not fh then return end
  fh:write(string.format("[chat:%s] " .. fmt .. "\n", category, ...))
  fh:flush()
end

return M
