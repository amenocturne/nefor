-- starter/generic-provider/init.lua — wrapper actor for the
-- generic-provider Rust binary.
--
-- Returns the actor spec directly. No CLI args; no adapter. Declares
-- canonical provider type tags against the combinators registry; the
-- wrapper has nothing to translate, so `receive_msg` is a no-op.

local config = require("config")

return {
  name        = "generic-provider",
  command     = { config.bin("generic-provider") },
  receive_msg = function(_entry) end,
}
