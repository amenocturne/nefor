-- nefor-pm — pure-Lua plugin manager.
--
-- install reconciles the caller's exact immutable identity; update refreshes
-- that same identity. Locks record installed state, never choose a revision.
-- Both return per-name {dir, identity, verification}. Optional spec.verify
-- receives {dir, name, identity} on staged and cached packages.
-- Managed Git requires url (or shorthand) and a full commit; archive requires
-- URL/SHA-256/layout. Local dir overrides and read-only register are explicit.

local M = {}
local archive = require("nefor-pm.archive")

-- Materialized package registry. Populated by register and install_spec;
-- pm.root/pm.bin resolve through it while pm.load delegates to Lua.
--   name → { dir = "/abs/path", source = "dir" | "data" }
local plugins = {}
local registered_searcher_installed = false

local function is_string(v) return type(v) == "string" and v ~= "" end
local function is_table(v) return type(v) == "table" end
local function is_function(v) return type(v) == "function" end

local function fail(label, msg)
  error(string.format("nefor-pm[%s]: %s", label, msg), 0)
end

-- Derive a git ref from the engine's version string.
-- Exact semver (e.g. "0.1.9") → tag "v0.1.9".
-- Anything with a suffix (nightly "0.1.9-12-gabcdef", dirty, etc.) → branch "main".
-- Returns ref, ref_kind.
local function engine_ref()
  local v = nefor and nefor.version
  if type(v) ~= "string" or v == "" then return "main", "branch" end
  if v:match("^%d+%.%d+%.%d+$") then
    return "v" .. v, "tag"
  end
  return "main", "branch"
end

-- Path join — trims trailing slash on a, leading on b.
local function pjoin(a, b)
  if not a or a == "" then return b end
  if not b or b == "" then return a end
  if a:sub(-1) == "/" then a = a:sub(1, -2) end
  if b:sub(1, 1) == "/" then b = b:sub(2) end
  return a .. "/" .. b
end

local function require_fs()
  if not (nefor and nefor.fs) then
    error("nefor-pm: nefor.fs binding not available", 0)
  end
  return nefor.fs
end

local function require_run()
  if not (nefor and nefor.process and nefor.process.run) then
    error("nefor-pm: nefor.process.run binding not available", 0)
  end
  return nefor.process.run
end

-- Idempotent `<dir>/?.lua;<dir>/?/init.lua` graft onto package.path.
-- pm.install calls this for every installed plugin's parent dir so
-- subsequent require() calls resolve through Lua's normal search.
local function ensure_on_path(dir)
  local patterns = {
    pjoin(dir, "?.lua"),
    pjoin(dir, "?/init.lua"),
  }
  for _, p in ipairs(patterns) do
    if not package.path:find(p, 1, true) then
      package.path = p .. ";" .. package.path
    end
  end
end

local function install_registered_searcher()
  if registered_searcher_installed then return end
  local searchers = package.searchers or package.loaders
  table.insert(searchers, 1, function(name)
    local root_name, suffix = name:match("^([^.]+)%.?(.*)$")
    local entry = root_name and plugins[root_name] or nil
    if not entry or entry.source ~= "registered" then return nil end
    local relative = suffix == "" and "init.lua" or (suffix:gsub("%.", "/") .. ".lua")
    local path = pjoin(entry.dir, relative)
    if suffix ~= "" and not require_fs().exists(path) then
      path = pjoin(entry.dir, suffix:gsub("%.", "/") .. "/init.lua")
    end
    if not require_fs().exists(path) then return "\n\tno file " .. path end
    local chunk, err = loadfile(path)
    if not chunk then return "\n\t" .. tostring(err) end
    return chunk, path
  end)
  registered_searcher_installed = true
end

-- Delegates to `nefor.fs.data_root()` — the engine's canonical resolved
-- data directory (CLI flag > NEFOR_DATA_DIR env var > XDG_DATA_HOME/nefor).
-- Avoids the historical Lua-side re-resolver that invented a
-- `NEFOR_DATA_HOME` the Rust resolver doesn't know about, which silently
-- diverged when both were set to different values.
local function data_root()
  return nefor.fs.data_root()
end

local function plugins_root()
  return pjoin(data_root(), "plugins")
end

local function parent_dir(path)
  return path:match("^(.*)/[^/]+$") or "."
end

local function lockfile_path()
  return pjoin(plugins_root(), "nefor-pm.lock.json")
end

-- Synchronous process driver. Returns `{ ok, exit_code, stdout, stderr }`
-- as data so the caller decides how to react. Spawn failures surface as
-- exit_code = -1 (the same shape Rust returns for that case).
local function run_cmd(argv, opts)
  opts = opts or {}
  local run = require_run()
  local result = run({
    cmd  = argv[1],
    args = { table.unpack(argv, 2) },
    cwd  = opts.cwd,
    env  = opts.env,
  })
  return {
    ok        = result.code == 0,
    exit_code = result.code,
    stdout    = result.stdout or "",
    stderr    = result.stderr or "",
  }
end

local function run_or_die(label, argv, opts)
  local res = run_cmd(argv, opts)
  if not res.ok then
    fail(label, string.format(
      "%s exited %d:\n%s",
      argv[1], res.exit_code,
      res.stderr ~= "" and res.stderr or res.stdout))
  end
  return res
end

-- Returns a normalized spec table:
--   { name, url?, tag?, branch?, commit?, path?, dir?, build?, ref, ref_kind }
-- Managed operations require a full commit selected by the caller.
local function parse_spec(spec, index)
  if not is_table(spec) then
    error(string.format("nefor-pm: spec #%d must be a table, got %s",
                        index, type(spec)), 0)
  end
  if not is_string(spec.name) then
    error(string.format(
      "nefor-pm: spec #%d missing required `name` field (must be a non-empty string)",
      index), 0)
  end
  local copy = {}
  for k, v in pairs(spec) do copy[k] = v end
  spec = copy
  local label = spec.name
  if label == "." or label == ".." or label:find("/", 1, true) then
    fail(label, "package name must be a single directory name")
  end
  if spec.verify ~= nil and not is_function(spec.verify) then fail(label, "`verify` must be a function") end
  if spec.archive ~= nil then
    for _, key in ipairs({ 1, "url", "tag", "branch", "commit", "path", "dir", "build" }) do
      if spec[key] ~= nil then fail(label, "`archive` cannot be combined with `" .. tostring(key) .. "`") end
    end
    if label == "." or label == ".." or label:find("/", 1, true) then
      fail(label, "archive package name must be a single directory name")
    end
    local ok, normalized = pcall(archive.normalize, spec.archive)
    if not ok then fail(label, normalized) end
    return { name = label, archive = normalized, verify = spec.verify }
  end

  local shorthand = spec[1]
  local url = spec.url
  if shorthand ~= nil and url ~= nil then
    fail(label, "both shorthand owner/repo and `url` provided — pick one")
  end
  if is_string(shorthand) then
    if not shorthand:find("/", 1, true) then
      fail(label, "shorthand must be `owner/repo`, got " .. tostring(shorthand))
    end
    url = "https://github.com/" .. shorthand .. ".git"
  end

  if not spec.dir then
    if spec.tag ~= nil or spec.branch ~= nil then
      fail(label, "managed Git requires an exact full `commit`, not tag or branch")
    end
    if not is_string(spec.commit) or #spec.commit ~= 40 or spec.commit:find("[^%x]") then
      fail(label, "managed Git requires an exact full 40-hex `commit`")
    end
  end
  local ref, ref_kind = spec.commit and spec.commit:lower(), "commit"

  if spec.build ~= nil and not is_function(spec.build) then
    fail(label, "`build` must be a function, got " .. type(spec.build))
  end

  if spec.path ~= nil and not is_string(spec.path) then
    fail(label, "`path` must be a string, got " .. type(spec.path))
  end

  if spec.dir ~= nil and not is_string(spec.dir) then
    fail(label, "`dir` must be a string, got " .. type(spec.dir))
  end
  -- Resolve relative dir overrides to absolute. A symlink target that's
  -- relative is interpreted relative to the LINK's own directory (not
  -- cwd) — passing a relative dir from a consumer that's launched from
  -- somewhere else would create a broken symlink. Absolutize once at
  -- the entrypoint so spec.dir is always a real, link-portable path.
  if spec.dir ~= nil then
    if spec.dir:sub(1, 1) ~= "/" then
      local cwd = os.getenv("PWD") or "."
      spec.dir = cwd .. "/" .. spec.dir
    end
  end

  -- Non-dir specs need a url to clone from.
  if not spec.dir and not is_string(url) then
    fail(label, "no `dir` override and no clonable source (set [1] = \"owner/repo\" or `url`)")
  end

  return {
    name     = spec.name,
    url      = url,
    tag      = spec.tag,
    branch   = spec.branch,
    commit   = spec.commit,
    path     = spec.path,
    dir      = spec.dir,
    build    = spec.build,
    verify   = spec.verify,
    ref      = ref,
    ref_kind = ref_kind,
  }
end

local function read_lockfile()
  local fs = require_fs()
  local res = fs.read_file(lockfile_path())
  if not res.ok then
    if fs.exists(lockfile_path()) then error("nefor-pm: cannot read lockfile: " .. tostring(res.error), 0) end
    return {}
  end
  local body = res.content or ""
  if body == "" then error("nefor-pm: lockfile is invalid: " .. lockfile_path(), 0) end
  local ok, decoded = pcall(nefor.json.decode, body)
  if not ok or type(decoded) ~= "table" then
    error("nefor-pm: lockfile is invalid: " .. lockfile_path(), 0)
  end
  for name, entry in pairs(decoded) do
    local valid = type(name) == "string" and type(entry) == "table"
    if valid and entry.kind == "archive" then
      valid = pcall(archive.normalize, entry)
    else
      valid = valid and entry.kind == nil and is_string(entry.ref) and is_string(entry.commit)
        and #entry.commit == 40 and not entry.commit:find("[^%x]")
      if valid and entry.identity ~= nil then
        local identity = entry.identity
        valid = type(identity) == "table" and identity.kind == "git" and is_string(identity.url)
          and identity.commit == entry.commit and (identity.path == nil or is_string(identity.path))
      end
    end
    if not valid then
      error("nefor-pm: lockfile has an invalid entry: " .. tostring(name), 0)
    end
  end
  return decoded
end

-- Encode sorted-by-key for stable diffs. nefor.json.encode does not
-- guarantee key order across pairs(); we build the JSON object manually
-- from a sorted key list so the lockfile diffs cleanly across runs.
local function write_lockfile(lock)
  local fs = require_fs()
  local mkdir = fs.mkdir_p(plugins_root())
  if not mkdir.ok then
    error("nefor-pm: cannot create plugins root: " .. tostring(mkdir.error), 0)
  end
  local keys = {}
  for k in pairs(lock) do keys[#keys + 1] = k end
  table.sort(keys)
  local out = "{"
  for i, k in ipairs(keys) do
    local entry_json = nefor.json.encode(lock[k])
    if i > 1 then out = out .. "," end
    out = out .. nefor.json.encode(k) .. ":" .. entry_json
  end
  out = out .. "}\n"
  local temporary = lockfile_path() .. ".tmp"
  local write = fs.write_file(temporary, out)
  if not write.ok then
    error("nefor-pm: cannot write lockfile: " .. tostring(write.error), 0)
  end
  local renamed, rename_error = os.rename(temporary, lockfile_path())
  if not renamed then
    fs.remove(temporary)
    error("nefor-pm: cannot replace lockfile: " .. tostring(rename_error), 0)
  end
end

-- Lua callback bodies have no portable identity. Explicit update refreshes
-- builds when only callback code changes; metadata is a cache predicate.
local function stable_encode(t)
  if type(t) ~= "table" then return nefor.json.encode(t) end
  local keys = {}
  for k in pairs(t) do keys[#keys + 1] = k end
  table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
  local parts = {}
  for _, k in ipairs(keys) do
    parts[#parts + 1] = nefor.json.encode(tostring(k)) .. ":" .. stable_encode(t[k])
  end
  return "{" .. table.concat(parts, ",") .. "}"
end

-- djb2 — small, deterministic, non-cryptographic. Collisions are
-- acceptable: this drives a cache-invalidation predicate, not security.
local function djb2(s)
  local h = 5381
  for i = 1, #s do
    h = ((h * 33) + s:byte(i)) % 0xFFFFFFFF
  end
  return string.format("%08x", h)
end

local function compute_build_hash(spec)
  if not spec.build then return nil end
  local payload = {
    name   = spec.name,
    tag    = spec.tag,
    branch = spec.branch,
    commit = spec.commit,
    path   = spec.path,
  }
  return djb2(stable_encode(payload))
end

local function git(label, args, opts)
  return run_or_die(label, { "git", table.unpack(args) }, opts)
end

local function current_commit(dir)
  local res = run_cmd({ "git", "-C", dir, "rev-parse", "--verify", "HEAD^{commit}" })
  if not res.ok then return nil end
  return (res.stdout:gsub("%s+$", ""))
end

local function resolved_commit(dir, ref)
  local res = run_cmd({ "git", "-C", dir, "rev-parse", "--verify", ref .. "^{commit}" })
  if not res.ok then return nil end
  return (res.stdout:gsub("%s+$", ""))
end

local function verify_checkout(label, dir, expected_ref)
  local expected = resolved_commit(dir, expected_ref)
  local head = current_commit(dir)
  if not expected then
    fail(label, "requested revision is unavailable after checkout: " .. expected_ref)
  end
  if head ~= expected then
    fail(label, string.format(
      "checkout HEAD mismatch: expected %s (%s), got %s",
      expected_ref, expected, tostring(head)))
  end
  return head
end

-- A successfully cloned plugin's marker — git always creates `.git`. We use
-- it as the "is this a real checkout?" sentinel rather than just relying on
-- the directory existing (the user might have an empty bin/ left over from a
-- failed install).
local function is_cloned(dir)
  local fs = require_fs()
  return fs.exists(pjoin(dir, ".git/HEAD")) or fs.exists(pjoin(dir, ".git"))
end

local function clone(label, spec, target_dir, progress)
  progress("Downloading package")
  local args = { "clone", "--depth", "1" }
  if spec.path then
    -- Sparse + blobless: cheaper for monorepo subtree extraction.
    args[#args + 1] = "--filter=blob:none"
    args[#args + 1] = "--sparse"
  end
  if spec.ref_kind == "branch" or spec.ref_kind == "tag" then
    args[#args + 1] = "--branch"
    args[#args + 1] = spec.ref
  end
  args[#args + 1] = spec.url
  args[#args + 1] = target_dir
  git(label, args)

  if spec.path then
    git(label, { "-C", target_dir, "sparse-checkout", "set", spec.path })
  end

  if spec.ref_kind == "commit" then
    -- Commit pins: cloned default-branch shallowly; fetch+checkout the
    -- target sha. --depth=1 keeps bandwidth low.
    progress("Fetching revision " .. spec.ref)
    git(label, { "-C", target_dir, "fetch", "--depth", "1", "origin", spec.ref })
    progress("Checking out " .. spec.ref)
    git(label, { "-C", target_dir, "checkout", spec.ref })
  end
end

local function normalize_sparse_paths(paths)
  if paths == nil then return nil end
  if is_string(paths) then return { paths } end
  if not is_table(paths) then
    error("nefor-pm.sync_checkout: `sparse` must be a string or list of strings", 0)
  end
  local out = {}
  for i, path in ipairs(paths) do
    if not is_string(path) then
      error(string.format(
        "nefor-pm.sync_checkout: sparse path #%d must be a non-empty string",
        i), 0)
    end
    out[#out + 1] = path
  end
  return out
end

-- Trim leading and trailing slashes. "/a/b/" -> "a/b", "a" -> "a", "" -> "".
local function strip_slashes(s)
  if not s or s == "" then return "" end
  return (s:gsub("^/+", ""):gsub("/+$", ""))
end

-- After sparse-checkout, the plugin's files land at
-- `<target_dir>/<spec.path>/`, but `require("<name>")` (resolved through
-- the `<plugins_root>/?/init.lua` + `<plugins_root>/?.lua` graft) needs
-- them flat at `<target_dir>/`. Flatten by staging the subtree dir to a
-- sibling under plugins_root, wiping non-`.git` entries from target_dir,
-- then moving the staged contents (visible and hidden) back.
--
-- `.git/` stays at `<target_dir>/.git/` so subsequent fetch/update works.
-- Idempotent: if the subtree dir is already empty/absent this no-ops.
local function flatten_subtree(label, target_dir, spec_path)
  local rel = strip_slashes(spec_path)
  if rel == "" then return end
  local subtree_dir = pjoin(target_dir, rel)

  local fs = require_fs()
  if not fs.exists(subtree_dir) then fail(label, "requested subtree is absent: " .. spec_path) end

  -- Stage the subtree to a sibling of target_dir so the move is a single
  -- rename within the same filesystem. Pre-clean any stale staging dir
  -- from a prior crashed install.
  local staging = target_dir .. ".staging"
  run_cmd({ "rm", "-rf", staging })
  run_or_die(label, { "mv", subtree_dir, staging })

  -- Wipe everything in target_dir except .git (clears the intermediate
  -- path components from `spec.path` and any stale files from a prior
  -- flatten when ref changed). `find` is portable and won't follow the
  -- preserved `.git/` because we filter by name at top level.
  run_or_die(label, {
    "sh", "-c",
    [[set -e; cd -- "$1" && find . -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +]],
    "_", target_dir,
  })

  -- Move staged entries (visible + hidden) up into target_dir.
  run_or_die(label, {
    "sh", "-c",
    [[set -e; cd -- "$1" && find . -mindepth 1 -maxdepth 1 -exec mv -- {} "$2/" \;]],
    "_", staging, target_dir,
  })

  run_or_die(label, { "rmdir", staging })
end

local function verify_package(spec, dir, identity)
  if not spec.verify then return nil end
  local evidence = spec.verify({name = spec.name, dir = dir, identity = identity})
  if evidence ~= nil then nefor.json.encode(evidence) end
  return evidence
end

local function stage_git(label, spec, target, exists, prepare)
  local fs = require_fs()
  local staging = run_or_die(label, {"mktemp", "-d", parent_dir(target) .. "/." .. label .. ".XXXXXX"}).stdout:gsub("%s+$", "")
  local content, previous = staging .. "/content", staging .. "/previous"
  local phase = "staging"
  local function rename(from, to)
    local ok, err = os.rename(from, to)
    if not ok then fail(label, "cannot move " .. from .. " to " .. to .. ": " .. tostring(err)) end
  end
  local function remove(dir)
    local result = fs.remove_dir_all(dir)
    if not result.ok then fail(label, "cannot remove " .. dir .. ": " .. tostring(result.error)) end
  end
  local function rollback()
    if phase == "installed" then remove(target) end
    if exists and (phase == "installed" or phase == "backed-up") then rename(previous, target) end
    remove(staging)
  end
  local ok, result = pcall(function()
    clone(label, spec, content, spec.progress or function() end)
    verify_checkout(label, content, spec.ref)
    local evidence = prepare(content)
    if exists then rename(target, previous) end
    phase = "backed-up"
    rename(content, target)
    phase = "installed"
    return evidence
  end)
  if not ok then
    local restored, err = pcall(rollback)
    if not restored then result = tostring(result) .. "\nRollback failed: " .. tostring(err) end
    error(result, 0)
  end
  return result, {rollback = rollback, commit = function()
    local removed = fs.remove_dir_all(staging)
    if not removed.ok then io.stderr:write("nefor-pm[" .. label .. "]: cannot remove staging: " .. tostring(removed.error) .. "\n") end
  end}
end

local function sync_checkout(opts, refresh)
  if not is_table(opts) then error("nefor-pm.sync_checkout: opts must be a table", 0) end
  local label = opts.name or "checkout"
  if not is_string(opts.dir) or not is_string(opts.url) then fail(label, "requires dir and url") end
  if opts.ref_kind ~= "commit" or not is_string(opts.ref) or #opts.ref ~= 40 or opts.ref:find("[^%x]") then
    fail(label, "requires ref_kind = commit and an exact full 40-hex ref")
  end
  local fs = require_fs()
  local sparse = normalize_sparse_paths(opts.sparse)
  local identity = {kind = "git", url = opts.url, commit = opts.ref:lower(), sparse = sparse}
  local marker = opts.dir .. "/.nefor-pm-checkout.json"
  local recorded = fs.read_file(marker)
  local same = false
  if recorded.ok then
    local ok, value = pcall(nefor.json.decode, recorded.content)
    same = ok and stable_encode(value) == stable_encode(identity)
  end
  local pin
  if opts.lockfile and fs.exists(opts.lockfile) then
    local read = fs.read_file(opts.lockfile)
    if not read.ok then fail(label, "cannot read checkout lock: " .. tostring(read.error)) end
    pin = read.content:match("^%s*(%x+)%s*$")
    if not pin or #pin ~= 40 then fail(label, "checkout lock is invalid: " .. opts.lockfile) end
  end
  if fs.is_symlink(opts.dir) then fail(label, "cannot replace development symlink") end
  local exists = fs.exists(opts.dir)
  if exists and (not is_cloned(opts.dir) or (not recorded.ok and not pin)) then fail(label, "cannot replace unowned checkout") end
  local made = fs.mkdir_p(parent_dir(opts.dir))
  if not made.ok then fail(label, made.error) end
  local transaction
  if refresh or not same or current_commit(opts.dir) ~= identity.commit then
    local spec = {url = opts.url, ref = identity.commit, ref_kind = "commit"}
    local _, tx = stage_git(label, spec, opts.dir, exists, function(content)
      if sparse then
        git(label, {"-C", content, "sparse-checkout", "init", "--cone"})
        git(label, {"-C", content, "sparse-checkout", "set", table.unpack(sparse)})
      end
      local written = fs.write_file(content .. "/.nefor-pm-checkout.json", nefor.json.encode(identity))
      if not written.ok then fail(label, written.error) end
    end)
    transaction = tx
  end
  local ok, err = pcall(function()
    if opts.lockfile then
      local temp = opts.lockfile .. ".tmp"
      local written = fs.write_file(temp, identity.commit .. "\n")
      if not written.ok then fail(label, "cannot write checkout lock: " .. tostring(written.error)) end
      local renamed, why = os.rename(temp, opts.lockfile)
      if not renamed then fs.remove(temp); fail(label, "cannot replace checkout lock: " .. tostring(why)) end
    end
  end)
  if not ok then
    if transaction then transaction.rollback() end
    error(err, 0)
  end
  if transaction then transaction.commit() end
  return {dir = opts.dir, ref = identity.commit, ref_kind = "commit", commit = identity.commit, head = identity.commit, identity = identity}
end

local function install_spec(spec, lock, refresh, progress)
  local label, fs = spec.name, require_fs()
  if plugins[label] and plugins[label].source == "registered" then fail(label, "cannot rebind a registered package") end
  if spec.dir then
    local made = fs.mkdir_p(plugins_root())
    if not made.ok then fail(label, made.error) end
    ensure_on_path(plugins_root())
    local link = pjoin(plugins_root(), label)
    local old = fs.is_symlink(link) and fs.read_link(link) or nil
    if fs.exists(link) and not old then fail(label, "cannot create dev-override symlink: a non-symlink entry already exists") end
    local identity = {kind = "dir", dir = spec.dir}
    local evidence = verify_package(spec, spec.dir, identity)
    local changed = old ~= spec.dir
    if changed then
      if old then fs.remove(link) end
      local result = fs.symlink(spec.dir, link)
      if not result.ok then
        if old then fs.symlink(old, link) end
        fail(label, "symlink failed: " .. tostring(result.error))
      end
    end
    plugins[label] = {dir = spec.dir, source = "dir"}
    return lock[label], changed and {rollback = function()
      fs.remove(link)
      if old then fs.symlink(old, link) end
    end, commit = function() end} or nil, {dir = spec.dir, identity = identity, verification = evidence}
  end
  local target = pjoin(plugins_root(), label)
  local made = fs.mkdir_p(plugins_root())
  if not made.ok then fail(label, made.error) end
  ensure_on_path(plugins_root())
  local entry = lock[label]
  if spec.archive then
    local installed, transaction, evidence = archive.install(spec, plugins_root(), entry, progress, run_cmd, refresh)
    plugins[label] = {dir = target, source = "data"}
    return installed, transaction, {dir = target, identity = spec.archive, verification = evidence}
  end
  if fs.is_symlink(target) then fail(label, "Git install cannot replace a development symlink") end
  local exists = fs.exists(target)
  if exists and not entry then fail(label, "Git install cannot replace an unowned directory: " .. target) end
  local identity = {kind = "git", url = spec.url, commit = spec.ref, path = spec.path}
  local installed = {ref = spec.ref, commit = spec.ref, identity = identity, build_hash = compute_build_hash(spec)}
  local evidence, transaction
  if not refresh and entry and stable_encode(entry.identity) == stable_encode(identity)
      and entry.build_hash == installed.build_hash and current_commit(target) == spec.ref then
    evidence = verify_package(spec, target, identity)
  else
    spec.progress = progress
    evidence, transaction = stage_git(label, spec, target, exists, function(content)
      if spec.path then flatten_subtree(label, content, spec.path) end
      if spec.build then
        progress("Building package")
        local bin = fs.mkdir_p(pjoin(content, "bin"))
        if not bin.ok then fail(label, bin.error) end
        spec.build({dir = content, name = label, tag = spec.ref, ref = spec.ref, url = spec.url, repo = spec.url, identity = identity, progress = progress})
      end
      return verify_package(spec, content, identity)
    end)
  end
  if transaction then
    local ok, value = pcall(verify_package, spec, target, identity)
    if not ok then transaction.rollback(); error(value, 0) end
    evidence = value
  end
  plugins[label] = {dir = target, source = "data"}
  return installed, transaction, {dir = target, identity = identity, verification = evidence}
end

local function apply_specs(specs, update, opts)
  opts = opts or {}
  if not is_table(opts) or (opts.on_progress ~= nil and not is_function(opts.on_progress)) then
    error("nefor-pm: options.on_progress must be a function", 0)
  end
  local on_progress = opts.on_progress
  local completed, results = {}, {}
  if not is_table(specs) then
    error("nefor-pm: specs must be a list of tables", 0)
  end
  local lock = read_lockfile()
  local new_lock = {}
  -- Preserve entries for plugins not mentioned in this operation (so a
  -- partial install or update doesn't wipe sibling lock state).
  for k, v in pairs(lock) do new_lock[k] = v end

  local transactions = {}
  local previous_plugins = {}
  for name, entry in pairs(plugins) do previous_plugins[name] = entry end
  local ok, err = pcall(function()
    for i, raw in ipairs(specs) do
      local spec = parse_spec(raw, i)
      local reported = false
      local function progress(message)
        if on_progress then
          on_progress(spec.name, message)
          reported = true
        end
      end
      local success, entry, transaction, result = pcall(install_spec, spec, new_lock, update, progress)
      if not success then
        local desired = spec.archive or {kind = spec.dir and "dir" or "git", url = spec.url, commit = spec.ref, path = spec.path, dir = spec.dir}
        fail(spec.name, "cannot reconcile desired " .. stable_encode(desired) .. "; recorded " .. stable_encode(new_lock[spec.name]) .. "\n" .. tostring(entry))
      end
      results[spec.name] = result
      if transaction then transactions[#transactions + 1] = transaction end
      if reported then completed[#completed + 1] = spec.name end
      if entry ~= nil then new_lock[spec.name] = entry end
    end
    write_lockfile(new_lock)
  end)
  if not ok then
    for i = #transactions, 1, -1 do
      local restored, restore_error = pcall(transactions[i].rollback)
      if not restored then err = tostring(err) .. "\nRollback failed: " .. tostring(restore_error) end
    end
    for name in pairs(plugins) do plugins[name] = nil end
    for name, entry in pairs(previous_plugins) do plugins[name] = entry end
    error(err, 0)
  end
  for _, transaction in ipairs(transactions) do transaction.commit() end
  for _, name in ipairs(completed) do on_progress(name, "Ready") end
  return results
end

function M.stderr_progress(name, message)
  io.stderr:write("[nefor-pm] " .. name .. ": " .. message .. "\n")
  io.stderr:flush()
end

function M.install(specs, opts)
  return apply_specs(specs, false, opts)
end

function M.update(specs, opts)
  return apply_specs(specs, true, opts)
end

function M.register(specs)
  if not is_table(specs) then
    error("nefor-pm.register: specs must be a list of tables", 0)
  end
  install_registered_searcher()
  for index, spec in ipairs(specs) do
    if not is_table(spec) or not is_string(spec.name) or not is_string(spec.dir) then
      error(string.format(
        "nefor-pm.register: spec #%d requires non-empty `name` and `dir` fields", index), 0)
    end
    if spec.dir:sub(1, 1) ~= "/" then
      fail(spec.name, "registered `dir` must be absolute")
    end
    local existing = plugins[spec.name]
    if existing and (existing.dir ~= spec.dir or existing.source ~= "registered") then
      fail(spec.name, "already registered with a different source")
    end
    if not require_fs().exists(spec.dir) then
      fail(spec.name, "registered dir does not exist: " .. spec.dir)
    end
    plugins[spec.name] = { dir = spec.dir, source = "registered" }
  end
end

-- pm.root and pm.bin use the registry to find a package's dir (the binary's
-- location is a pm concern, not Lua's). pm.load delegates to require —
-- after pm.install has put the right parent dirs on package.path, Lua's
-- own resolution handles dotted names, init.lua entrypoints, and the
-- module → file mapping uniformly.
local function package_dir(name)
  local entry = plugins[name]
  if entry then return entry.dir end
  -- The plugins directory is nefor-pm's materialization root even when the
  -- package is consumed as MAG data rather than Lua or an executable. A prior
  -- install in another process therefore remains resolvable without replaying
  -- the original spec.
  local fallback = pjoin(plugins_root(), name)
  local fs = require_fs()
  if fs.exists(fallback) then
    plugins[name] = { dir = fallback, source = "data" }
    return fallback
  end
  return nil
end

function M.root(name)
  if not is_string(name) then
    error("nefor-pm.root: name must be a non-empty string", 0)
  end
  local dir = package_dir(name)
  if not dir then
    error(string.format(
      "nefor-pm.root: package %q is not registered or installed", name), 0)
  end
  return dir
end

function M.load(name)
  if not is_string(name) then
    error("nefor-pm.load: name must be a non-empty string", 0)
  end
  return require(name)
end

function M.require(name)
  -- Alias kept for build-shorthand parity with lazy.nvim's spelling:
  --   build = function(p) pm.require("name").build(p) end
  return M.load(name)
end

function M.sync_checkout(opts)
  return sync_checkout(opts, false)
end

function M.update_checkout(opts)
  return sync_checkout(opts, true)
end

function M.bin(name, binary_name)
  if not is_string(name) then
    error("nefor-pm.bin: name must be a non-empty string", 0)
  end
  binary_name = binary_name or name
  local dir = package_dir(name)
  if not dir then
    error(string.format(
      "nefor-pm.bin: plugin %q not installed (run pm.install first)", name), 0)
  end
  local path = pjoin(pjoin(dir, "bin"), binary_name)
  local fs = require_fs()
  if not fs.exists(path) then
    error(string.format(
      "nefor-pm.bin: binary %q for plugin %q not found at %s",
      binary_name, name, path), 0)
  end
  return path
end

function M.engine_ref() return engine_ref() end

M._internals = {
  parse_spec      = parse_spec,
  engine_ref      = engine_ref,
  data_root       = data_root,
  plugins_root    = plugins_root,
  lockfile_path   = lockfile_path,
  read_lockfile   = read_lockfile,
  write_lockfile  = write_lockfile,
  compute_build_hash = compute_build_hash,
  sync_checkout   = sync_checkout,
  djb2            = djb2,
  stable_encode   = stable_encode,
  plugins         = plugins,
  reset           = function()
    for k in pairs(plugins) do plugins[k] = nil end
  end,
  register        = function(name, dir)
    plugins[name] = { dir = dir, source = "test" }
  end,
  ensure_on_path  = ensure_on_path,
}

return M
