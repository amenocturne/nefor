-- mag/init.lua — MAG workspace management and preview formatting.
--
-- Provides two things:
--   1. Workspace lifecycle: init a per-session MAG workspace seeded
--      from the config library (mag/lib/).
--   2. Preview formatting: render a graph modification (the shape the
--      mag plugin replies with on `mag.loaded`) into a human-readable
--      string the lead can inspect before executing.
--
-- Compilation itself lives in the mag plugin: the lead emits `mag.load`
-- and reads the modification off the `mag.loaded` reply
-- (starter/lead-workflow/init.lua). The `mag` CLI binary remains a dev
-- tool for humans; nothing here shells out to it.

local M = {}

local function sh_quote(value)
  return "'" .. tostring(value):gsub("'", "'\\''") .. "'"
end

local function data_root()
  if nefor and nefor.fs and type(nefor.fs.data_root) == "function" then
    local ok, root = pcall(nefor.fs.data_root)
    if ok and type(root) == "string" and root ~= "" then return root end
  end
  local override = os.getenv("NEFOR_DATA_DIR")
  if override ~= nil and override ~= "" then return override end
  local xdg = os.getenv("XDG_DATA_HOME")
  if xdg ~= nil and xdg ~= "" then return xdg .. "/nefor" end
  local home = os.getenv("HOME") or ""
  if home == "" then return nil end
  return home .. "/.local/share/nefor"
end

local function mkdir_p(path)
  if nefor and nefor.fs and type(nefor.fs.mkdir_p) == "function" then
    local ok = pcall(nefor.fs.mkdir_p, path)
    if ok then return true end
  end
  local ok = os.execute("mkdir -p " .. sh_quote(path) .. " >/dev/null 2>&1")
  return ok == true or ok == 0
end

-- Get the MAG workspace directory for a session.
function M.workspace_dir(session_id)
  local root = data_root()
  if not root then return nil end
  return root .. "/sessions/" .. session_id .. "/mag"
end

-- Initialize workspace: create dir, seed from config library.
-- Returns the workspace path on success, nil + error on failure.
function M.init_workspace(session_id, config_dir)
  local ws = M.workspace_dir(session_id)
  if not ws then return nil, "no data root available" end

  if not mkdir_p(ws .. "/lib/prompts") then
    return nil, "failed to create workspace: " .. ws
  end

  -- Seed from config mag/lib/ contents. -n = no-clobber.
  local config_mag = config_dir .. "/mag/lib"
  os.execute("cp -Rn " .. sh_quote(config_mag) .. "/. " .. sh_quote(ws) .. "/lib/ 2>/dev/null")

  return ws, nil
end

-- Render one param value compactly: quoted strings (truncated), inline
-- arrays, `{…}` for nested maps.
local MAX_STR = 48

local function format_value(value)
  local t = type(value)
  if t == "string" then
    local s = value
    if #s > MAX_STR then s = s:sub(1, MAX_STR - 1) .. "…" end
    return string.format("%q", s)
  end
  if t == "table" then
    -- Array: render inline. Map: elide (params summaries stay one line).
    if #value > 0 or next(value) == nil then
      local parts = {}
      for _, v in ipairs(value) do parts[#parts + 1] = tostring(v) end
      return "[" .. table.concat(parts, ", ") .. "]"
    end
    return "{…}"
  end
  return tostring(value)
end

local function format_params(params)
  if type(params) ~= "table" or next(params) == nil then return "" end
  local keys = {}
  for k in pairs(params) do keys[#keys + 1] = tostring(k) end
  table.sort(keys)
  local parts = {}
  for _, k in ipairs(keys) do
    parts[#parts + 1] = k .. ": " .. format_value(params[k])
  end
  return " {" .. table.concat(parts, ", ") .. "}"
end

local function format_routes(routes)
  if type(routes) ~= "table" or next(routes) == nil then return nil end
  local keys = {}
  for k in pairs(routes) do keys[#keys + 1] = tostring(k) end
  table.sort(keys)
  local parts = {}
  for _, ty in ipairs(keys) do
    local dests = routes[ty]
    local names = {}
    if type(dests) == "table" then
      for _, d in ipairs(dests) do names[#names + 1] = tostring(d) end
    end
    parts[#parts + 1] = ty .. " -> " .. table.concat(names, ", ")
  end
  return table.concat(parts, "; ")
end

-- Format a graph modification (`mag.loaded` reply shape) into a
-- human-readable preview: actors with factory + params summary, their
-- typed routes, the initial messages, the hash, and the kernel
-- registry's factory names.
function M.preview(modification, hash, factories)
  if type(modification) ~= "table" then return "(invalid modification)" end
  local actors = modification.actors or {}
  local messages = modification.messages or {}

  local lines = {}
  lines[#lines + 1] = string.format("Modification: %d actors, %d initial messages",
    #actors, #messages)
  lines[#lines + 1] = "Hash: " .. tostring(hash or modification.hash)
  lines[#lines + 1] = ""

  lines[#lines + 1] = "Actors:"
  for _, actor in ipairs(actors) do
    lines[#lines + 1] = string.format("  %s (%s)%s",
      tostring(actor.id), tostring(actor.factory), format_params(actor.params))
    local routes = format_routes(actor.routes)
    if routes then
      lines[#lines + 1] = "    routes: " .. routes
    end
  end

  if #messages > 0 then
    lines[#lines + 1] = ""
    lines[#lines + 1] = "Initial messages:"
    for _, msg in ipairs(messages) do
      local kind = type(msg.content) == "table" and msg.content.kind or nil
      lines[#lines + 1] = string.format("  -> %s (%s)",
        tostring(msg.to), tostring(kind or "message"))
    end
  end

  if type(factories) == "table" and #factories > 0 then
    local names = {}
    for _, f in ipairs(factories) do names[#names + 1] = tostring(f) end
    table.sort(names)
    lines[#lines + 1] = ""
    lines[#lines + 1] = "Registry factories: " .. table.concat(names, ", ")
  end

  return table.concat(lines, "\n")
end

return M
