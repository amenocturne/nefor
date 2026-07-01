-- lua/core/combinator_shim.lua — Lua-side shim for combinators.query / invoke
--
-- With the nefor-combinators Rust plugin removed (MAG replaces it at
-- compile time), the bus events `combinators.query` and
-- `combinators.invoke` go unanswered. Graphs with fanout nodes hang
-- in PendingTypecheck forever because the reasoner-graph scheduler
-- waits for a `combinators.query.result` that never arrives.
--
-- This shim registers bus handlers that implement the two-phase
-- fanout protocol inline:
--
--   1. `combinators.query` → reply `{ missing: [] }` (all available).
--      The type-level availability check is a MAG concern; at runtime
--      we assume the graph author got it right.
--
--   2. `combinators.invoke` → inspect the input value and route it to
--      the matching output type based on a simple shape heuristic
--      (tool_calls present → ToolCalls type, else → FinalAnswer type).
--      This covers the orchestrator's tool_split fanout. Exotic fanout
--      signatures that don't match the heuristic fall back to routing
--      the input to the first output type — good enough until MAG
--      compiles the routing statically.
--
-- Call `require("core.combinator_shim").install()` after
-- `actor.install()` to arm the handlers.

local json = nefor.json
local M = {}

local function encode(t)
  local ok, s = pcall(json.encode, t)
  if ok then return s end
  return nil
end

-- Respond to `combinators.query` with all-available.
local function handle_query(env)
  local payload = env.payload
  if type(payload) ~= "string" then return end
  local decoded = json.decode(payload)
  if type(decoded) ~= "table" then return end
  local body = decoded.body
  if type(body) ~= "table" then return end
  local request_id = body.id
  if type(request_id) ~= "string" then return end

  local reply = encode({
    type = "event",
    from = "engine",
    ts   = nefor.engine.now(),
    body = {
      kind    = "combinators.query.result",
      id      = request_id,
      missing = {},
    },
  })
  if reply then nefor.engine.send(reply) end
end

-- Respond to `combinators.invoke` with shape-based routing.
local function handle_invoke(env)
  local payload = env.payload
  if type(payload) ~= "string" then return end
  local decoded = json.decode(payload)
  if type(decoded) ~= "table" then return end
  local body = decoded.body
  if type(body) ~= "table" then return end
  local invocation_id = body.id
  if type(invocation_id) ~= "string" then return end
  local signature = body.signature
  if type(signature) ~= "table" then return end
  local out_types = signature.out
  if type(out_types) ~= "table" or #out_types == 0 then return end
  local input = body.input

  -- Heuristic: if the input has tool_calls (non-empty array), route to
  -- the output type containing "ToolCalls". Otherwise route to the
  -- output type containing "FinalAnswer". If neither heuristic matches,
  -- route the full input to the first output type.
  local has_tool_calls = false
  if type(input) == "table" then
    local tc = input.tool_calls
    if type(tc) == "table" and #tc > 0 then
      has_tool_calls = true
    end
  end

  -- Build outputs for ALL declared types. The matching type gets the
  -- input value; non-matching types get json null so the scheduler
  -- properly suppresses those edges (apply_fanout_outputs needs
  -- explicit null entries to mark edges as suppressed).
  local matched_type = nil
  if has_tool_calls then
    for _, out_type in ipairs(out_types) do
      if out_type:find("ToolCalls") then matched_type = out_type; break end
    end
  else
    for _, out_type in ipairs(out_types) do
      if out_type:find("FinalAnswer") then matched_type = out_type; break end
    end
  end
  -- Fallback: if no heuristic matched, route to the first output type.
  if matched_type == nil then matched_type = out_types[1] end

  local outputs = {}
  for _, out_type in ipairs(out_types) do
    if out_type == matched_type then
      outputs[#outputs + 1] = { type = out_type, value = input }
    else
      -- Omit `value` → the Rust parser defaults to serde_json Null,
      -- which apply_fanout_outputs treats as edge suppression.
      outputs[#outputs + 1] = { type = out_type }
    end
  end

  local reply = encode({
    type = "event",
    from = "engine",
    ts   = nefor.engine.now(),
    body = {
      kind    = "combinators.invoke.result",
      id      = invocation_id,
      outputs = outputs,
    },
  })
  if reply then nefor.engine.send(reply) end
end

function M.install()
  nefor.bus.on_event("combinators.query", handle_query)
  nefor.bus.on_event("combinators.invoke", handle_invoke)
end

return M
