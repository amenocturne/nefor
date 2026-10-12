-- plugins/mag/lua/mag-kernel/factories/tool-result.lua — the tool-result adaptation
-- boundary.
--
-- The contract-declared replacement for the shape-sniffing `adapter` handler
-- (examples/nefor-agent/reasoners/init.lua, `adapter_run_node`): it turns aggregated tool
-- results into the provider input for the next llm turn. The baseline scanned
-- its inputs for a `.tool_results` field to decide what it had received; here
-- the declared input contract states it — nothing sniffs a runtime shape
-- (docs/ir.md, Firing; task-nefor-mag-primitive-tools "Done when").
--
-- Contract (reconciled against the plugin acceptance graph —
-- `run-tool` routes `generic-tool.ToolHandle` here, and this node routes
-- `generic-provider.ProviderOut` onward, back into the llm; flagged):
--   input   generic-tool.ToolHandle       (single; fires per handle)
--   output  generic-provider.ProviderOut  (the next provider turn's input)
--
-- ── adaptation shape (flagged) ───────────────────────────────────────────────
--   The ToolHandle carries `results = { { id, name, output, error }, … }` (the
--   run-tool aggregation). Each becomes one tool-role message keyed by the
--   model's `tool_call_id`, so the provider can pair a result to the call that
--   produced it:
--     { kind="generic-provider.ProviderOut", from=id,
--       messages = { { role="tool", tool_call_id=<id>, name=<tool>,
--                      content=<string|structured>, error=<e?> }, … } }
--   `content` is the tool output projected as bounded provider text: strings pass
--   through unchanged while structured output is JSON-encoded. Recognized image
--   media remains structured so the provider can consume it as media. An errored
--   call carries a readable `[tool error] …` content plus the raw `error`. No
--   shape sniffing: the per-field handling is output FORMATTING, not input SELECTION.
--
-- No signal handlers: adaptation is synchronous over an already-arrived batch;
-- the node holds no in-flight external work to abort or flush (actor-model.md,
-- Signals: "explicit signal handlers only where meaningful").

local kinds = require("kinds")
local model_context = require("model-context")

local M = {}

local function is_image_media(value)
  return type(value) == "table"
    and value.type == "media"
    and type(value.media_type) == "string"
    and value.media_type:sub(1, 6) == "image/"
    and type(value.data) == "string"
    and value.data ~= ""
end

M.declaration = {
  name = "tool-result",
  semantic = {
    input={kind="named",name="nefor.wire.ToolHandle",arguments={}},
    output={kind="named",name="nefor.contracts.ProviderInput",arguments={}},
    inputs={{wire="generic-tool.ToolHandle",type={kind="named",name="nefor.wire.ToolHandle",arguments={}}}},
    outputs={{wire="generic-provider.ProviderOut",type={kind="named",name="nefor.contracts.ProviderInput",arguments={}}}},
  },

  params = {},
  template = { relocations = {} },

  inputs = {
    handle = "generic-tool.ToolHandle",
  },

  outputs = {
    "generic-provider.ProviderOut",
  },

  signals = {},
}

-- Format one aggregated tool result into a tool-role provider message. Pure:
-- strings pass through, structured output passes through verbatim, an error
-- becomes readable content while keeping the raw error field.
local function to_message(r)
  r = r or {}
  local content
  if type(r.error) == "string" and r.error ~= "" then
    content = "[tool error] " .. r.error
  elseif type(r.output) == "string" then
    content = r.output
  elseif r.output ~= nil then
    content = r.output
  else
    content = ""
  end
  return {
    role = "tool",
    tool_call_id = r.id,
    name = r.name,
    content = content,
    error = r.error,
    completion_delivery = r.completion_delivery,
  }
end

-- construct(id, params, emit, deps) -> instance
function M.construct(id, params, emit, deps)
  params = params or {}
  deps = deps or {}

  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }

  -- deliver(activation) -> completion (routing.lua, the kernel⇄factory
  -- contract). Single input: fires per ToolHandle. Synchronous — adapt the
  -- aggregated results into a ProviderInput turn and return a successful
  -- completion (the kernel then emits mag.Unit along any dependency edges).
  function instance.deliver(activation)
    activation = activation or {}
    local message = ((activation.messages or {})[1] or {}).message or {}
    local value = type(message.value) == "table" and message.value or {}
    local results = message.results or value.results or {}
    local contents, projectable, positions = {}, {}, {}
    for i, r in ipairs(results) do
      r = r or {}
      local value
      if type(r.error) == "string" and r.error ~= "" then
        value = "[tool error] " .. r.error
      elseif r.output ~= nil then
        value = r.output
      else
        value = ""
      end
      if is_image_media(value) then
        contents[i] = value
      else
        projectable[#projectable + 1] = { value = value, output_path = r.output_path }
        positions[#positions + 1] = i
      end
    end
    for index, content in ipairs(model_context.project(projectable)) do
      contents[positions[index]] = content
    end
    local messages = {}
    for i, r in ipairs(results) do
      local message = to_message(r)
      message.content = contents[i]
      messages[i] = message
    end
    -- Keep the canonical ProviderInput value and routing messages aligned. The
    -- provider boundary consumes only `messages` when recording model context,
    -- so this persisted transport envelope is not a second wire copy.
    emit(sign({ kind = "generic-provider.ProviderOut",
      value = { content = messages }, messages = messages }))
    return { status = "ok" }
  end

  -- Readiness confirmation (actor-model.md, Lifecycle): construction happens at
  -- the first activation, so this emit coincides with beginning work.
  emit(sign({ kind = kinds.ready }))

  return instance
end

return M
