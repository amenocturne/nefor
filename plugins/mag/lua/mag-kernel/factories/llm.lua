-- Prose provider boundary. Shared request/correlation/tool lifecycle lives
-- in provider-boundary.lua; this module only declares and classifies its final
-- output.
local boundary = require("factories.provider-boundary")
local M = {}

local function final_answer_text(result)
  local content = boundary.answer_text(result)
  if type(content) ~= "string" or content == "" then return content end
  if type(nefor) == "table" and type(nefor.json) == "table"
      and type(nefor.json.decode) == "function" then
    local ok, decoded = pcall(nefor.json.decode, content)
    if ok and type(decoded) == "table" and type(decoded.content) == "string" then
      return decoded.content
    end
  end
  return content
end

M.declaration = {
  name = "llm",
  semantic = {
    input={kind="named",name="nefor.contracts.ProviderInput",arguments={}},
    output={kind="union",items={
      {kind="named",name="nefor.contracts.ToolCalls",arguments={}},
      {kind="named",name="nefor.contracts.FinalAnswer",arguments={}},
    }},
    inputs={{wire="generic-provider.ProviderOut",type={kind="named",name="nefor.contracts.ProviderInput",arguments={}}}},
    outputs={
      {wire="generic-tool.ToolCalls",type={kind="named",name="nefor.contracts.ToolCalls",arguments={}}},
      {wire="generic-provider.FinalAnswer",type={kind="named",name="nefor.contracts.FinalAnswer",arguments={}}},
    },
  },
  params = {
    model = "string?", system = "string?", tools = "table?",
    profile = "string?", reasoning_effort = "string?",
    provider = "string?", history = "table?",
    conversation_context = "table?",
    conversation_id = "string?",
    turn_id = "string?",
  },
  inputs = { provider_input = "generic-provider.ProviderOut" },
  outputs = { "generic-tool.ToolCalls", "generic-provider.FinalAnswer" },
  signals = { "kill", "drain", "steer" },
}

function M.construct(id, params, emit, deps)
  deps = deps or {}
  return boundary.construct(id, params, emit, {
    conversation = deps.conversation,
    name = "llm",
    steerable = true,
    on_steered_final = function(state, result)
      local content = final_answer_text(result)
      if type(content) == "string" and content ~= "" then
        state:append({ role = "assistant", content = content })
      end
    end,
    on_final = function(state, result)
      local content = final_answer_text(result) or ""
      local final = { kind = "generic-provider.FinalAnswer",
        value = { content = content }, result = result }
      if type(result) == "table" then
        final.text = result.text
        final.final_answer = result.final_answer
      end
      if content ~= "" then state:append({ role = "assistant", content = content }) end
      state:finish(final)
    end,
  })
end

return M
