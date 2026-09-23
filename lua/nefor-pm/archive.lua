-- Verified tar.gz packages. Staging stays on the destination filesystem so
-- replacing a managed package can be rolled back until its lock is persisted.
local M = {}
local MARKER = ".nefor-pm-archive.json"

function M.normalize(value)
  if type(value) ~= "table" then error("`archive` must be a table", 0) end
  if type(value.url) ~= "string" or value.url == "" then
    error("archive.url must be a non-empty string", 0)
  end
  if type(value.sha256) ~= "string" or #value.sha256 ~= 64 or value.sha256:find("[^%x]") then
    error("archive.sha256 must contain exactly 64 hexadecimal characters", 0)
  end
  local strip = value.strip_components or 0
  if type(strip) ~= "number" or strip < 0 or strip % 1 ~= 0 then
    error("archive.strip_components must be a nonnegative integer", 0)
  end
  return { kind = "archive", url = value.url, sha256 = value.sha256:lower(), strip_components = strip }
end

local function same(left, right)
  return type(left) == "table" and left.kind == "archive"
    and left.url == right.url and left.sha256 == right.sha256
    and left.strip_components == right.strip_components
end

function M.install(spec, root, locked, progress, run)
  local fs = nefor.fs
  local target = root .. "/" .. spec.name
  local desired = spec.archive
  local function fail(message) error("nefor-pm[" .. spec.name .. "]: " .. message, 0) end
  local function command(argv)
    local result = run(argv)
    if not result.ok then
      fail(argv[1] .. " exited " .. tostring(result.exit_code) .. ": " ..
        (result.stderr ~= "" and result.stderr or result.stdout))
    end
    return result.stdout
  end
  local function rename(from, to)
    local ok, err = os.rename(from, to)
    if not ok then fail("cannot move " .. from .. " to " .. to .. ": " .. tostring(err)) end
  end
  local function remove(path)
    local result = fs.remove_dir_all(path)
    if not result.ok then fail("cannot remove " .. path .. ": " .. tostring(result.error)) end
  end

  if fs.is_symlink(target) then fail("archive install cannot replace a development symlink: " .. target) end
  local exists = fs.exists(target)
  if exists and not locked then fail("archive install cannot replace an unowned directory: " .. target) end
  local marker = fs.read_file(target .. "/" .. MARKER)
  if exists and same(locked, desired) and marker.ok then
    local ok, installed = pcall(nefor.json.decode, marker.content)
    if ok and same(installed, desired) then return desired end
  end

  local staging = command({ "mktemp", "-d", root .. "/." .. spec.name .. ".XXXXXX" }):gsub("%s+$", "")
  local content = staging .. "/content"
  local previous = staging .. "/previous"
  local phase = "staging"
  local function rollback()
    if phase == "installed" then remove(target) end
    if phase == "installed" or phase == "backed-up" then
      if exists then rename(previous, target) end
    end
    remove(staging)
  end
  local ok, err = pcall(function()
    progress("Downloading archive")
    local download = staging .. "/package.tar.gz"
    command({ "curl", "--fail", "--location", "--silent", "--show-error", "--output", download, desired.url })
    progress("Verifying archive checksum")
    local checksum = run({ "shasum", "-a", "256", download })
    if checksum.exit_code == -1 then checksum = run({ "sha256sum", download }) end
    if not checksum.ok then fail("cannot compute archive SHA-256: " .. checksum.stderr) end
    local actual = checksum.stdout:match("^(%x+)")
    if actual ~= desired.sha256 then
      fail("archive SHA-256 mismatch: expected " .. desired.sha256 .. ", got " .. tostring(actual))
    end
    progress("Extracting archive")
    local made = fs.mkdir_p(content)
    if not made.ok then fail("cannot create staging directory: " .. tostring(made.error)) end
    command({ "tar", "-xzf", download, "--strip-components", tostring(desired.strip_components), "-C", content })
    local entries, list_error = fs.list_dir(content)
    if not entries or #entries == 0 then fail("archive produced no files: " .. tostring(list_error or content)) end
    local written = fs.write_file(content .. "/" .. MARKER, nefor.json.encode(desired))
    if not written.ok then fail("cannot record archive identity: " .. tostring(written.error)) end
    if exists then rename(target, previous) end
    phase = "backed-up"
    rename(content, target)
    phase = "installed"
  end)
  if not ok then
    local restored, restore_error = pcall(rollback)
    if not restored then err = tostring(err) .. "\nRollback failed: " .. tostring(restore_error) end
    error(err, 0)
  end
  return desired, {
    rollback = rollback,
    commit = function()
      local cleaned = fs.remove_dir_all(staging)
      if not cleaned.ok then
        io.stderr:write("nefor-pm[" .. spec.name .. "]: installed successfully, but cannot remove staging directory " ..
          staging .. ": " .. tostring(cleaned.error) .. "\n")
        io.stderr:flush()
      end
    end,
  }
end

return M
