-- Prose provider boundary. Shared transcript/correlation/tool lifecycle lives
-- in provider-boundary.lua; this module only declares and classifies its final
-- output.
local boundary = require("factories.provider-boundary")
local M = {}

M.declaration = {
  name = "llm",
  params = {
    model = "string?", system = "string?", tools = "table?",
    profile = "string?", reasoning_effort = "string?",
    provider = "string?", history = "table?",
  },
  inputs = { provider_out = "generic-provider.ProviderOut" },
  outputs = { "generic-tool.ToolCalls", "generic-provider.FinalAnswer" },
  signals = { "kill", "drain" },
}

function M.construct(id, params, emit)
  return boundary.construct(id, params, emit, {
    name = "llm",
    on_final = function(state, result)
      local final = { kind = "generic-provider.FinalAnswer", result = result }
      if type(result) == "table" then
        final.text = result.text
        final.final_answer = result.final_answer
        if type(result.text) == "string" and result.text ~= "" then
          state:append({ role = "assistant", content = result.text })
        end
      end
      local delta = state:transcript_delta()
      if #delta > 0 then final.transcript_delta = delta end
      state:finish(final)
    end,
  })
end

return M
