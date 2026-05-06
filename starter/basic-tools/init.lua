-- starter/basic-tools/init.lua — wrapper actor for the basic-tools
-- Rust binary.
--
-- Returns the actor spec directly. Binary name and CLI args are
-- intrinsic to this plugin and live here; path resolution is the
-- active config's concern (`require("config").bin`).
--
-- `receive_msg` is a no-op: basic-tools needs no adapter. Its wire
-- output speaks the canonical tool contract that tool-gate consumes
-- directly. The Rust binary participates on the bus through the
-- broker's stdin/stdout pipes.

local config = require("config")

return {
  name        = "basic-tools",
  command     = { config.bin("basic-tools"), "--gate", "tool-gate" },
  receive_msg = function(_entry) end,
}
