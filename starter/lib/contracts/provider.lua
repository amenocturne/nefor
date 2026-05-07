-- starter/lib/contracts/provider.lua — provider wire-protocol contract.
--
-- Owns the canonical type tags every provider-shaped reasoner ecosystem
-- agrees on. Replaces the `generic-provider` Rust binary's role as a
-- passive type-registry hub: instead of a separate process whose only
-- job is to send `combinators.register` on startup, we declare the same
-- types from Lua against `nefor-combinators`.
--
-- ## Types
--
-- - `generic-provider.ProviderIn`  — chat-completion request shape
-- - `generic-provider.ProviderOut` — chat-completion response shape
-- - `generic-provider.ChatHistory` — provider-shaped reasoner state
-- - `generic-provider.NoState`     — unit/empty state for stateless reasoners
-- - `generic-provider.FinalAnswer` — escape-edge type emitted by `tool_split`
--
-- Concrete provider plugins (openai-provider, mock-plugin, …) declare
-- `Into<generic-provider.ProviderIn, <them>.RawRequest>` and
-- `Into<<them>.RawResponse, generic-provider.ProviderOut>` against
-- combinators, referring to these tags by their fully-qualified names.
--
-- ## How `declare()` works
--
-- The combinators registry namespaces a registration by `envelope.from`.
-- We emit with `from = "generic-provider"` so combinators stores the
-- types under the canonical hub-namespace. (See lib/envelope.emit_as.)
--
-- Timing: combinators is a Rust binary that needs its own ready
-- handshake before it can receive events. `declare()` emits the
-- registration eagerly at module load, before combinators is spawned;
-- when combinators readies, ncp.lua's replay-on-attach delivers the
-- prior step-origin emission to it. (Step entries log as bus events
-- post-deliver/send split; replay carries them to late attachers.)

local envelope = require("lib.envelope")

local FROM = "generic-provider"

local M = {}

-- Canonical type-name constants. Bare names are what concrete plugins
-- list in their declared `types[]`; dotted forms are what they reference
-- cross-namespace (e.g. in graph edges or `Into.in`/`out`).
M.PROVIDER_IN   = FROM .. ".ProviderIn"
M.PROVIDER_OUT  = FROM .. ".ProviderOut"
M.CHAT_HISTORY  = FROM .. ".ChatHistory"
M.NO_STATE      = FROM .. ".NoState"
M.FINAL_ANSWER  = FROM .. ".FinalAnswer"

-- Bare-name list emitted in `combinators.register`'s `types[]`.
local BARE_TYPES = {
  "ProviderIn",
  "ProviderOut",
  "ChatHistory",
  "NoState",
  "FinalAnswer",
}

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

-- Idempotent: emit once. Combinators handles re-registration gracefully
-- (atomic replace per sender), but no point spamming.
function M.declare()
  if declared then return end
  declared = true
  emit_register()
end

-- Test escape hatch: re-arm the emission. Production code should not
-- call this.
function M._reset()
  declared = false
end

return M
