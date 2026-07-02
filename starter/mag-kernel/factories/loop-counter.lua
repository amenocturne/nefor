-- starter/mag-kernel/factories/loop-counter.lua — the bounded-cycle primitive.
--
-- One of the irreducible runtime-machinery factories: MAG cannot express a
-- per-instance mutable counter, so the loop bound lives in Lua. Every cycle
-- in a composition passes through one loop-counter (docs/lowering.md, "Cycles,
-- loop-counters, typed exits"); because ids are namespaced, two agents each
-- get their own instance with its own `max` and its own back-route — the
-- counters are independent by construction (per-instance state, nothing shared).
--
-- Contract (matches tests/fixtures/two-agents.modification.json):
--   input   generic-provider.ProviderOut                    (single; fires per message)
--   output  generic-provider.ProviderOut | mag.LoopExhausted (union; a typed exit)
--   params  { max }                                         per-instance bound
--
-- Below the bound the input passes through unchanged (re-signed with this
-- instance's id) back into the cycle; at exhaustion the counter diverts to the
-- `mag.LoopExhausted` exit. Which variant leaves the cycle is a type fact, not
-- a position heuristic (actor-model.md, Factories).
--
-- Boundary decision (flagged): exhaustion is `count > max`, matching the
-- landed `starter/reasoners/loop_counter.lua` baseline (`exceeded = count >
-- limit`). So `max` is the number of pass-throughs permitted: activations
-- 1..max re-enter the loop, activation max+1 is diverted. The alternative
-- reading ("the max-th activation is the exhausting one", `count >= max`) is a
-- one-line change; picked the baseline-consistent form.
--
-- No signal handlers: a loop-counter holds no external work to abort or flush,
-- so per actor-model.md ("explicit signal handlers only where meaningful") it
-- declares and implements none. Its whole state is an integer.

local M = {}

M.declaration = {
  name = "loop-counter",

  params = {
    max = "number", -- per-instance cycle bound; lowering always supplies it
  },

  inputs = {
    provider_out = "generic-provider.ProviderOut",
  },

  outputs = {
    "generic-provider.ProviderOut",
    "mag.LoopExhausted",
  },

  signals = {},
}

-- construct(id, params, emit, deps) -> instance
function M.construct(id, params, emit, deps)
  params = params or {}

  -- Per-instance state — the reason this primitive is Lua, not MAG. Two
  -- instances constructed from the same template close over distinct `count`s.
  local max = (type(params.max) == "number" and params.max >= 1) and params.max or math.huge
  local count = 0

  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }

  -- Shallow pass-through: preserve the incoming ProviderOut payload verbatim,
  -- re-sign it with this instance's id, and send it back into the cycle.
  local function passthrough(message)
    local out = {}
    for k, v in pairs(message) do
      out[k] = v
    end
    out.kind = "generic-provider.ProviderOut"
    out.count = count
    return sign(out)
  end

  -- deliver(activation) -> completion (routing.lua, the kernel⇄factory
  -- contract). Single input: fires per arriving ProviderOut. Emits one union
  -- variant (pass-through below the bound, LoopExhausted at exhaustion) and
  -- returns a successful completion — the kernel then emits mag.Unit along any
  -- dependency edges (the typed variant is the data output; mag.Unit is the
  -- kernel-synthesized status).
  function instance.deliver(activation)
    activation = activation or {}
    local message = ((activation.messages or {})[1] or {}).message or {}
    count = count + 1
    if count > max then
      -- LoopExhausted payload (flagged for review): carries enough context for
      -- a downstream summarizer (the fixture wires this to an `llm` "exhaust"
      -- node). `last` is the most recent provider output — the in-progress
      -- state to summarize; `max`/`count` explain why the loop stopped.
      emit(sign({
        kind = "mag.LoopExhausted",
        reason = "loop-exhausted",
        max = (max == math.huge) and nil or max,
        count = count,
        last = message,
      }))
    else
      emit(passthrough(message))
    end
    return { status = "ok" }
  end

  -- Ready barrier (actor-model.md, Lifecycle): confirm creation for this id.
  emit(sign({ kind = "mag.ready" }))

  return instance
end

return M
