-- starter/generic-tool/init.lua — wrapper actor for the generic-tool
-- Rust binary.
--
-- Returns the actor spec directly. No CLI args; no adapter. Declares
-- canonical tool type tags against the combinators registry; the
-- wrapper has nothing to translate, so `receive_msg` is a no-op.

local config = require("config")

return {
  name        = "generic-tool",
  command     = { config.bin("generic-tool") },
  receive_msg = function(_entry) end,
}
