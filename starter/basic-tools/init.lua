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
--
-- ## Phase 2: tool-contract declaration
--
-- basic-tools is a tool consumer (advertises tools to the gate, emits
-- tool results). It depends on the canonical `generic-tool.ToolCalls`/
-- `generic-tool.ToolResults` type tags being declared against
-- nefor-combinators. We declare them from Lua via the tool contract
-- lib instead of relying on the (now-deleted) generic-tool Rust
-- binary's startup envelope. `declare()` is idempotent and timing-safe
-- (eagerly emits at load; combinators picks up the registration via
-- ncp.lua's replay-on-attach when it readies).

local config        = require("config")
local tool_contract = require("lib.contracts.tool")

tool_contract.declare()

return {
  name        = "basic-tools",
  command     = { config.bin("basic-tools"), "--gate", "tool-gate" },
  receive_msg = function(_entry) end,
}
