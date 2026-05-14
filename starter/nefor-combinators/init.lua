-- starter/nefor-combinators/init.lua — wrapper actor for the
-- nefor-combinators Rust binary.
--
-- Returns the actor spec directly. No CLI args; no adapter. The Rust
-- binary participates on the bus through the broker's stdin/stdout
-- pipes, and `receive_msg` is a no-op because the wrapper has nothing
-- to translate.

local config = require("config")

return {
  name        = "nefor-combinators",
  command     = { config.bin("nefor-combinators") },
  receive_msg = function(_entry) end,
}
