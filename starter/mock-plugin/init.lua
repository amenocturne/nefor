-- starter/mock-plugin/init.lua — wrapper actor for the mock-plugin
-- Rust binary.
--
-- The mock plugin speaks the same provider wire-protocol as
-- openai-provider (`<prefix>.chat.create`, `<prefix>.stream.delta`, …)
-- so the wrapper composition is identical: we delegate to the
-- openai-provider wrapper's `spawn_spec` constructor with whatever
-- `prefix` the test/CI configuration uses (typically `mock-plugin`).
--
-- This shim exists so `init.lua` can write
-- `actor.spawn(require("mock-plugin").spawn_spec(...))` symmetrically
-- with the real provider, without forcing the active config to know
-- which one it's wiring.

local openai_provider = require("openai-provider")

local M = {}

function M.spawn_spec(name, command, opts)
  return openai_provider.spawn_spec(name, command, opts)
end

return M
