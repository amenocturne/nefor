-- Shared per-node output persistence for MAG graph reasoners.
--
-- Writes successful node outputs under the active MAG workspace:
--   <data-root>/sessions/<session-id>/mag/runs/<run-name>/<node-id>.output
--
-- The library is intentionally side-effect free until persist() is called.
-- Callers own policy: which reasoner completions count as successful and
-- which result shape should be written.

local json = nefor.json

local M = {}

local function sh_quote(value)
  return "'" .. tostring(value):gsub("'", "'\\''") .. "'"
end

local function safe_segment(value, fallback)
  local s = tostring(value or "")
  s = s:gsub("[^%w%._%-]+", "-"):gsub("^-+", ""):gsub("-+$", "")
  if s == "" then return fallback end
  return s
end

local function data_root()
  if nefor and nefor.fs and type(nefor.fs.data_root) == "function" then
    local ok, root = pcall(nefor.fs.data_root)
    if ok and type(root) == "string" and root ~= "" then return root end
  end
  local override = os.getenv("NEFOR_DATA_DIR")
  if override ~= nil and override ~= "" then return override end
  return nil
end

local function current_session_id()
  local ok, sessions = pcall(require, "sessions")
  if not ok or type(sessions) ~= "table" or type(sessions.current_id) ~= "function" then
    return nil
  end
  local ok_id, id = pcall(sessions.current_id)
  if ok_id and type(id) == "string" and id ~= "" then return id end
  return nil
end

local function mkdir_p(path)
  if nefor and nefor.fs and type(nefor.fs.mkdir_p) == "function" then
    local ok, result = pcall(nefor.fs.mkdir_p, path)
    if ok and type(result) == "table" and result.ok == true then return true end
  end
  local ok = os.execute("mkdir -p " .. sh_quote(path) .. " >/dev/null 2>&1")
  return ok == true or ok == 0
end

local function output_text(output)
  if type(output) == "table" and type(output.text) == "string" then return output.text end
  if type(output) == "string" then return output end
  local ok, encoded = pcall(json.encode, output)
  if ok and type(encoded) == "string" then return encoded end
  return tostring(output)
end

local function copy_table(t)
  local out = {}
  for k, v in pairs(t) do out[k] = v end
  return out
end

function M.persist(body, output)
  if type(body) ~= "table" or type(output) ~= "table" then return output end
  local run_id = body.run_id
  local node_id = body.node_id
  if type(run_id) ~= "string" or run_id == "" then return output end
  if type(node_id) ~= "string" or node_id == "" then return output end

  local root = data_root()
  local session_id = current_session_id()
  if type(root) ~= "string" or type(session_id) ~= "string" then return output end

  local run_segment = safe_segment(body.run_name or run_id, safe_segment(run_id, "run"))
  local node_segment = safe_segment(node_id, "node")
  local relpath = "runs/" .. run_segment .. "/" .. node_segment .. ".output"
  local dir = root .. "/sessions/" .. session_id .. "/mag/runs/" .. run_segment
  local path = root .. "/sessions/" .. session_id .. "/mag/" .. relpath

  if not mkdir_p(dir) then return output end
  local fh = io.open(path, "w")
  if not fh then return output end
  fh:write(output_text(output))
  fh:close()

  local persisted = copy_table(output)
  persisted.output_path = path
  persisted.output_relpath = relpath
  return persisted
end

return M
