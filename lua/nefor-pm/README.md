# External config bootstrap

This page is for **external** consumer configs (anything outside the
upstream `nefor` repo — e.g. `nefor-team`, personal customisation
configs) that want to depend on the upstream's plugin libraries and
Rust binaries via `nefor-pm`.

In-tree development inside this repo uses `dir`-overrides on every
`pm.install` spec and never hits the fetch path, so the bootstrap is
only relevant to configs that _don't_ sit next to the upstream tree.

## The chicken-and-egg problem

`nefor-pm` is itself shipped in the upstream repo at
`lua/nefor-pm/init.lua`. An external config needs to call
`require("nefor-pm")` before it can declare any plugin specs — so the
pm has to already be on disk and on `package.path` before the
consumer's `init.lua` runs `pm.install({...})`.

The solution is the same shape lazy.nvim uses: a small bootstrap
snippet at the very top of the consumer's `init.lua` that ensures a
sparse subtree of the upstream repo exists at the ref implied by the
engine version, then puts the pm dir on `package.path`. After that the
rest of `init.lua` is plain composition.

## The snippet

```lua
-- Bootstrap nefor-pm. A release binary (nefor.version = "X.Y.Z") always
-- runs against upstream tag vX.Y.Z; dev/nightly builds use main.
local function bootstrap_pm()
  local data_dir = os.getenv("NEFOR_DATA_DIR")
                or (os.getenv("XDG_DATA_HOME") or (os.getenv("HOME") .. "/.local/share"))
                   .. "/nefor"
  local pm_root  = data_dir .. "/nefor"
  local pm_init  = pm_root .. "/lua/nefor-pm/init.lua"
  local upstream_ref = (nefor and nefor.version and nefor.version:match("^%d+%.%d+%.%d+$"))
                    and ("v" .. nefor.version)
                    or "main"

  -- Lua 5.2+ returns (true|nil, "exit"|"signal", code); 5.1 returns
  -- the raw exit code. Capture once — re-checking by re-calling
  -- os.execute would re-run the command and `git clone` would then
  -- blow up on "already exists" right after a successful first run.
  local function run(cmd)
    local ok = os.execute(cmd)
    return ok == true or ok == 0
  end
  local function sh_quote(s)
    return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
  end
  local function fetch_ref()
    if upstream_ref:match("^v%d+%.%d+%.%d+$") then
      return "tag " .. sh_quote(upstream_ref)
    end
    return sh_quote(upstream_ref)
  end

  local f = io.open(pm_init, "r")
  if f then
    f:close()
  else
    os.execute("mkdir -p '" .. data_dir .. "'")
    local clone_cmd = "git clone --depth 1 --filter=blob:none --sparse "
                   .. "--branch " .. sh_quote(upstream_ref) .. " "
                   .. "https://github.com/amenocturne/nefor.git " .. sh_quote(pm_root)
    if not run(clone_cmd) then
      error("nefor bootstrap: git clone failed; check network + git availability")
    end
  end
  if not run("git -C " .. sh_quote(pm_root) .. " fetch --depth 1 origin " .. fetch_ref()) then
    error("nefor bootstrap: git fetch failed for " .. upstream_ref)
  end
  if not run("git -C " .. sh_quote(pm_root) .. " checkout --force FETCH_HEAD") then
    error("nefor bootstrap: git checkout failed for " .. upstream_ref)
  end
  if not run("git -C " .. sh_quote(pm_root) .. " sparse-checkout set lua/nefor-pm") then
    error("nefor bootstrap: git sparse-checkout set failed")
  end

  package.path = table.concat({
    pm_root .. "/lua/?.lua",
    pm_root .. "/lua/?/init.lua",
    package.path,
  }, ";")
end

bootstrap_pm()

local pm = require("nefor-pm")

pm.install({
  { "amenocturne/nefor", name = "core",
    tag = "v0.1.5", path = "lua/core/" },

  { "amenocturne/nefor", name = "libs",
    tag = "v0.1.5", path = "lua/libs/" },

  { "amenocturne/nefor", name = "openai-provider",
    tag = "v0.1.5", path = "plugins/openai-provider/lua/openai-provider/" },

  { "amenocturne/nefor", name = "tool-gate",
    tag = "v0.1.5", path = "plugins/tool-gate/lua/tool-gate/" },

  -- ... your other plugin specs, including Rust-binary specs with a
  -- `build = function(plugin) ... end` callback if you need to compile
  -- binaries from source on the consumer machine.
})

-- After pm.install returns, plugin libs are on package.path so plain
-- `require("openai-provider")` resolves to the upstream plugin lib.
-- Compose your config below as plain Lua.
```

## Notes

- `os.execute` is fine for the bootstrap step because it runs once,
  before `nefor.process.spawn` is reachable through the (not-yet-loaded)
  pm. Subsequent fetches inside `pm.install` use the Rust-backed
  `nefor.process.spawn` binding via `handle:wait()` and don't shell out.
- The bootstrap path uses `git` directly — same cross-platform constraint
  the rest of the pm honours (`git` is the only binary the bootstrap
  invokes, and it's available on every developer machine).
- Behaviour is version-synchronising: a cached checkout is fetched and
  force-checked-out to the ref selected from `nefor.version` on each boot.
- The engine binary must be installed separately — this bootstrap only
  fetches the _Lua_ side of nefor. Use your platform's package manager
  for the engine itself (or build from source via `cargo install`).
- After `nefor-pm` is loaded, use `pm.sync_checkout({...})` for any other
  managed upstream checkout that must stay aligned with a branch, tag, or
  commit.
