-- starter/mag-kernel/factories/adapter.lua — the agent's entry boundary
-- type-shift.
--
-- Every agent template (crates/nefor-mag eval_agent; plugins/mag/docs/lowering.md,
-- "the entry adapter's role") opens with an `entry` node of factory `adapter`,
-- params `{ seed = "provider-in" }`, wired `IN -> generic-provider.ProviderOut`.
-- Its job is the boundary type shift: whatever crosses into the agent — the
-- program's initial task seed, or an upstream agent's FinalAnswer — is lifted
-- into the `generic-provider.ProviderOut` turn the downstream `llm` consumes.
-- This is why the same template instantiates against `mag.Task` (agent 1) and
-- `generic-provider.FinalAnswer` (agent 2) unchanged: the entry adapter absorbs
-- the difference (two-agents.mag, "Boundary contract").
--
-- Contract (reconciled against tests/fixtures/two-agents.modification.json and
-- the loader's eval_agent, which authors this node; flagged):
--   input   ( task | generic-provider.FinalAnswer )   union — fires on either
--   output  generic-provider.ProviderOut              the next provider turn
--
-- The union input is the whole point of the boundary: `task` is the initial
-- activation content the loader seeds a source agent with (crates/nefor-mag
-- ir.rs initial_activation_content -> `{ kind = "task", prompt = ... }`), and
-- `generic-provider.FinalAnswer` is what an upstream agent routes in
-- (docs-explorer.llm -> code-writer.entry). Firing "on any" (shape.lua)
-- means whichever arrives activates alone — no waiting, no accumulation.
--
-- ── the boundary mapping (flagged) ──────────────────────────────────────────
--   Both inputs lift into ONE user-role turn message, the shape the provider
--   loop consumes (the canonical ProviderOut carries a `messages` array of
--   role-tagged turn messages — factories/tool-result.lua emits exactly this,
--   and factories/llm.lua extends its per-instance transcript with them):
--     { kind = "generic-provider.ProviderOut", from = id,
--       messages = { { role = "user", content = <lifted> } } }
--
--   Per-input extraction is DISPATCHED ON THE DECLARED TAG, not sniffed from the
--   value's shape (actor-model.md, Factories: "nothing selects inputs by
--   sniffing their shape" — the firing machine already stamped the type fact
--   onto the activation triple):
--     task                          -> content = message.prompt
--     generic-provider.FinalAnswer  -> content = message.final_answer
--                                                 or message.text
--                                                 or message.result
--   The FinalAnswer preference order mirrors what factories/llm.lua lifts onto a
--   FinalAnswer (`final_answer`/`text` when the provider result is a table, else
--   the raw `result`). A string passes through; a structured value passes
--   through verbatim for the provider layer to serialize (the kernel VM ships no
--   json binding — see factories/tool-result.lua — so this factory stays pure
--   and does not stringify). The `seed = "provider-in"` param names the boundary
--   shape (lift into provider input); the mapping is otherwise fixed.
--
-- No signal handlers: the shift is synchronous over an already-arrived message;
-- the node holds no in-flight external work to abort or flush (actor-model.md,
-- Signals: explicit handlers only where meaningful — cf. factories/tool-result.lua).

local kinds = require("kinds")

local M = {}

local FINAL_ANSWER = "generic-provider.FinalAnswer"

M.declaration = {
  name = "adapter",

  params = {
    seed = "string?", -- boundary-shape label (the loader authors "provider-in")
  },

  -- Union input (shape.lua): the initial task seed OR an upstream FinalAnswer.
  -- `task` is the loader's initial activation kind; the qualified FinalAnswer is
  -- what an upstream agent routes in. Firing "on any".
  inputs = {
    boundary = { "task", FINAL_ANSWER },
  },

  outputs = {
    "generic-provider.ProviderOut",
  },

  signals = {},
}

-- Lift one boundary input into the downstream provider turn. Dispatch on the
-- declared tag (a type fact), extract the turn content per input, wrap it as a
-- single user-role message. Pure: strings pass through, structured values pass
-- through verbatim for the provider layer to serialize.
local function to_provider_out(tag, message)
  message = message or {}
  local content
  if tag == FINAL_ANSWER then
    content = message.final_answer or message.text or message.result
  else
    -- the initial task seed ({ kind = "task", prompt = ... })
    content = message.prompt
  end
  return {
    kind = "generic-provider.ProviderOut",
    messages = { { role = "user", content = content } },
  }
end

-- construct(id, params, emit, deps) -> instance
function M.construct(id, params, emit, deps)
  params = params or {}

  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }

  -- deliver(activation) -> completion (routing.lua, the kernel⇄factory
  -- contract). Union input: fires per arriving boundary message. Synchronous —
  -- lift it into the provider turn and return a successful completion (the
  -- kernel then emits mag.Unit along any dependency edges).
  function instance.deliver(activation)
    activation = activation or {}
    local one = (activation.messages or {})[1] or {}
    emit(sign(to_provider_out(one.tag, one.message)))
    return { status = "ok" }
  end

  -- Readiness confirmation (actor-model.md, Lifecycle): construction happens at
  -- the first activation, so this emit coincides with beginning work.
  emit(sign({ kind = kinds.ready }))

  return instance
end

return M
