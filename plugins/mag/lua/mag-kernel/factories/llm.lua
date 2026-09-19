-- Direct-text provider boundary. The compiler selects this factory only for
-- the nominal nefor.contracts.TextAnswer codec; structured contracts use the
-- structured-output factory instead.
local boundary = require("factories.provider-boundary")
local M = {}
local RESULT = "nefor.agent.Result"

M.declaration = {
  name = "llm",
  type_variables = { "R" },
  semantic = {
    input={kind="named",name="nefor.contracts.ProviderInput",arguments={}},
    output={kind="variable",name="R"},
    inputs={{wire="generic-provider.ProviderOut",type={kind="named",name="nefor.contracts.ProviderInput",arguments={}}}},
    outputs={
      {wire="generic-tool.ToolCalls",type={kind="named",name="nefor.contracts.ToolCalls",arguments={}}},
      {wire=RESULT,type={kind="variable",name="R"}},
    },
  },
  params = {
    model = "string", system = "string?", tools = "table?",
    reasoning_effort = "string?",
    provider_options = "table?",
    provider = "string", history = "table?",
    max_tool_call_corrections = "number?",
    output_type = "string", error_type = "string", provider_error_type = "string",
    conversation_id = "string?",
    turn_id = "string?", submission_ids = "table?", input_cause = "string?",
    authored_prompt = "string?",
  },
  template = { relocations = {} },
  inputs = { provider_input = "generic-provider.ProviderOut" },
  outputs = { "generic-tool.ToolCalls", RESULT },
  signals = { "kill", "drain", "steer" },
}

local function option(value)
  if type(value) == "string" then return { present = true, value = value } end
  return { present = false, value = "" }
end

local function provider_error(detail)
  if type(detail) == "table" then
    local message = detail.message
    if type(message) ~= "string" or message == "" then message = tostring(detail) end
    return { message = message, detail = option(detail.detail) }
  end
  return { message = tostring(detail), detail = option(nil) }
end

local function record_final_message(state, content)
  local message = { role = "assistant", content = content or "" }
  if not state:complete_streamed_message(message) and message.content ~= "" then
    state:append(message)
  end
end

function M.construct(id, params, emit, deps)
  params = params or {}
  deps = deps or {}
  if type(params.output_type) ~= "string" or type(params.error_type) ~= "string"
      or type(params.provider_error_type) ~= "string" then
    return nil, string.format("llm '%s': compiler result constructor ids are required", tostring(id))
  end
  return boundary.construct(id, params, emit, {
    conversation = deps.conversation,
    name = "llm",
    steerable = true,
    on_steered_final = function(state, result)
      record_final_message(state, boundary.answer_text(result))
    end,
    on_final = function(state, result)
      local content = boundary.answer_text(result) or ""
      record_final_message(state, content)
      local value = { constructor = "Ok", value = content }
      state:finish({ kind = RESULT, value = value, result = result }, {
          result = result, value = value,
        })
    end,
    on_error = function(state, detail)
      state:finish({ kind = RESULT, value = {
        constructor = "Error", value = {
          last_output = nefor.json.decode("null"), reason = {
            constructor = "ProviderError", value = provider_error(detail),
          },
        },
      }}, { error = detail })
    end,
  })
end

return M
