# `nefor-pm` reference

`nefor-pm` is the synchronous Lua package/source manager used during configuration bootstrap. It manages Lua module roots and source checkouts; it is not an engine subcommand.

## API

```lua
pm.install(specs)
pm.update(specs)
pm.register(specs)
pm.load(name)
pm.require(name)                 -- alias of load
pm.bin(name [, binary_name])
pm.sync_checkout(opts)
pm.update_checkout(opts)
pm.engine_ref()
```

## Managed plugin specs

```lua
local pm = require("nefor-pm")

pm.install {
  {
    "owner/repository",
    name = "my-plugin",
    tag = "v0.4.0",              -- choose only one of tag/branch/commit
    -- branch = "main",
    -- commit = "full-or-resolvable-sha",
    -- url = "/alternate/git/source",
    path = "plugins/my-plugin/lua/my-plugin/",
    build = function(plugin)
      -- Build from plugin.dir and place executables in plugin.bin_dir.
    end,
  },
}

local lib = pm.load("my-plugin")
local executable = pm.bin("my-plugin", "my-plugin")
```

`path` uses sparse checkout and flattens the selected subtree into the managed package directory. Build callbacks must populate the package's `bin` directory. They rerun when checkout/pin/spec build metadata requires it; changing only the Lua function body is not detected by the current hash, so explicitly update/rebuild after such a change.

The plugin lock lives at `<data-root>/plugins/nefor-pm.lock.json`. `install` reproduces an existing exact pin and does not move it. `update` resolves again and moves the selected pins. Partial operations preserve unrelated lock entries.

### Source modes

- **Managed:** repository/ref fields create a manager-owned checkout and lock entry.
- **Development override:** `dir = "/absolute/local/path"` creates a symlink to mutable source, performs no clone, and writes no lock entry. It refuses to replace a non-symlink.
- **Immutable registration:** `pm.register { { name = "x", dir = "/absolute/materialized/x" } }` changes only the current Lua resolver. It creates no checkout, symlink, or lock and refuses to rebind a name to another directory.

Use `register` for packaged immutable generations, managed install/update for package-manager state, and `dir` only for deliberate development overrides.

`pm.load`/`pm.require` call Lua `require`; they never install. `pm.bin` fails if the expected executable is absent.

## Version-derived refs

`pm.engine_ref()` maps an exact engine semantic version to `v<version>`. Development, nightly, dirty, and described builds map to `main`. An external distribution can override this policy by supplying an explicit ref/pin and owning compatibility.

## Managed source checkouts

Use this API when a distribution needs an exact source generation rather than a flattened plugin package:

```lua
local checkout = pm.sync_checkout {
  name = "nefor-runtime",
  dir = data_root .. "/runtime/nefor",
  url = "https://github.com/amenocturne/nefor.git",
  ref = pm.engine_ref().ref,
  ref_kind = pm.engine_ref().ref_kind,
  lockfile = data_root .. "/runtime/nefor.commit",
  sparse = { "lua", "starter" },
}

print(checkout.commit)
```

Options:

- `name`: diagnostic label;
- `dir`: checkout destination;
- `url`: Git URL or local repository path;
- `ref`: branch, tag, or commit (defaults from `engine_ref`);
- `ref_kind`: `branch`, `tag`, or `commit`;
- `lockfile`: one-commit text lock;
- `sparse`: one path or an array of paths.

With a lockfile, `sync_checkout` treats the pinned commit as authoritative and verifies the checkout. `update_checkout` is the operation that resolves the ref again and moves the lock. The returned record includes the requested ref/ref kind and verified `commit`/`head`. Local Git sources and unpublished local commits are supported.

## Current starter bootstrap

Do not copy the historical bootstrap snippet from `lua/nefor-pm/README.md`; its pinned examples predate v0.4.0. The canonical complete bootstrap is the beginning of [`starter/init.lua`](../../starter/init.lua):

1. accept a valid `NEFOR_DEV_DIR` mutable checkout;
2. otherwise accept a valid `NEFOR_RUNTIME_ROOT` immutable generation;
3. otherwise synchronize version-derived source under the data root;
4. establish `package.path`;
5. register materialized core/library/plugin module directories with `pm.register`.

This avoids a second competing bootstrap authority and keeps runtime source identity aligned with the engine/distribution.

## Failure and trust model

Package refs and locks select code that runs inside the engine's Lua process and may build executables. Pin trusted sources. A build callback is arbitrary config code, not a sandbox. Network/Git/build failures are synchronous startup failures rather than silently falling back to another revision.

Implementation and exhaustive edge-case tests live in [`lua/nefor-pm/init.lua`](../../lua/nefor-pm/init.lua) and `engine/tests/nefor_pm_test.rs`.
