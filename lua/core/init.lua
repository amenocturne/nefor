-- core — aggregator for the multi-consumer protocol primitives.
--
-- Two consumption patterns are both supported by Lua's package
-- resolution after `lua/?.lua;lua/?/init.lua` is on package.path:
--
--   * Bundle:    local core = require("core")
--                core.envelope, core.ncp, core.actor, ...
--   * Granular:  local envelope = require("core.envelope")
--
-- `core/` holds the primitives every consumer of the bus depends on:
-- envelope construction, NCP protocol, the actor runtime, ID minting,
-- history-replay helpers (which also own the replay-window flag).
-- Independent generic libs live under `libs/`; plugin-specific helpers
-- live alongside the plugin.

local M = {}
M.envelope       = require("core.envelope")
M.ncp            = require("core.ncp")
M.actor          = require("core.actor")
M.ids            = require("core.ids")
M.history_replay = require("core.history_replay")
M.event          = require("core.event")
return M
