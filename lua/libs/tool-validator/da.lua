-- da package definition for compositions that use shell-script classification.
-- The composition owns source/release pins and invokes nefor-pm; this module
-- describes da's supported release layout and explicit source-build option.

local M = {}

local DEFAULT_URL = "https://github.com/amenocturne/da.git"
local MODEL_PATH = "classifier/model.onnx"
local LFS_POINTER_PREFIX = "version https://git-lfs.github.com/spec/v1"

local function command_output(label, result)
  if type(result) ~= "table" then return "" end
  local lines = {}
  if result.stdout and result.stdout ~= "" then
    lines[#lines + 1] = label .. " stdout: " .. result.stdout
  end
  if result.stderr and result.stderr ~= "" then
    lines[#lines + 1] = label .. " stderr: " .. result.stderr
  end
  return table.concat(lines, "\n")
end

local function append_output(message, ...)
  local details = {}
  for _, detail in ipairs({ ... }) do
    if detail ~= "" then details[#details + 1] = detail end
  end
  if #details == 0 then return message end
  return message .. ":\n" .. table.concat(details, "\n")
end

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
  local progress = plugin.progress or function() end
  local state, detail = model_state(plugin.dir)
  if state == nil then error("da package: " .. detail, 0) end
  if state == "pointer" then
    progress("Downloading classifier model (Git LFS)")
    local install = nefor.process.run {
      cmd = "git",
      args = { "lfs", "install", "--local", "--skip-repo" },
      cwd = plugin.dir,
    }
    if type(install) ~= "table" or install.code ~= 0 then
      error(append_output(
        "da package: cannot prepare repository-local Git LFS filters; install Git LFS and retry",
        command_output("git lfs install", install)
      ), 0)
    end
    local pull = nefor.process.run {
      cmd = "git",
      args = { "lfs", "pull", "--include", MODEL_PATH, "--exclude", "" },
      cwd = plugin.dir,
    }
    if type(pull) ~= "table" or pull.code ~= 0 then
      error(append_output(
        "da package: the embedded classifier model is a Git LFS pointer; " ..
          "`git lfs pull --include " .. MODEL_PATH .. " --exclude ''` failed",
        command_output("git lfs pull", pull)
      ), 0)
    end
    local after, after_detail = model_state(plugin.dir)
    if after ~= "materialized" then
      error(append_output(
        "da package: Git LFS did not materialize " .. tostring(after_detail),
        command_output("git lfs install", install),
        command_output("git lfs pull", pull)
      ), 0)
    end
  end

  progress("Compiling classifier (Cargo)")
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

local function release_target(system, machine)
  local systems = { Darwin = "apple-darwin", Linux = "unknown-linux-gnu" }
  local arches = { arm64 = "aarch64", aarch64 = "aarch64", x86_64 = "x86_64" }
  local os_part, arch = systems[system], arches[machine]
  local target = os_part and arch and (arch .. "-" .. os_part)
  if target == "aarch64-apple-darwin" or target == "aarch64-unknown-linux-gnu"
      or target == "x86_64-unknown-linux-gnu" then return target end
  error("da release: no prebuilt binary for " .. tostring(system) .. "/" .. tostring(machine), 0)
end

function M.release_package(opts)
  opts = opts or {}
  if type(opts.version) ~= "string" or not opts.version:match("^%d+%.%d+%.%d+$") then
    error("da release: `version` must be an exact release version", 0)
  end
  local function uname(flag)
    local result = nefor.process.run { cmd = "uname", args = { flag } }
    if result.code ~= 0 then error("da release: cannot detect platform: " .. (result.stderr or ""), 0) end
    return (result.stdout:gsub("%s+$", ""))
  end
  local target = release_target(uname("-s"), uname("-m"))
  local checksum = opts.checksums and opts.checksums[target]
  if type(checksum) ~= "string" or #checksum ~= 64 or checksum:find("[^%x]") then
    error("da release: a SHA-256 checksum is required for " .. target, 0)
  end
  return {
    name = opts.name or "da",
    archive = {
      url = "https://github.com/amenocturne/da/releases/download/v" .. opts.version .. "/da-" .. target .. ".tar.gz",
      sha256 = checksum,
      strip_components = 1,
    },
  }
end

M._internals = {
  release_target = release_target,
  build = build,
  model_state = model_state,
}

return M
