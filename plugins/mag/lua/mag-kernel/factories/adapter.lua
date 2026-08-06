-- plugins/mag/lua/mag-kernel/factories/adapter.lua — the agent's entry boundary
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
--   input   ( task | generic-provider.FinalAnswer | human.Rejected )
--           union — fires on any
--   output  generic-provider.ProviderOut              the next provider turn
--
-- The union input is the whole point of the boundary: `task` is the initial
-- activation content the loader seeds a source agent with (crates/nefor-mag
-- ir.rs initial_activation_content -> `{ kind = "task", prompt = ... }`),
-- `generic-provider.FinalAnswer` is what an upstream agent routes in
-- (docs-explorer.llm -> code-writer.entry), and `human.Rejected` is the gate
-- template's revise leg (starter/mag/lib/templates.mag: the rejection reason
-- re-enters the producing llm as its next turn — the llm's owned transcript
-- already carries the rejected draft, so the reason alone is the feedback).
-- Firing "on any" (shape.lua) means whichever arrives activates alone — no
-- waiting, no accumulation.
--
-- ── the boundary mapping (flagged) ──────────────────────────────────────────
--   A single input lifts into one user-role turn message. A product assembled
--   by the firing machine from separate routes lifts into one message per
--   component, in product-position order. Each message carries that component's
--   own semantic schema; a whole product arriving on one edge remains one
--   message carrying the whole product schema. The provider boundary appends
--   this native `messages` list to its transcript unchanged.
--
--   Per-input extraction is DISPATCHED ON THE DECLARED TAG, not sniffed from the
--   value's shape (actor-model.md, Factories: "nothing selects inputs by
--   sniffing their shape" — the firing machine already stamped the type fact
--   onto the activation triple):
--     task                          -> content = message.prompt
--     generic-provider.FinalAnswer  -> content = message.final_answer
--                                                 or message.text
--                                                 or message.result
--     human.Rejected                -> content = message.reason
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
local PROVIDER_INPUT = "generic-provider.ProviderOut"
local REJECTED = "human.Rejected"
local AGENT_RESULT = "nefor.agent.Result"

M.declaration = {
  name = "adapter",
  type_variables = { "T" },
  semantic = {
    input={kind="variable",name="T"},
    output={kind="named",name="nefor.contracts.ProviderInput",arguments={}},
    inputs = {
      {wire="task",type={kind="variable",name="T"}},
      {wire=FINAL_ANSWER,type={kind="variable",name="T"}},
      {wire=REJECTED,type={kind="variable",name="T"}},
      {wire=PROVIDER_INPUT,type={kind="variable",name="T"}},
      {wire=AGENT_RESULT,type={kind="variable",name="T"}},
    },
    outputs = {{ wire = "generic-provider.ProviderOut", type = {
      kind="named", name="nefor.contracts.ProviderInput", arguments={}
    }}},
  },

  params = {
    seed = "string?", -- boundary-shape label (the loader authors "provider-in")
    schema = "table",
  },

  -- Union input (shape.lua): the initial task seed, an upstream FinalAnswer,
  -- or a human gate's rejection re-entering the provider loop (the gate
  -- template's revise leg). Firing "on any".
  inputs = {
    boundary = { "task", FINAL_ANSWER, REJECTED, PROVIDER_INPUT, AGENT_RESULT },
  },

  outputs = {
    "generic-provider.ProviderOut",
  },

  signals = {},
}

-- Lift boundary inputs into downstream provider turns. Dispatch on each
-- declared tag (a type fact), extract the turn content per input, and preserve
-- the firing machine's product order. Pure: strings and structured values pass
-- through verbatim for the provider layer to serialize.
local function selected_content(content, schema, arrival)
  if type(schema) == "table" and type(schema.root) == "table"
      and schema.root.kind == "union" and type(arrival) == "table"
      and type(arrival.constructor_id) == "string" then
    return { type = arrival.constructor_id, value = content }
  end
  return content
end

local function component_schema(schema, position)
  local root = type(schema) == "table" and schema.root or nil
  local components = type(root) == "table" and root.kind == "product" and root.components or nil
  local component = type(components) == "table" and components[position] or nil
  if component == nil then return schema end
  return { version = schema.version, root = component }
end

local function turn_message(tag, message, schema, arrival)
  message = message or {}
  local content = message.value
  if tag == REJECTED and type(content) == "table" and content.reason ~= nil then
    content = content.reason
  elseif content == nil and tag == FINAL_ANSWER then
    -- Legacy/non-strict factory tests still exercise the old provider envelope.
    content = message.final_answer or message.text or message.result
  elseif content == nil and tag == REJECTED then
    content = message.reason
  elseif content == nil and message.prompt ~= nil then
    -- Initial task injection is the only untyped compatibility boundary.
    content = { prompt = message.prompt }
  elseif content == nil then
    content = message
  end
  return { role = "user", content = {
    mag_type = schema,
    value = selected_content(content, schema, arrival),
  } }
end

local function to_provider_input(activation, schema)
  local inputs = activation.messages or {}
  if #inputs == 1 and inputs[1].tag == PROVIDER_INPUT then return inputs[1].message end

  local messages = {}
  for position, input in ipairs(inputs) do
    local input_schema = activation.whole and schema or component_schema(schema, position)
    messages[position] = turn_message(
      input.tag, input.message, input_schema, input.arrival)
  end
  return {
    kind = "generic-provider.ProviderOut",
    value = { content = messages[1] and messages[1].content.value },
    messages = messages,
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
    emit(sign(to_provider_input(activation, params.schema)))
    return { status = "ok" }
  end

  -- Readiness confirmation (actor-model.md, Lifecycle): construction happens at
  -- the first activation, so this emit coincides with beginning work.
  emit(sign({ kind = kinds.ready }))

  return instance
end

return M
