-- Typed JSON provider boundary. Generic provider lifecycle is shared with the
-- prose llm factory; this module owns only schema instruction, validation and
-- bounded correction policy.
local boundary = require("factories.provider-boundary")
local M = {}
local MAX_ATTEMPTS = 3
local VALIDATED = "nefor.structured.Validated"

M.declaration = {
  name = "structured-output",
  type_variables = { "T" },
  params = {
    model = "table?", profile = "table?", provider = "string",
    system = "string?", tools = "table?", history = "table?", schema = "table",
  },
  inputs = { provider_out = "generic-provider.ProviderOut" },
  outputs = { "generic-tool.ToolCalls", VALIDATED },
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

local function output_error(validation, attempts, kind, message)
  return {
    kind = kind or (type(validation.error) == "table" and validation.error.kind) or "schema_violation",
    message = message or (type(validation.error) == "table" and validation.error.message)
      or "JSON did not conform to the required MAG type",
    attempts = attempts,
    violations = validation.violations or {},
  }
end

function M.construct(id, params, emit)
  params = params or {}
  if type(params.schema) ~= "table" then
    return nil, string.format("structured-output '%s': params.schema is required", tostring(id))
  end
  if type(nefor.typed_json) ~= "table" or type(nefor.typed_json.validate) ~= "function" then
    return nil, "structured-output requires nefor.typed_json.validate"
  end
  local attempts = 0
  return boundary.construct(id, params, emit, {
    name = "structured-output",
    on_turn_start = function(state)
      attempts = 0
      state:append({
        role = "user",
        content = "Return exactly one bare JSON value and no markdown or commentary. "
          .. "It must conform to this versioned MAG type descriptor: " .. json_encode(params.schema),
      })
    end,
    on_final = function(state, result)
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
        state:finish({ kind = VALIDATED, tag = "core.validated.Valid", value = validation.value })
        return
      end
      attempts = attempts + 1
      if attempts >= MAX_ATTEMPTS then
        state:finish({ kind = VALIDATED, tag = "core.validated.Invalid",
          errors = { output_error(validation, attempts) } })
        return
      end
      if state:is_draining() then
        state:finish({ kind = VALIDATED, tag = "core.validated.Invalid",
          errors = { output_error(validation, attempts, "drained",
            "structured output was draining and could not start a correction round") } })
        return
      end
      state:append({ role = "user", content = correction(validation) })
      state:retry()
    end,
  })
end

return M
