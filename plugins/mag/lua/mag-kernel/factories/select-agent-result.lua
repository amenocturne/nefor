local kinds = require("kinds")
local M = {}

local RESULT = "nefor.agent.Result"
local OUTPUT = "nefor.agent.Output"
local ERROR = "nefor.agent.Error"
local agent_error = {kind="named",name="nefor.contracts.AgentError",arguments={}}
local variable = {kind="variable",name="T"}
local result_type = {kind="union",items={variable,agent_error}}

M.declaration = {
  name = "select-agent-result",
  type_variables = { "T" },
  semantic = {
    input = result_type,
    output = result_type,
    inputs = {{ wire=RESULT, type=result_type }},
    outputs = {{ wire=OUTPUT, type=variable }, { wire=ERROR, type=agent_error }},
  },
  params = {},
  inputs = { result = RESULT },
  outputs = { OUTPUT, ERROR },
  signals = {},
}

function M.construct(id, _, emit)
  local instance = { id = id }
  function instance.deliver(activation)
    local incoming = ((activation or {}).messages or {})[1] or {}
    local message = incoming.message or {}
    local variant = message.variant
    if variant == "success" then
      emit({ kind=OUTPUT, from=id, value=message.value })
    elseif variant == "error" then
      emit({ kind=ERROR, from=id, value=message.value })
    else
      return { status="failed", failure=kinds.Failed,
        value={ error="agent result is missing a known variant" } }
    end
    return { status="ok" }
  end
  emit({ kind=kinds.ready, from=id })
  return instance
end

return M
