# External config bootstrap

This page is for **external** consumer configs (anything outside the
upstream `nefor` repo — e.g. `nefor-team`, personal customisation
configs) that want to depend on the upstream's plugin libraries and
Rust binaries via `nefor-pm`.

In-tree development inside this repo uses `dir`-overrides on every
`pm.install` spec and never hits the fetch path, so the bootstrap is
only relevant to configs that *don't* sit next to the upstream tree.

## The chicken-and-egg problem

`nefor-pm` is itself shipped in the upstream repo at
`lua/nefor-pm/init.lua`. An external config needs to call
`require("nefor-pm")` before it can declare any plugin specs — so the
pm has to already be on disk and on `package.path` before the
consumer's `init.lua` runs `pm.install({...})`.

The solution is the same shape lazy.nvim uses: a small bootstrap
snippet at the very top of the consumer's `init.lua` that clones a
sparse subtree of the upstream repo into `$NEFOR_DATA_DIR` if missing,
then puts the pm dir on `package.path`. After that the rest of
`init.lua` is plain composition.

## The snippet

```lua
-- Bootstrap nefor-pm. Runs once on first boot of a fresh machine;
-- a no-op on subsequent boots when the pm dir already exists.
local function bootstrap_pm()
  local data_dir = os.getenv("NEFOR_DATA_DIR")
                or (os.getenv("XDG_DATA_HOME") or (os.getenv("HOME") .. "/.local/share"))
                   .. "/nefor"
  local pm_root  = data_dir .. "/nefor"
  local pm_init  = pm_root .. "/lua/nefor-pm/init.lua"

  -- Lua 5.2+ returns (true|nil, "exit"|"signal", code); 5.1 returns
  -- the raw exit code. Capture once — re-checking by re-calling
  -- os.execute would re-run the command and `git clone` would then
  -- blow up on "already exists" right after a successful first run.
  local function run(cmd)
    local ok = os.execute(cmd)
    return ok == true or ok == 0
  end

  local f = io.open(pm_init, "r")
  if f then
    f:close()
  else
    -- First boot: sparse-clone just lua/nefor-pm. os.execute is fine
    -- here because nefor.process.spawn isn't wired yet (the binding
    -- exists, but the pm that uses it isn't loaded).
    os.execute("mkdir -p '" .. data_dir .. "'")
    local clone_cmd = "git clone --depth 1 --filter=blob:none --sparse "
                   .. "https://github.com/amenocturne/nefor.git '" .. pm_root .. "'"
    if not run(clone_cmd) then
      error("nefor bootstrap: git clone failed; check network + git availability")
    end
    if not run("git -C '" .. pm_root .. "' sparse-checkout set lua/nefor-pm") then
      error("nefor bootstrap: git sparse-checkout set failed")
    end
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
- Behaviour is idempotent: the existence check on `pm_init` short-circuits
  on every boot after the first.
- The engine binary must be installed separately — this bootstrap only
  fetches the *Lua* side of nefor. Use your platform's package manager
  for the engine itself (or build from source via `cargo install`).
- If you want a specific upstream tag for the pm itself (rather than
  default branch), change the `git clone` flags to add `--branch <tag>`.
  Most consumer configs are fine pinning their plugin specs to specific
  tags and letting the pm float on the upstream default branch.
