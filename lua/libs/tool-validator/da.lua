-- da package definition for compositions that use shell-script classification.
-- The composition owns the source pin and invokes nefor-pm; this module owns
-- only da's source-specific build prerequisites and package layout.

local M = {}

local DEFAULT_URL = "https://github.com/amenocturne/da.git"
local MODEL_PATH = "classifier/model.onnx"
local LFS_POINTER_PREFIX = "version https://git-lfs.github.com/spec/v1"

local function run_or_error(label, opts)
  local result = nefor.process.run(opts)
  if type(result) ~= "table" or result.code ~= 0 then
    local detail = type(result) == "table" and (result.stderr or result.stdout) or nil
    error(label .. " failed" .. (detail and detail ~= "" and ": " .. detail or ""), 0)
  end
end

local function model_state(root)
  local path = root .. "/" .. MODEL_PATH
  local file, open_error = io.open(path, "rb")
  if not file then return nil, "cannot read " .. path .. ": " .. tostring(open_error) end
  local prefix = file:read(#LFS_POINTER_PREFIX) or ""
  local size = file:seek("end")
  file:close()
  if prefix == LFS_POINTER_PREFIX then
    return "pointer", path
  end
  if not size or size == 0 then return nil, "classifier model is empty at " .. path end
  return "materialized", path
end

local function build(plugin)
  local state, detail = model_state(plugin.dir)
  if state == nil then error("da package: " .. detail, 0) end
  if state == "pointer" then
    local result = nefor.process.run {
      cmd = "git",
      args = { "lfs", "pull", "--include", MODEL_PATH },
      cwd = plugin.dir,
    }
    if type(result) ~= "table" or result.code ~= 0 then
      local output = type(result) == "table" and (result.stderr or result.stdout) or ""
      error("da package: the embedded classifier model is a Git LFS pointer; " ..
        "install Git LFS and retry so `git lfs pull --include " .. MODEL_PATH ..
        "` can materialize it" .. (output ~= "" and ": " .. output or ""), 0)
    end
    local after, after_detail = model_state(plugin.dir)
    if after ~= "materialized" then
      error("da package: Git LFS did not materialize " .. tostring(after_detail), 0)
    end
  end

  run_or_error("da package: cargo build", {
    cmd = "cargo",
    args = {
      "install", "--locked", "--force",
      "--path", ".",
      "--root", ".",
    },
    cwd = plugin.dir,
  })
end

function M.package(opts)
  opts = opts or {}
  if type(opts.commit) ~= "string" or opts.commit == "" then
    error("libs.tool-validator.da.package: `commit` is required", 0)
  end
  local name = opts.name or "da"
  local url = opts.url or DEFAULT_URL
  if type(name) ~= "string" or name == "" then
    error("libs.tool-validator.da.package: `name` must be a non-empty string", 0)
  end
  if type(url) ~= "string" or url == "" then
    error("libs.tool-validator.da.package: `url` must be a non-empty string", 0)
  end
  return {
    name = name,
    url = url,
    commit = opts.commit,
    build = build,
  }
end

M._internals = {
  build = build,
  model_state = model_state,
}

return M
