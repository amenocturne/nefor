-- starter/reasoners/loop_counter.lua — `loop-counter` guard reasoner.
--
-- A trivial counter that tracks per-cycle firings against a configured
-- limit. Used to guard graph loops (retry-up-to-N, refine-up-to-N, …)
-- by emitting a typed `exceeded = true` flag once the limit is
-- crossed. The graph topology decides what to do when `exceeded` is
-- true — typically wires to an error-emit node that surfaces a typed
-- error (e.g. `"loop-X-Y exceeded N iterations"`) to the orchestrator.
--
-- Retries are graph nodes, not internal reasoner state (per
-- lead-workflow-spec §2). This reasoner is the building block that
-- makes "place a counter at the loop boundary" expressible without
-- the agent reasoner growing a `maxAttempts` knob.
--
-- ## Dispatch envelope
--
--   tool.invoke {
--     id   = <firing_id>,
--     name = "loop-counter",
--     args = {
--       run_id, node_id,
--       args = {
--         limit = <int>,            -- required, > 0
--         key   = <string?>,        -- optional label; surfaces in
--                                   -- the error-message convention
--       },
--       inputs     = { ... },       -- ignored — counter is stateless
--                                   -- across upstreams; the only state
--                                   -- it tracks is its own re-fires.
--       prev_state = nil | { count = <int> },
--     }
--   }
--
-- ## Reply envelope
--
--   tool.result {
--     id     = <firing_id>,
--     result = {
--       count      = <int>,        -- post-increment count for THIS firing
--       exceeded   = <bool>,       -- count > limit
--       key        = <string?>,    -- echoed back for downstream error-emit
--       limit      = <int>,        -- echoed back for downstream error-emit
--       next_state = { count = <int> },
--     }
--   }
--
-- ## State threading
--
-- `prev_state` arrives as serde_json `null` on first firing (decoded
-- to lightuserdata NULL by mlua, NOT Lua nil). The first-firing test
-- is `type(prev_state) ~= "table"` — same as `provider_run_node` at
-- reasoners/init.lua:185. On re-fire `prev_state.count` carries the
-- previous count; the per-`key` distinction is automatic per-node
-- (each reasoner-graph node has its own state cell), so two counter
-- nodes with different `key`s in the same graph don't collide
-- mechanically. `key` is preserved for the downstream error-emit node
-- to format a meaningful diagnostic.

local envelope = require("lib.envelope")

local emit_as = envelope.emit_as

local M = {}

-- ------------------------------------------------------------------
-- dispatch handler — called from reasoners/init.lua
-- ------------------------------------------------------------------

-- Returns "_already_replied" because we emit tool.result synchronously;
-- reasoners/init.lua's err path is bypassed.
local function handle(body)
  local firing_id = body.firing_id
  local args = body.args or {}
  local limit = args.limit
  local key   = args.key
  local prev_state = body.prev_state

  if type(limit) ~= "number" or limit < 1 then
    emit_as("loop-counter", nil, {
      kind  = "tool.result",
      id    = firing_id,
      error = "loop-counter reasoner: args.limit must be a positive integer",
    })
    return "_already_replied"
  end

  local prev_count = 0
  if type(prev_state) == "table" and type(prev_state.count) == "number" then
    prev_count = prev_state.count
  end
  local count = prev_count + 1
  local exceeded = count > limit

  local result = {
    count      = count,
    exceeded   = exceeded,
    limit      = limit,
    next_state = { count = count },
  }
  if type(key) == "string" then
    result.key = key
  end

  emit_as("loop-counter", nil, {
    kind   = "tool.result",
    id     = firing_id,
    result = result,
  })

  return "_already_replied"
end

M.handle = handle

return M
