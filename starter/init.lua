-- starter/init.lua — default engine composition.
--
-- Post Slice 2 I4 the engine is pure glue: no hardcoded NCP behavior, no
-- bundled widgets. This file is the canonical reference config:
--
--   1. Wire `package.path` so `require("ncp")` + `require("lib.json")`
--      resolve to the bundled Lua modules next to this file.
--   2. Optionally declare a parent session id to resume from (commented out
--      by default — uncomment and fill in a uuid to continue a prior run).
--   3. Define the global `step` hook the engine calls on every inbound line.
--      Delegates to `ncp.step` — the protocol module is where the NCP v0.1
--      semantics live.
--   4. Register plugins via `nefor.plugins.spawn`. Mirrors the pre-split
--      reference config (`tmp/smoke-config-m2/init.lua`) plus the
--      combinators plugin; swap or remove entries to compose your own stack.
--
-- This replaces the legacy MVP config (single-process Lua widgets) that
-- lived at this path pre-Slice-2. The widget model is now plugin-land
-- (`nefor-tui`, `nefor-chat`, ...); the starter's job is just composition.
--
-- Run:
--   NEFOR_PLUGIN_DIR=$PWD/plugins cargo run --bin nefor -- --config ./starter

-------------------------------------------------------------------------
-- 1. Lua module path — bundled protocol + json alongside this file
-------------------------------------------------------------------------
-- STARTER_ROOT points at the directory this init.lua lives in. We derive
-- it from the chunk name at load time so the user doesn't need to set an
-- env var. The engine loads init.lua with `set_name(<abs path>)`, and
-- `debug.getinfo(1).source` returns that path prefixed with "@".
local function starter_root()
  local src = debug.getinfo(1, "S").source
  if src:sub(1, 1) == "@" then src = src:sub(2) end
  -- Strip the trailing "/init.lua".
  return src:match("^(.*)/[^/]+$") or "."
end

local STARTER_ROOT = starter_root()

package.path = table.concat({
  STARTER_ROOT .. "/?.lua",
  STARTER_ROOT .. "/?/init.lua",
  package.path,
}, ";")

-------------------------------------------------------------------------
-- 2. Optional parent session id (resume a prior run)
-------------------------------------------------------------------------
-- Uncomment and set to a previous session's UUID (printed in the engine
-- log at startup) to hydrate `saved_log` on the next run. `saved_log` is
-- currently ignored by `ncp.step` — see `ncp.lua` for why.
--
-- nefor.parent_session = "00000000-0000-0000-0000-000000000000"

-------------------------------------------------------------------------
-- 3. Step function
-------------------------------------------------------------------------
local ncp = require("ncp")

function step(saved_log, current_log)
  ncp.step(saved_log, current_log)
end

-------------------------------------------------------------------------
-- 4. Plugin composition
-------------------------------------------------------------------------
-- Paths match the default `--plugin-dir` layout: <plugin_root>/<name>/.
-- Adjust per-plugin `command` entries if you've installed plugins
-- elsewhere or want to run release builds.
--
-- `ncp.spawn` accepts everything `nefor.plugins.spawn` does plus optional
-- `from_plugin` / `to_plugin` envelope transforms. See `ncp.lua` for the
-- contract and `mock_plugin_adapter.lua` for the worked example: it adapts
-- mock-plugin's `cc.*` namespace to nefor-chat's `chat-contract v0.1`.

local cc_adapter = require("mock_plugin_adapter")

ncp.spawn {
  name        = "mock-plugin",
  command     = { "../target/debug/mock-plugin" },
  from_plugin = cc_adapter.from_plugin,
  to_plugin   = cc_adapter.to_plugin,
}

ncp.spawn {
  name    = "nefor-chat",
  command = { "../target/debug/nefor-chat" },
}

ncp.spawn {
  name    = "nefor-tui",
  command = { "../target/debug/nefor-tui" },
}

ncp.spawn {
  name    = "nefor-combinators",
  command = { "../target/debug/nefor-combinators" },
}
