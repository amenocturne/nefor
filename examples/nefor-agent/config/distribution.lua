-- Distribution-owned runtime paths. Installers may replace `installed` with
-- immutable absolute paths; environment variables are explicit launcher/test
-- overrides, not engine-discovered state.
local installed = {
  runtime_root = nil,
  executable_root = nil,
}

local M = {}

local function nonempty(value)
  return value and value ~= "" and value or nil
end

local function exists(path)
  if nefor and nefor.fs and nefor.fs.exists then return nefor.fs.exists(path) end
  local file = io.open(path, "r")
  if file then file:close(); return true end
  return false
end

local function require_file(label, path)
  if not exists(path) then
    error("nefor distribution: missing " .. label .. " at " .. path)
  end
  return path
end

local home = os.getenv("HOME")
local default_data = nonempty(os.getenv("NEFOR_DATA_DIR"))
  or (nonempty(os.getenv("XDG_DATA_HOME")) and (os.getenv("XDG_DATA_HOME") .. "/nefor"))
  or (home and (home .. "/.local/share/nefor"))

function M.runtime_root()
  local dev = nonempty(os.getenv("NEFOR_DEV_DIR"))
  local root = dev or nonempty(os.getenv("NEFOR_RUNTIME_ROOT")) or installed.runtime_root
    or (default_data and (default_data .. "/runtime"))
  if not root then
    error("nefor distribution: no immutable runtime root configured; reinstall the starter or set NEFOR_RUNTIME_ROOT (NEFOR_DEV_DIR is development-only)")
  end
  require_file("runtime marker lua/nefor-pm/init.lua", root .. "/lua/nefor-pm/init.lua")
  return root
end

function M.executable_root()
  local dev = nonempty(os.getenv("NEFOR_DEV_DIR"))
  local root = nonempty(os.getenv("NEFOR_EXECUTABLE_ROOT"))
    or (dev and (dev .. "/target/debug"))
    or installed.executable_root
    or (default_data and (default_data .. "/bin"))
  if not root then
    error("nefor distribution: no plugin executable root configured; reinstall the starter or set NEFOR_EXECUTABLE_ROOT")
  end
  return root
end

function M.binary(name)
  return require_file("plugin executable " .. name, M.executable_root() .. "/" .. name)
end

return M
