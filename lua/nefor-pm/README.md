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
    commit = "<author-chosen full 40-hex commit>",
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

`path` uses sparse checkout and flattens the selected subtree into the managed package directory. Build callbacks must populate the package's `bin` directory. They run in a staging checkout before replacement. Changing only a Lua callback body is not detectable from the declaration; use `update` to explicitly rebuild the same identity.

The plugin lock lives at `<data-root>/plugins/nefor-pm.lock.json`. The declaration, not the lock, chooses the desired immutable identity. `install` reconciles changed declarations, including explicit downgrades. `update` rematerializes and rebuilds the SAME declared identity; it never discovers or selects another version. Partial operations preserve unrelated lock entries.

### Prebuilt archives

A package can select a verified tar.gz archive instead of a Git checkout:

```lua
pm.install {
  {
    name = "my-tool",
    archive = {
      url = "https://example.org/releases/my-tool.tar.gz",
      sha256 = "<64 hexadecimal characters>",
      strip_components = 1,
    },
  },
}
local executable = pm.bin("my-tool", "my-tool")
```

Archive specs cannot also select Git, `dir`, `path`, or `build`. The archive
must contain the final package layout (including `bin/` for executables).
Installation uses the external `lua-http` library through ordinary Lua `require`,
plus `tar` and either `shasum` or `sha256sum`; no compiler, `curl`, or source
checkout is used during archive installation. The composition supplies the URL and SHA-256.

`install` adopts the declared URL/checksum/layout even when a different identity is recorded in the lock; `update` explicitly refreshes that declared identity. A cached package with the matching lock and installation
marker does not download or extract again. Download, checksum, extraction, verification, build, or
lock-write failures preserve the previous package and lock. After successful
lock persistence, a cleanup failure reports the leftover staging path without
undoing or misreporting the installed package. These guarantees cover reported
operation failures, not process termination or simultaneous writers.

Archives can replace an existing managed Git package with a lock entry.
Unowned directories and development symlinks are never replaced. Git and archive installs both stage the new package before replacing an owned directory, including when switching between these source kinds. Packages remain under `<data-root>/plugins/<name>`.

The shell classifier helper `libs.tool-validator.da.release_package` produces
this spec from a composition-owned `version` and a `checksums` table keyed by
release target. It supports Apple Silicon macOS and GNU Linux on x86_64/ARM64,
and fails explicitly on unsupported platforms or missing checksums. The
separate `da.package { commit = ... }` helper remains an explicit source-build
choice requiring Rust and Git LFS.

### Verification and results

`install` and `update` return a table keyed by package name. Each value is
`{ dir, identity, verification }`: the final directory, declared normalized
identity, and optional current verifier evidence. Git identity is
`{ kind = "git", url, commit, path? }`; archive identity is the normalized
archive record; development identity is `{ kind = "dir", dir }`.

An optional `verify = function(package) ... end` receives
`{ name, dir, identity }`. It must raise on mismatch and may return JSON-serializable
evidence. It runs on staged packages before replacement, again at the final
path, and on matching cached packages before success. Verifiers must be read-only;
they do not select versions or install dependencies. Locks and markers establish
cache identity, not executable correctness; a configured verifier still runs
offline and rejects a replaced, absent, or wrong-version binary.

`da.release_package` attaches a verifier for its caller-owned version.
`da.verify_version(path, expected)` requires successful `--version` output exactly
`da X.Y.Z` (apart from surrounding whitespace) and returns
`{ version, executable }`. `da.package` can opt into that contract with an
explicit `version`; a source commit alone does not imply a release version.
The validator's optional `classifier_check(path)` can reuse this helper at its
first-use cached probe, before classification, without downloading anything.

Immutable `register` roots remain read-only and cannot be rebound by an install.
Managed operations refuse unowned directories or development symlinks. Existing
Git lock records without repository/subtree identity trigger one staged
reconciliation rather than silently inferring identity from the path. A matching
prepared identity needs no fetch; a changed identity unavailable offline fails
with desired and recorded identities, preserving the old package and lock.

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

- **Managed Git:** repository/full-commit fields create a manager-owned checkout and lock entry.
- **Managed archive:** an `archive` record selects a checksum-verified tar.gz package with an exact lock.
- **Development override:** `dir = "/absolute/local/path"` creates a symlink to mutable source, performs no clone, and writes no lock entry. It refuses to replace a non-symlink.
- **Immutable registration:** `pm.register { { name = "x", dir = "/absolute/materialized/x" } }` changes only the current Lua resolver. It creates no checkout, symlink, or lock and refuses to rebind a name to another directory.

Use `register` for packaged immutable generations, managed install/update for package-manager state, and `dir` only for deliberate development overrides.

`pm.load`/`pm.require` call Lua `require`; they never install. `pm.bin` fails if the expected executable is absent.
`pm.root` returns the materialized directory for any registered or managed
package, including packages containing MAG modules or documentation instead of
Lua. An existing managed package remains resolvable in a later process without
replaying its install spec.

## Version-derived refs

`pm.engine_ref()` maps an exact engine semantic version to `v<version>`. Development, nightly, dirty, and described builds map to `main`. This is an informational helper only. Managed installs and checkouts never use it as a default: missing full commits fail before fetching. Tags and branches must be resolved by the author to an immutable commit before declaring a managed package.

## Managed source checkouts

Use this API when a distribution needs an exact source generation rather than a flattened plugin package:

```lua
local checkout = pm.sync_checkout {
  name = "nefor-runtime",
  dir = data_root .. "/runtime/nefor",
  url = "https://github.com/amenocturne/nefor.git",
  ref = "<author-chosen full 40-hex commit>",
  ref_kind = "commit",
  lockfile = data_root .. "/runtime/nefor.commit",
  sparse = { "lua", "examples/nefor-agent" },
}

print(checkout.commit)
```

Options:

- `name`: diagnostic label;
- `dir`: checkout destination;
- `url`: Git URL or local repository path;
- `ref`: required full 40-hex commit;
- `ref_kind`: required `"commit"`;
- `lockfile`: one-commit text lock;
- `sparse`: one path or an array of paths.

`sync_checkout` adopts the declared repository/commit/sparse identity, regardless of the recorded lock. `update_checkout` refreshes that same declaration. The checkout is staged and verified before replacement, and a reported lock-write failure restores the previous checkout. The returned record includes the requested ref/ref kind and verified `commit`/`head`. Local Git sources and unpublished local commits are supported.

## Runtime bootstrap

The installable agent example demonstrates a complete bootstrap in [`examples/nefor-agent/init.lua`](../../examples/nefor-agent/init.lua): it selects an explicit development or immutable runtime root, establishes `package.path`, and registers materialized module directories. External compositions should own their bootstrap policy and use the APIs above rather than copying version-specific snippets.

## Failure and trust model

Package declarations select code that runs inside the engine's Lua process and may build executables. Pin trusted sources. A build callback is arbitrary config code, not a sandbox. Network/Git/build failures are synchronous startup failures rather than silently falling back to another revision.

Implementation and exhaustive edge-case tests live in [`lua/nefor-pm/init.lua`](init.lua) and `engine/tests/nefor_pm_test.rs`.

## External Lua dependencies

The library-owned `nefor-pm-dev-1.rockspec` declares Lua 5.4 and `http` 0.4.
A distribution or configuration installer prepares its dependency tree before
startup and selects its ordinary `package.path` and `package.cpath`. The engine
neither selects nor installs these dependencies. Another manager can use the
same standard Lua loading mechanisms without depending on nefor-pm.

Archive downloads load `http.request` lazily: Git packages, immutable
registrations, and cached archive installs do not require the HTTP library.
Downloads use lua-http's normal TLS verification, proxy discovery, and bounded
redirect behavior. Transfers select HTTP/1.1 and detach unfinished streams
before closing them, avoiding the pinned library's truncated-body shutdown loop.
A supplied Content-Length is checked before archive checksum verification.
HTTP errors and incomplete bodies fail installation, which removes the staging directory and preserves the previous package and lock.

Preparing the initial tree is an installer/build responsibility; nefor-pm does
not attempt to download its own absent HTTP transport. Reproducible distributions
must pin the resolved dependency closure and build it for the selected platform
and Lua 5.4 ABI, rather than relying on future dependency resolution.
