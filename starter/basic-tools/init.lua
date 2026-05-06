-- starter/basic-tools/init.lua — wrapper actor for the basic-tools
-- Rust binary.
--
-- ## Shape
--
-- Module is a constructor: `require("basic-tools")(config)` returns the
-- actor spec. The caller (`init.lua`) supplies path resolution and the
-- gate name; the plugin doesn't reach for env vars or globals so it
-- composes uniformly across configs (starter, cli-config, custom
-- harnesses) that resolve binaries differently.
--
-- `receive_msg` is a no-op because basic-tools needs no adapter — its
-- wire output already speaks the canonical tool contract that tool-gate
-- consumes directly. The Rust binary participates on the bus through
-- the broker's stdin/stdout pipes.

---@param config { bin: string, gate: string? }
return function(config)
  return {
    name        = "basic-tools",
    command     = { config.bin, "--gate", config.gate or "tool-gate" },
    receive_msg = function(_entry) end,
  }
end
