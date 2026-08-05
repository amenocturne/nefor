-- Typed JSON provider boundary. Generic provider lifecycle is shared with the
-- prose llm factory; this module owns only schema instruction, validation and
-- bounded correction policy.
local boundary = require("factories.provider-boundary")
local M = {}
local preview_components = require("preview-components")
local RESULT = "nefor.agent.Result"

M.declaration = {
  preview = preview_components.structured_output(),
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
    output_type = "string", error_type = "string",
    provider_error_type = "string", validation_error_type = "string",
    max_corrections = "number",
    conversation_context = "table?",
    conversation_id = "string?",
    turn_id = "string?",
  },
  inputs = { provider_input = "generic-provider.ProviderOut" },
  outputs = { "generic-tool.ToolCalls", RESULT },
  signals = { "kill", "drain" },
}

local function json_encode(value)
  local ok, encoded = pcall(nefor.json.encode, value)
  return ok and encoded or "<unencodable>"
end

local function correction(validation, wrapped)
  local detail
  if type(validation.error) == "table" then
    detail = validation.error.kind .. ": " .. validation.error.message
  else
    detail = json_encode(validation.violations or {})
  end
  local shape = wrapped
    and 'Return only a JSON object of the form {"value": <corrected value>}.'
    or "Return the corrected JSON object only."
  return "Your previous response was not valid for the required MAG type. "
    .. shape .. " Diagnostics: " .. detail
end

local function root_union(schema)
  local root = schema and schema.root
  while type(root) == "table" and root.kind == "named" do root = root.body end
  return type(root) == "table" and root.kind == "union"
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
    return { present = true, value = value }
  end
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

function M.construct(id, params, emit, deps)
  params = params or {}
  deps = deps or {}
  if type(params.schema) ~= "table" then
    return nil, string.format("structured-output '%s': params.schema is required", tostring(id))
  end
  if type(nefor.typed_json) ~= "table"
      or type(nefor.typed_json.validate_provider) ~= "function"
      or type(nefor.typed_json.provider_schema) ~= "function" then
    return nil, "structured-output requires the MAG typed JSON provider bridge"
  end
  if type(params.max_corrections) ~= "number" or params.max_corrections < 0
      or params.max_corrections % 1 ~= 0 then
    return nil, string.format("structured-output '%s': params.max_corrections must be a non-negative integer", tostring(id))
  end
  if type(params.output_type) ~= "string" or type(params.error_type) ~= "string"
      or type(params.provider_error_type) ~= "string"
      or type(params.validation_error_type) ~= "string" then
    return nil, string.format(
      "structured-output '%s': compiler result constructor ids are required",
      tostring(id))
  end
  local corrections = 0
  local last_output = nefor.json.decode("null")
  local provider_schema = nefor.typed_json.provider_schema(params.schema)
  local provider_params = {}
  for key, value in pairs(params) do provider_params[key] = value end
  provider_params.schema = provider_schema.schema
  local function finish_result(state, type_id, value)
    local message = { kind=RESULT, semantic_type_id=type_id, value=value }
    state:finish(message, {
      result = last_output,
      value = value,
      semantic_type_id = type_id,
    })
  end
  local function finish_error(state, reason_type, reason)
    finish_result(state, params.error_type, {
      last_output=last_output, reason={type=reason_type,value=reason},
    })
  end
  return boundary.construct(id, provider_params, emit, {
    conversation = deps.conversation,
    name = "structured-output",
    preview = deps.preview,
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
        validation = nefor.typed_json.validate_provider(params.schema, text)
      end
      if validation.ok then
        local value = validation.value
        local selected = params.output_type
        if root_union(params.schema) then
          selected = value.type
          value = value.value
        end
        local content = type(value) == "table" and value.content or nil
        state:append({
          role = "assistant",
          content = type(content) == "string" and content or text,
        })
        finish_result(state, selected, value)
        return
      end
      if text ~= nil then state:append({ role = "assistant", content = text }) end
      if state:is_draining() then
        state:fail("structured output was draining and could not start a correction round")
        return
      end
      local violations = output_violations(validation)
      state:observe("append", "transcript", { kind = "validation", value = {
        attempt = corrections + 1, violations = violations,
      } })
      if corrections >= params.max_corrections then
        finish_error(state, params.validation_error_type, { violations=violations })
        return
      end
      corrections = corrections + 1
      state:append({ role = "user", content = correction(validation, provider_schema.wrapped) })
      state:retry("structured_output_correction")
    end,
    on_tool_calls = function(_, result)
      last_output = result
    end,
    on_error = function(state, detail)
      finish_error(state, params.provider_error_type, provider_error(detail))
    end,
  })
end

return M
