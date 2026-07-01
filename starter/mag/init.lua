-- mag/init.lua — MAG workspace management and compiler bridge.
--
-- Provides three things:
--   1. Workspace lifecycle: init a per-session MAG workspace seeded
--      from the config library (mag/lib/).
--   2. Compiler bridge: shell out to the `mag` binary, parse the
--      resulting graph IR JSON.
--   3. Preview formatting: render IR into a human-readable string
--      the lead can inspect before executing.

local json = nefor.json

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

-- Compile a .mag file and return the parsed IR or nil + error.
function M.compile(file_path, source_dir)
  local root = data_root()
  if not root then return nil, "no data root available" end
  local mag_bin = root .. "/bin/mag"

  local cmd = mag_bin .. " " .. sh_quote(file_path) .. " --source-dir " .. sh_quote(source_dir) .. " 2>&1"
  local pipe = io.popen(cmd, "r")
  if not pipe then
    return nil, "failed to run mag compiler"
  end
  local output = pipe:read("*a") or ""
  local ok, _, code = pipe:close()

  if not ok or code ~= 0 then
    return nil, output  -- compiler error message
  end

  local decode_ok, ir = pcall(json.decode, output)
  if not decode_ok or type(ir) ~= "table" then
    return nil, "failed to parse compiler output: " .. tostring(ir or output)
  end

  return ir, nil
end

-- Format IR into a human-readable preview string.
function M.preview(ir)
  if type(ir) ~= "table" then return "(invalid IR)" end
  local nodes = ir.nodes or {}
  local edges = ir.edges or {}

  local lines = {}
  lines[#lines + 1] = string.format("Graph: %d nodes, %d edges", #nodes, #edges)
  lines[#lines + 1] = string.format("Terminal: %s", tostring(ir.terminal))
  lines[#lines + 1] = string.format("Hash: %s", tostring(ir.hash))
  lines[#lines + 1] = ""

  lines[#lines + 1] = "Nodes:"
  for _, node in ipairs(nodes) do
    local fanout_str = ""
    if type(node.fanout) == "table" then
      local out_ids = type(node.fanout.out) == "table" and table.concat(node.fanout.out, ", ") or "?"
      fanout_str = string.format(" [fanout: %s -> %s]", tostring(node.fanout["in"]), out_ids)
    end
    lines[#lines + 1] = string.format("  %s (%s)%s", tostring(node.id), tostring(node.reasoner), fanout_str)
  end

  lines[#lines + 1] = ""
  lines[#lines + 1] = "Edges:"
  for _, edge in ipairs(edges) do
    local type_str = edge.type and (" [" .. edge.type .. "]") or ""
    lines[#lines + 1] = string.format("  %s -> %s%s", tostring(edge.from), tostring(edge.to), type_str)
  end

  return table.concat(lines, "\n")
end

return M
