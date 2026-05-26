-- nefor-pm — pure-Lua plugin manager.
--
-- Public API:
--   pm.install(specs)        ensure each spec is on disk, write lockfile.
--                            Synchronous: blocks until every clone/build
--                            finishes so init.lua can rely on plugins
--                            being present immediately after the call.
--   pm.load(name)            plain `require(name)`. Resolution is Lua's job —
--                            pm.install has already augmented package.path so
--                            installed plugins' dirs are searched.
--   pm.bin(name[, binname])  resolve <plugin_dir>/bin/<binname> (default: name).
--   pm.require(name)         alias of pm.load — never triggers install.
--   pm.engine_ref()          returns (ref, ref_kind) derived from nefor.version:
--                            exact semver → ("vX.Y.Z", "tag"); otherwise ("main", "branch").
--
-- Spec shape (see design note):
--   {
--     "owner/repo",                     -- [1] shorthand → https://github.com/owner/repo
--     name   = "string",                -- required, non-empty.
--     tag    = "v0.1.5",                -- optional. default: engine version tag (or "main" for dev).
--     branch = "main",                  -- optional. mutually exclusive with tag/commit.
--     commit = "<sha>",                 -- optional. mutually exclusive with tag/branch.
--     url    = "...",                   -- optional. mutually exclusive with [1].
--     path   = "subtree/",              -- optional. sparse-checkout this dir only.
--     dir    = "/local/path",           -- optional. dev override — skip clone.
--                                          Install creates a symlink
--                                          <plugins_root>/<name> -> dir so
--                                          require("<name>") resolves through
--                                          the canonical <plugins_root>/?/init.lua
--                                          graft. basename(dir) does not need
--                                          to match name.
--     build  = function(plugin) ... end -- optional. runs after clone.
--   }

local M = {}

-- Resolver registry. Populated by parse_spec (dir override) and install_spec
-- (clone path). Lookups for pm.load / pm.bin go through here.
--   name → { dir = "/abs/path", source = "dir" | "data" }
local plugins = {}

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
-- ref/ref_kind = the resolved ref to check out.
-- Defaults to the engine's version tag for exact releases, "main" branch otherwise.
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
  local label = spec.name

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

  local ref, ref_kind
  local ref_count = 0
  if is_string(spec.tag)    then ref = spec.tag;    ref_kind = "tag";    ref_count = ref_count + 1 end
  if is_string(spec.branch) then ref = spec.branch; ref_kind = "branch"; ref_count = ref_count + 1 end
  if is_string(spec.commit) then ref = spec.commit; ref_kind = "commit"; ref_count = ref_count + 1 end
  if ref_count > 1 then
    fail(label, "at most one of `tag`, `branch`, `commit` may be set")
  end
  if ref_count == 0 then ref, ref_kind = engine_ref() end

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
    ref      = ref,
    ref_kind = ref_kind,
  }
end

local function read_lockfile()
  local fs = require_fs()
  local res = fs.read_file(lockfile_path())
  if not res.ok then return {} end
  local body = res.content or ""
  if body == "" then return {} end
  local ok, decoded = pcall(nefor.json.decode, body)
  if not ok or type(decoded) ~= "table" then
    return {}
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
  local write = fs.write_file(lockfile_path(), out)
  if not write.ok then
    error("nefor-pm: cannot write lockfile: " .. tostring(write.error), 0)
  end
end

-- We hash a stable JSON serialization of the spec's *configuration* fields
-- (everything except `dir`, `url`, and the `build` function itself). Changing
-- tag / branch / commit / path / name triggers a rebuild; moving the clone
-- url or pointing at a local dir does not. Lua functions have no portable
-- hash; documented limitation — bumping `tag` forces a rebuild.

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
  local res = run_cmd({ "git", "-C", dir, "rev-parse", "HEAD" })
  if not res.ok then return nil end
  return (res.stdout:gsub("%s+$", ""))
end

-- A successfully cloned plugin's marker — git always creates `.git`. We use
-- it as the "is this a real checkout?" sentinel rather than just relying on
-- the directory existing (the user might have an empty bin/ left over from a
-- failed install).
local function is_cloned(dir)
  local fs = require_fs()
  return fs.exists(pjoin(dir, ".git/HEAD")) or fs.exists(pjoin(dir, ".git"))
end

local function clone(label, spec, target_dir)
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
    git(label, { "-C", target_dir, "fetch", "--depth", "1", "origin", spec.ref })
    git(label, { "-C", target_dir, "checkout", spec.ref })
  end
end

local function update_to_ref(label, spec, target_dir)
  git(label, { "-C", target_dir, "fetch", "--depth", "1", "origin", spec.ref })
  if spec.ref_kind == "commit" then
    git(label, { "-C", target_dir, "checkout", spec.ref })
  else
    -- For branches/tags `origin/<ref>` follows the remote head after fetch.
    git(label, { "-C", target_dir, "checkout", spec.ref })
    git(label, { "-C", target_dir, "reset", "--hard", "FETCH_HEAD" })
  end
  if spec.path then
    git(label, { "-C", target_dir, "sparse-checkout", "set", spec.path })
  end
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
  if not fs.exists(subtree_dir) then return end

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

local function install_spec(spec, lock)
  local label = spec.name
  local fs = require_fs()

  if spec.dir then
    -- Dev override: place a symlink at <plugins_root>/<name> -> spec.dir so
    -- the canonical <plugins_root>/?/init.lua graft used by the clone path
    -- also resolves dir-overrides. Decouples basename(dir) from name.
    local root_mk = fs.mkdir_p(plugins_root())
    if not root_mk.ok then
      fail(label, "cannot create plugins root: " .. tostring(root_mk.error))
    end
    ensure_on_path(plugins_root())
    local link_path = pjoin(plugins_root(), spec.name)
    local existing_target
    if fs.is_symlink(link_path) then
      existing_target = fs.read_link(link_path)
    end
    if existing_target ~= spec.dir then
      if existing_target ~= nil then
        local rm = fs.remove(link_path)
        if not rm.ok then
          fail(label, "cannot remove stale symlink at " .. link_path .. ": " .. tostring(rm.error))
        end
      elseif fs.exists(link_path) then
        -- Non-symlink entry occupying the slot (e.g. leftover real clone from
        -- a previous non-dir install). Refuse rather than silently nuking
        -- whatever lives there.
        fail(label, string.format(
          "cannot create dev-override symlink at %s: a non-symlink entry already exists; remove it manually",
          link_path))
      end
      local sl = fs.symlink(spec.dir, link_path)
      if not sl.ok then
        fail(label, "symlink failed: " .. tostring(sl.error))
      end
    end
    plugins[spec.name] = { dir = spec.dir, source = "dir" }
    return lock[spec.name]
  end

  local target_dir = pjoin(plugins_root(), spec.name)
  local root_mk = fs.mkdir_p(plugins_root())
  if not root_mk.ok then
    fail(label, "cannot create plugins root: " .. tostring(root_mk.error))
  end
  -- Cloned plugins live as siblings under plugins_root(); add the parent
  -- once so subsequent require() calls resolve through Lua's own search.
  ensure_on_path(plugins_root())

  local entry = lock[spec.name]
  local build_hash = compute_build_hash(spec)

  -- Idempotency: same ref AND same build_hash AND clone exists → skip.
  local fresh_clone = false
  if not is_cloned(target_dir) then
    clone(label, spec, target_dir)
    fresh_clone = true
    if spec.path then
      flatten_subtree(label, target_dir, spec.path)
    end
  else
    local head_now = current_commit(target_dir)
    local need_update = true
    if entry and entry.ref == spec.ref and entry.commit and entry.commit == head_now then
      need_update = false
    end
    if need_update then
      update_to_ref(label, spec, target_dir)
      if spec.path then
        flatten_subtree(label, target_dir, spec.path)
      end
    end
  end

  local need_build = spec.build ~= nil and (
    fresh_clone
    or not entry
    or entry.build_hash ~= build_hash
  )

  if need_build then
    -- Build is responsible for placing artefacts at target_dir/bin/<name>.
    local bin_mk = fs.mkdir_p(pjoin(target_dir, "bin"))
    if not bin_mk.ok then
      fail(label, "cannot create bin dir: " .. tostring(bin_mk.error))
    end
    local plugin_record = {
      dir  = target_dir,
      name = spec.name,
      tag  = spec.ref,
      ref  = spec.ref,
      url  = spec.url,
      repo = spec.url,
    }
    local ok, err = pcall(spec.build, plugin_record)
    if not ok then
      fail(label, "build function raised: " .. tostring(err))
    end
  end

  plugins[spec.name] = { dir = target_dir, source = "data" }

  return {
    ref        = spec.ref,
    commit     = current_commit(target_dir),
    build_hash = build_hash,
  }
end

function M.install(specs)
  if not is_table(specs) then
    error("nefor-pm.install: specs must be a list of tables", 0)
  end
  local lock = read_lockfile()
  local new_lock = {}
  -- Preserve entries for plugins not mentioned in this install call (so a
  -- partial install doesn't wipe sibling lock state). Specs override.
  for k, v in pairs(lock) do new_lock[k] = v end

  for i, raw in ipairs(specs) do
    local spec = parse_spec(raw, i)
    local entry = install_spec(spec, lock)
    if entry ~= nil then
      new_lock[spec.name] = entry
    end
  end

  write_lockfile(new_lock)
end

-- pm.bin still uses the registry to find a plugin's dir (the binary's
-- location is a pm concern, not Lua's). pm.load delegates to require —
-- after pm.install has put the right parent dirs on package.path, Lua's
-- own resolution handles dotted names, init.lua entrypoints, and the
-- module → file mapping uniformly.
local function plugin_dir(name)
  local entry = plugins[name]
  if entry then return entry.dir end
  -- Fall back to the on-disk default location. Lets pm.bin work after a
  -- prior pm.install in another process, without re-registering everything.
  local fallback = pjoin(plugins_root(), name)
  local fs = require_fs()
  if is_cloned(fallback) or fs.exists(pjoin(fallback, "init.lua")) then
    plugins[name] = { dir = fallback, source = "data" }
    return fallback
  end
  return nil
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

function M.bin(name, binary_name)
  if not is_string(name) then
    error("nefor-pm.bin: name must be a non-empty string", 0)
  end
  binary_name = binary_name or name
  local dir = plugin_dir(name)
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
