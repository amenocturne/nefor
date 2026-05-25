local M = {}

local nefor_debug = os.getenv("NEFOR_DEBUG") or ""
local enabled = nefor_debug ~= ""
local log_fh = nil

local function resolve_log_path()
  local dir
  if nefor_debug == "1" or nefor_debug:lower() == "true" or nefor_debug == "" then
    local data = os.getenv("NEFOR_DATA_DIR")
      or ((os.getenv("XDG_DATA_HOME") or "") ~= "" and os.getenv("XDG_DATA_HOME") .. "/nefor")
      or ((os.getenv("HOME") or "") ~= "" and os.getenv("HOME") .. "/.local/share/nefor")
    if data then dir = data .. "/debug" end
  else
    dir = nefor_debug
  end
  if not dir then return "/tmp/nefor-chat-debug.log" end
  os.execute(string.format("mkdir -p %q 2>/dev/null", dir))
  return dir .. "/nefor-chat.log"
end

local function get_fh()
  if log_fh then return log_fh end
  log_fh = io.open(resolve_log_path(), "w")
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
