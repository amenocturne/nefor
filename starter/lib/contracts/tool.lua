-- starter/lib/contracts/tool.lua — tool wire-protocol contract.
--
-- Owns the canonical type tags every tool-shaped reasoner ecosystem
-- agrees on. Replaces the `generic-tool` Rust binary's role as a
-- passive type-registry hub: instead of a separate process whose only
-- job is to send `combinators.register` on startup, we declare the same
-- types from Lua against `nefor-combinators`.
--
-- ## Types
--
-- - `generic-tool.ToolCalls`   — list of tool invocations a provider
--   asked for (the fanout target slot for `tool_split`).
-- - `generic-tool.ToolResults` — list of tool execution outcomes that
--   feeds back into the provider on the next firing.
--
-- Concrete tool sources (basic-tools, mock-plugin's tool layer, …)
-- declare `Into<generic-tool.ToolCalls, <them>.RawCalls>` and
-- `Into<<them>.RawResults, generic-tool.ToolResults>` against
-- combinators, referring to these tags by their fully-qualified names.
--
-- ## How `declare()` works
--
-- See `lib/contracts/provider.lua`'s docstring — the same shape applies
-- here, namespaced under `generic-tool`.

local envelope = require("lib.envelope")

local FROM = "generic-tool"

local M = {}

-- Canonical type-name constants.
M.TOOL_CALLS   = FROM .. ".ToolCalls"
M.TOOL_RESULTS = FROM .. ".ToolResults"

-- Bare-name list emitted in `combinators.register`'s `types[]`.
local BARE_TYPES = { "ToolCalls", "ToolResults" }

local function emit_register()
  -- `implementations` omitted: this hub-namespace owns no combinator
  -- handlers, and `parse_register_body` treats a missing field as an
  -- empty array. Sending `implementations = {}` from Lua serializes to
  -- a JSON object (empty table → object) which the Rust side rejects.
  envelope.emit_as(FROM, nil, {
    kind  = "combinators.register",
    types = BARE_TYPES,
  })
end

local declared = false

-- Idempotent: emit once. See lib/contracts/provider.lua's docstring —
-- the emission lands as a step-origin LogEntry; combinators sees it via
-- ncp.lua's replay-on-attach when it readies.
function M.declare()
  if declared then return end
  declared = true
  emit_register()
end

-- Test escape hatch.
function M._reset()
  declared = false
end

return M
