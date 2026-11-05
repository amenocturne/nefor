-- Keep the mutex inode separate from the files and directories being replaced.
-- Never unlink it: waiters must continue locking the same inode after release.
local M = {}
local active = {}

function M.with(path, operation)
  if active[path] then
    error("nefor-pm: recursive reconciliation of " .. path, 0)
  end
  local fcntl = require("posix.fcntl")
  local stdio = require("posix.stdio")
  local errno = require("posix.errno")
  local file, open_error = io.open(path, "a+")
  if not file then error("nefor-pm: cannot open operation lock: " .. tostring(open_error), 0) end
  active[path] = true
  local ok, result = pcall(function()
    local fd, descriptor_error = stdio.fileno(file)
    if not fd then error("nefor-pm: cannot resolve lock descriptor: " .. tostring(descriptor_error), 0) end
    local configured, flag_error = fcntl.fcntl(fd, fcntl.F_SETFD, fcntl.FD_CLOEXEC)
    if not configured then error("nefor-pm: cannot configure operation lock: " .. tostring(flag_error), 0) end
    while true do
      local locked, lock_error, code = fcntl.fcntl(fd, fcntl.F_SETLKW, {
        l_type = fcntl.F_WRLCK, l_whence = 0, l_start = 0, l_len = 0,
      })
      if locked then break end
      if code ~= errno.EINTR then
        error("nefor-pm: cannot acquire operation lock: " .. tostring(lock_error), 0)
      end
    end
    return operation()
  end)
  local closed, close_error = file:close()
  active[path] = nil
  if not closed then
    local message = "nefor-pm: cannot close operation lock: " .. tostring(close_error)
    if not ok then message = tostring(result) .. "\n" .. message end
    error(message, 0)
  end
  if not ok then error(result, 0) end
  return result
end

return M
