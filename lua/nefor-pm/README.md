# `nefor-pm` reference

`nefor-pm` is the synchronous Lua package/source manager used during configuration bootstrap. It manages materialized package roots and source checkouts; Lua, MAG, and executable consumers explicitly select what they use. It is not an engine subcommand.

## API

```lua
pm.install(specs [, opts])
pm.update(specs [, opts])
pm.stderr_progress(name, message)
pm.register(specs)
pm.root(name)
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
      plugin.progress("Compiling executable")
      -- Build from plugin.dir and place executables in plugin.dir .. "/bin".
    end,
  },
}

local lib = pm.load("my-plugin")
local executable = pm.bin("my-plugin", "my-plugin")
```

`path` uses sparse checkout and flattens the selected subtree into the managed package directory. Build callbacks must populate the package's `bin` directory. They rerun when checkout/pin/spec build metadata requires it; changing only the Lua function body is not detected by the current hash, so explicitly update/rebuild after such a change.

The plugin lock lives at `<data-root>/plugins/nefor-pm.lock.json`. `install` reproduces an existing exact pin and does not move it. `update` resolves again and moves the selected pins. Partial operations preserve unrelated lock entries.

### Startup progress

`install` and `update` accept an optional `on_progress(name, message)` callback.
Compositions can opt into flushed stderr messages before the TUI starts:

```lua
pm.install(specs, { on_progress = pm.stderr_progress })
```

The manager reports each download, fetch, checkout, or build phase before
starting its synchronous operation. Build callbacks receive
`plugin.progress(message)` for more specific phases, such as downloading a
model or invoking a compiler. `pm.stderr_progress` prefixes messages with the
package name and flushes stderr immediately; stdout stays available for the
frontend's output. These are phase notifications, not byte or percentage
progress from the underlying commands.

A package reports `Ready` after installation and lock persistence succeed.
Failures still raise the normal startup error and do not report `Ready`.
Already prepared packages emit no messages during `install`. Without an
`on_progress` callback, both the manager and `plugin.progress` stay quiet.

### Source modes

- **Managed:** repository/ref fields create a manager-owned checkout and lock entry.
- **Development override:** `dir = "/absolute/local/path"` creates a symlink to mutable source, performs no clone, and writes no lock entry. It refuses to replace a non-symlink.
- **Immutable registration:** `pm.register { { name = "x", dir = "/absolute/materialized/x" } }` changes only the current Lua resolver. It creates no checkout, symlink, or lock and refuses to rebind a name to another directory.

Use `register` for packaged immutable generations, managed install/update for package-manager state, and `dir` only for deliberate development overrides.

`pm.load`/`pm.require` call Lua `require`; they never install. `pm.bin` fails if the expected executable is absent.
`pm.root` returns the materialized directory for any registered or managed
package, including packages containing MAG modules or documentation instead of
Lua. An existing managed package remains resolvable in a later process without
replaying its install spec.

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
  sparse = { "lua", "examples/nefor-agent" },
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

## Runtime bootstrap

The installable agent example demonstrates a complete bootstrap in [`examples/nefor-agent/init.lua`](../../examples/nefor-agent/init.lua): it selects an explicit development or immutable runtime root, establishes `package.path`, and registers materialized module directories. External compositions should own their bootstrap policy and use the APIs above rather than copying version-specific snippets.

## Failure and trust model

Package refs and locks select code that runs inside the engine's Lua process and may build executables. Pin trusted sources. A build callback is arbitrary config code, not a sandbox. Network/Git/build failures are synchronous startup failures rather than silently falling back to another revision.

Implementation and exhaustive edge-case tests live in [`lua/nefor-pm/init.lua`](init.lua) and `engine/tests/nefor_pm_test.rs`.
