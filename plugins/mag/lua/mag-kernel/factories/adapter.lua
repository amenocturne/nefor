-- plugins/mag/lua/mag-kernel/factories/adapter.lua — the agent's entry boundary
-- type-shift.
--
-- Every agent opens with an `entry` node of factory `adapter`, params
-- `{ seed = "provider-in" }`, wired `IN -> generic-provider.ProviderOut`.
-- Its job is the boundary type shift: whatever crosses into the agent — the
-- program's typed input, or an upstream agent result — is lifted
-- into the `generic-provider.ProviderOut` turn the downstream `llm` consumes.
-- A ProviderInput is already a complete provider turn and passes through.
--
-- Contract (reconciled against the plugin acceptance graph and the MAG agent
-- library lowering which authors this node; flagged):
--   input   ( nefor.agent.Input | generic-provider.ProviderOut )
--           union — fires on either
--   output  generic-provider.ProviderOut              the next provider turn
--
-- `nefor.actors.agent` derives the first wire for every semantic input other
-- than ProviderInput, including sums and products. The ProviderInput nominal
-- type derives the continuation wire. Firing "on any" means either activates
-- alone; product assembly still happens before delivery.
--
-- ── the boundary mapping (flagged) ──────────────────────────────────────────
--   A single input lifts into one user-role turn message. A product assembled
--   by the firing machine from separate routes lifts into one message per
--   component, in product-position order. Each message carries that component's
--   own semantic schema; a whole product arriving on one edge remains one
--   message carrying the whole product schema. The provider boundary appends
--   this native `messages` list to its transcript unchanged.
--
--   The semantic value arrives in `message.value`. A string or structured
--   value passes through verbatim for the provider layer to serialize. The
--   `seed = "provider-in"` param names the boundary shape; the mapping is fixed.
--
-- No signal handlers: the shift is synchronous over an already-arrived message;
-- the node holds no in-flight external work to abort or flush (actor-model.md,
-- Signals: explicit handlers only where meaningful — cf. factories/tool-result.lua).

local kinds = require("kinds")
local node_approval = require("node-approval")
local model_context = require("model-context")

local M = {}

local AGENT_INPUT = "nefor.agent.Input"
local PROVIDER_INPUT = "generic-provider.ProviderOut"

M.declaration = {
  name = "adapter",
  -- Turns a typed value into a provider turn; it changes nothing outside the run.
  approval = node_approval.inert("adapter"),
  type_variables = { "T" },
  semantic = {
    input={kind="variable",name="T"},
    output={kind="named",name="nefor.contracts.ProviderInput",arguments={}},
    inputs = {
      {wire=AGENT_INPUT,type={kind="variable",name="T"}},
      {wire=PROVIDER_INPUT,type={kind="variable",name="T"}},
    },
    outputs = {{ wire = "generic-provider.ProviderOut", type = {
      kind="named", name="nefor.contracts.ProviderInput", arguments={}
    }}},
  },

  params = {
    seed = "string?", -- boundary-shape label (the loader authors "provider-in")
    schema = "table",
  },
  template = { relocations = {} },

  -- Union input (shape.lua): a fresh typed turn or an already-built provider
  -- continuation. Firing "on any".
  inputs = {
    boundary = { AGENT_INPUT, PROVIDER_INPUT },
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
local function selected_content(content)
  return content
end

local function component_schema(schema, position)
  local root = type(schema) == "table" and schema.root or nil
  local components = type(root) == "table" and root.kind == "product" and root.components or nil
  local component = type(components) == "table" and components[position] or nil
  if component == nil then return schema end
  return { version = schema.version, root = component }
end

local function turn_message(message, schema, arrival)
  message = message or {}
  local content = message.value
  if content == nil then
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
  local projectable = {}
  for position, input in ipairs(inputs) do
    local input_schema = activation.whole and schema or component_schema(schema, position)
    local message = turn_message(input.message, input_schema, input.arrival)
    messages[position] = message
    projectable[position] = {
      value = message.content,
      output_path = type(input.message) == "table" and input.message.output_path or nil,
    }
  end
  local projected = model_context.project(projectable, true)
  for position, content in ipairs(projected) do messages[position].content = content end
  local first_content = messages[1] and messages[1].content
  return {
    kind = "generic-provider.ProviderOut",
    value = {
      content = type(first_content) == "table" and first_content.value or first_content,
    },
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
