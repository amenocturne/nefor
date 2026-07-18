-- Typed JSON provider boundary. Generic provider lifecycle is shared with the
-- prose llm factory; this module owns only schema instruction, validation and
-- bounded correction policy.
local boundary = require("factories.provider-boundary")
local M = {}
local RESULT = "nefor.agent.Result"

M.declaration = {
  name = "structured-output",
  type_variables = { "T" },
  semantic = {
    input={kind="named",name="nefor.contracts.ProviderInput",arguments={}},
    output={kind="union",items={
      {kind="named",name="nefor.contracts.ToolCalls",arguments={}},
      {kind="union",items={
        {kind="variable",name="T"},
        {kind="named",name="nefor.contracts.AgentError",arguments={}},
      }},
    }},
    inputs = {{ wire="generic-provider.ProviderOut", type={kind="named",
      name="nefor.contracts.ProviderInput",arguments={}} }},
    outputs = {
      {wire="generic-tool.ToolCalls",type={kind="named",name="nefor.contracts.ToolCalls",arguments={}}},
      {wire=RESULT,type={kind="union",items={
        {kind="variable",name="T"},
        {kind="named",name="nefor.contracts.AgentError",arguments={}},
      }}},
    },
  },
  params = {
    model = "table?", profile = "table?", provider = "string",
    system = "string?", tools = "table?", history = "table?", schema = "table",
    max_corrections = "number",
  },
  inputs = { provider_out = "generic-provider.ProviderOut" },
  outputs = { "generic-tool.ToolCalls", RESULT },
  signals = { "kill", "drain" },
}

local function json_encode(value)
  local ok, encoded = pcall(nefor.json.encode, value)
  return ok and encoded or "<unencodable>"
end

local function correction(validation)
  local detail
  if type(validation.error) == "table" then
    detail = validation.error.kind .. ": " .. validation.error.message
  else
    detail = json_encode(validation.violations or {})
  end
  return "Your previous response was not valid for the required MAG type. "
    .. "Return a corrected bare JSON value only. Diagnostics: " .. detail
end

local function output_violations(validation, message)
  local violations = validation.violations
  if type(violations) ~= "table" or next(violations) == nil then
    violations = {{ path = "$", code = "invalid_json", expected = "typed JSON",
      actual = "invalid", message = message or
        (type(validation.error) == "table" and validation.error.message) or "invalid JSON" }}
  end
  return violations
end

local function option(value)
  if type(value) == "string" then
    return { tag = "core.types.Some", value = value }
  end
  return { tag = "core.types.None" }
end

local function provider_error(detail)
  if type(detail) == "table" then
    local message = detail.message
    if type(message) ~= "string" or message == "" then message = tostring(detail) end
    return { message = message, detail = option(detail.detail) }
  end
  return { message = tostring(detail), detail = option(nil) }
end

function M.construct(id, params, emit)
  params = params or {}
  if type(params.schema) ~= "table" then
    return nil, string.format("structured-output '%s': params.schema is required", tostring(id))
  end
  if type(nefor.typed_json) ~= "table" or type(nefor.typed_json.validate) ~= "function" then
    return nil, "structured-output requires nefor.typed_json.validate"
  end
  if type(params.max_corrections) ~= "number" or params.max_corrections < 0
      or params.max_corrections % 1 ~= 0 then
    return nil, string.format("structured-output '%s': params.max_corrections must be a non-negative integer", tostring(id))
  end
  local corrections = 0
  local last_output = nefor.json.decode("null")
  local function finish_result(state, variant, value)
    local message = { kind=RESULT, variant=variant, value=value }
    local delta = state:transcript_delta()
    if #delta > 0 then message.transcript_delta = delta end
    state:finish(message)
  end
  local function finish_error(state, reason)
    finish_result(state, "error", {
      last_output=last_output, reason=reason,
    })
  end
  return boundary.construct(id, params, emit, {
    name = "structured-output",
    on_turn_start = function(_)
      corrections = 0
      last_output = nefor.json.decode("null")
    end,
    on_final = function(state, result)
      last_output = result
      local text = boundary.answer_text(result)
      local validation
      if text == nil then
        validation = { ok = false, error = {
          kind = "missing_text", message = "provider final answer had no textual content",
        }}
      else
        validation = nefor.typed_json.validate(params.schema, text)
        state:append({ role = "assistant", content = text })
      end
      if validation.ok then
        finish_result(state, "success", validation.value)
        return
      end
      if state:is_draining() then
        state:fail("structured output was draining and could not start a correction round")
        return
      end
      if corrections >= params.max_corrections then
        finish_error(state, { violations=output_violations(validation) })
        return
      end
      corrections = corrections + 1
      state:append({ role = "user", content = correction(validation) })
      state:retry()
    end,
    on_tool_calls = function(_, result)
      last_output = result
    end,
    on_error = function(state, detail)
      finish_error(state, provider_error(detail))
    end,
  })
end

return M
