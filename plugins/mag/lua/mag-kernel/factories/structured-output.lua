-- Typed JSON provider boundary. Generic provider lifecycle is shared with the
-- prose llm factory; this module owns only schema instruction, validation and
-- editable drafts and explicit submission.
local boundary = require("factories.provider-boundary")
local M = {}
local RESULT = "nefor.agent.Result"

M.declaration = {
  name = "structured-output",
  type_variables = { "T", "R" },
  semantic = {
    input={kind="named",name="nefor.contracts.ProviderInput",arguments={}},
    output={kind="variable",name="R"},
    inputs = {{ wire="generic-provider.ProviderOut", type={kind="named",
      name="nefor.contracts.ProviderInput",arguments={}} }},
    outputs = {
      {wire="generic-tool.ToolCalls",type={kind="named",name="nefor.contracts.ToolCalls",arguments={}}},
      {wire=RESULT,type={kind="variable",name="R"}},
    },
  },
  params = {
    model = "string", provider = "string",
    reasoning_effort = "string?",
    provider_options = "table?",
    system = "string?", tools = "table?", history = "table?", schema = "table",
    output_type = "string", error_type = "string",
    provider_error_type = "string",
    conversation_id = "string?",
    turn_id = "string?", submission_ids = "table?", input_cause = "string?",
    authored_prompt = "string?",
    dynamic = "bool?", dynamic_item_type = "string?", dynamic_item_descriptor = "table?",
  },
  template = { relocations = {}, parameter_equals = { dynamic = false } },
  inputs = { provider_input = "generic-provider.ProviderOut" },
  outputs = { "generic-tool.ToolCalls", RESULT },
  signals = { "kill", "drain" },
}

local function json_encode(value)
  local ok, encoded = pcall(nefor.json.encode, value)
  return ok and encoded or "<unencodable>"
end

local DIAGNOSTIC_LIMIT = 512

local function bounded(value)
  value = tostring(value or "")
  local boundary = utf8.offset(value, DIAGNOSTIC_LIMIT + 1)
  if not boundary then return value end
  return value:sub(1, boundary - 1) .. "…"
end

local function excerpt(value, position)
  local start = math.max(1, (position or 1) - 80)
  while value:byte(start) and value:byte(start) >= 128 and value:byte(start) < 192 do
    start = start + 1
  end
  local finish = utf8.offset(value, 241, start)
  return value:sub(start, finish and finish - 1 or #value)
end

local function diagnostic(validation)
  if type(validation.error) == "table" then
    return bounded(tostring(validation.error.kind or "invalid") .. " at $: "
      .. tostring(validation.error.message or "invalid structured output"))
  end
  local details = {}
  local violations = validation.violations or {}
  for _, violation in ipairs(violations) do
    details[#details + 1] = string.format("%s [%s]: expected %s, got %s (%s)",
      bounded(violation.path or "$"), bounded(violation.code or "invalid"),
      bounded(violation.expected or "schema match"), bounded(violation.actual or "invalid"),
      bounded(violation.message or "validation failed"))
    if #details >= 4 then break end
  end
  if #violations > #details then
    details[#details + 1] = tostring(#violations - #details) .. " additional violations; see complete structured diagnostics"
  end
  return #details > 0 and table.concat(details, "; ") or "invalid structured output at $"
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
      or type(nefor.typed_json.validate) ~= "function"
      or type(nefor.typed_json.schema) ~= "function" then
    return nil, "structured-output requires the MAG typed JSON draft bridge"
  end
  if type(params.output_type) ~= "string" or type(params.error_type) ~= "string"
      or type(params.provider_error_type) ~= "string"
 then
    return nil, string.format(
      "structured-output '%s': compiler result constructor ids are required",
      tostring(id))
  end
  params.dynamic = params.dynamic == true
  if params.dynamic and ((params.dynamic_item_type == nil) ~= (params.dynamic_item_descriptor == nil)) then
    return nil, string.format(
      "structured-output '%s': dynamic item type and descriptor must be supplied together",
      tostring(id))
  end
  if params.dynamic and
      (type(params.dynamic_item_type) ~= "string"
        or type(params.dynamic_item_descriptor) ~= "table") then
    return nil, string.format(
      "structured-output '%s': invalid dynamic item type metadata", tostring(id))
  end
  local draft = nil
  local directory = nil
  local dynamic_sequence = 0
  local last_output = nefor.json.decode("null")
  local output_schema = nefor.typed_json.schema(params.schema)
  local provider_params = {}
  for key, value in pairs(params) do provider_params[key] = value end
  provider_params.schema = nil
  provider_params.tools = {}
  for _, name in ipairs(params.tools or {}) do
    if name == "write_output" or name == "submit_output" then
      return nil, "configured tool conflicts with intrinsic result tool: " .. name
    end
    provider_params.tools[#provider_params.tools + 1] = name
  end
  provider_params.tools[#provider_params.tools + 1] = "write_output"
  provider_params.tools[#provider_params.tools + 1] = "submit_output"
  provider_params.tool_specs = {
    { name = "write_output", owner = "mag-runtime", execution = { kind = "routed" },
      description = "Create or edit this activation's private output draft. The runtime supplies its schema and path. Without old_string, replace the whole draft; with old_string, replace exactly one matching substring. validate defaults to false. true saves first, then validates the complete resulting canonical MAG JSON (no provider-only root value envelope). Invalid edited contents remain saved for repair. A failed edit leaves the file unchanged and does not validate. Validation returns structured diagnostics and never completes the activation; submit_output separately to finish.",
      parameters = { type = "object", additionalProperties = false, required = { "new_string" },
        properties = {
          new_string = { type = "string", description = "Whole draft contents when old_string is absent, or literal replacement text for its unique match." },
          old_string = { type = "string", description = "Optional literal substring to replace. It must occur exactly once in the existing draft; zero or multiple matches fail without changing it." },
          validate = { type = "boolean", default = false, description = "Defaults false: save without validation while building an incomplete draft. Set true when expecting a complete result: save the edit then validate the whole saved draft. Invalid JSON/schema contents stay saved; validation success does not submit or finish." },
        } } },
    { name = "submit_output", owner = "mag-runtime", execution = { kind = "routed" },
      description = "Finish this activation by submitting its current private draft. Call with {} as the sole tool call in the model response. The runtime rereads and validates canonical MAG JSON against its supplied schema, then captures the accepted snapshot and completes without another model call. No path, schema, body or prior successful validation is required. Failures return structured repair diagnostics and keep the activation open. When batched with any other call, submission is rejected while other permitted calls run; use write_output with validate:true for immediate edit feedback, then submit separately.",
      parameters = { type = "object", additionalProperties = false, properties = {} } },
  }
  local function receipt(operation, status, extra)
    local result = { draft = draft, operation = operation, write = status,
      validation = { status = "not_requested" } }
    for key, value in pairs(extra or {}) do result[key] = value end
    return result
  end
  local function validate_draft()
    local read = nefor.fs.read_file(draft)
    if not read.ok then
      return { status = "operational_error", error = { code = "draft_read_failed", operation = "read",
        draft = draft, message = read.error, action = "Create the draft with write_output, or repair the file operation and try again." } }
    end
    local checked = nefor.typed_json.validate(params.schema, read.content)
    if checked.ok then return { status = "valid", value = checked.value } end
    return { status = "invalid", error = checked.error, violations = checked.violations,
      message = diagnostic(checked) }
  end
  local function write_draft(args)
    for key in pairs(args) do
      if key ~= "new_string" and key ~= "old_string" and key ~= "validate" then
        return receipt("write", "failed", { error = { code = "invalid_arguments", message = "Unknown parameter: " .. tostring(key) } })
      end
    end
    if type(args.new_string) ~= "string" or (args.old_string ~= nil and type(args.old_string) ~= "string")
        or (args.validate ~= nil and type(args.validate) ~= "boolean") then
      return receipt("write", "failed", { error = { code = "invalid_arguments", message = "new_string must be a string; old_string optional string; validate optional boolean." } })
    end
    local content = args.new_string
    if args.old_string ~= nil then
      local read = nefor.fs.read_file(draft)
      if not read.ok then return receipt("edit", "failed", { error = { code = "draft_read_failed", message = read.error, operation = "read", draft = draft, action = "Create the draft with write_output before editing it." } }) end
      local first, last = read.content:find(args.old_string, 1, true)
      if args.old_string == "" or not first or read.content:find(args.old_string, first + 1, true) then
        return receipt("edit", "failed", { error = { code = first and "non_unique_match" or "missing_match",
          message = "old_string must be nonempty and match exactly once; draft unchanged.",
          excerpt = excerpt(read.content, first) } })
      end
      content = read.content:sub(1, first - 1) .. args.new_string .. read.content:sub(last + 1)
    end
    local created = nefor.fs.mkdir_p(directory)
    if not created.ok then return receipt("write", "failed", { error = {
      code = "draft_directory_failed", operation = "mkdir", draft = draft,
      message = created.error, action = "Repair the draft directory operation and try write_output again." } }) end
    local saved = nefor.fs.write_file_atomic(draft, content)
    if not saved.ok then return receipt("write", "failed", { error = { code = "draft_write_failed", operation = "write", draft = draft, message = saved.error, action = "Restore draft file access and retry write_output." } }) end
    local result = receipt(args.old_string and "edit" or "write", "saved")
    if args.validate == true then
      result.validation = validate_draft()
      result.validation.saved = true
      result.validation.value = nil
    end
    return result
  end
  local function finish_result(state, value)
    local result_value = { constructor = "Ok", value = value }
    state:finish({ kind=RESULT, value=result_value }, {
      result = value,
      value = result_value,
    })
  end
  local function finish_dynamic(state, values)
    dynamic_sequence = dynamic_sequence + 1
    local collection = id .. "@" .. tostring(dynamic_sequence)
    local messages = {}
    for index, item in ipairs(values) do
      local semantic_value = item
      local value = item

      messages[#messages + 1] = {
        kind = RESULT,
        value = { constructor = "Ok", value = value },
        semantic_value = semantic_value,
        dynamic = { kind = "item", collection = collection, index = index - 1 },
      }
    end
    messages[#messages + 1] = {
      kind = RESULT,
      value = { constructor = "Ok", value = nefor.json.decode("null") },
      dynamic = { kind = "complete", collection = collection, count = #values },
    }
    state:finish_many(messages, {
      result = values,
      value = { constructor = "Ok", value = nefor.json.decode("null") },
      dynamic_count = #values,
    })
  end
  local function finish_error(state, reason_type, reason)
    state:finish({ kind=RESULT, value={ constructor="Error", value={
      last_output=last_output, reason={constructor=reason_type,value=reason},
    } } }, { error = reason })
  end
  return boundary.construct(id, provider_params, emit, {
    conversation = deps.conversation,
    diagnostic = deps.diagnostic,
    name = "structured-output",
    on_turn_start = function(state)
      last_output = nefor.json.decode("null")
      directory = nefor.fs.data_root() .. "/output-drafts/" .. nefor.opaque_id()
      draft = directory .. "/output.json"
      state:append({ role = "system", content = "This activation produces a typed result. Its private draft is " .. draft
        .. ". Expected canonical MAG JSON schema: " .. json_encode(output_schema)
        .. ". Write the value directly, without a provider-only root {\"value\":...} envelope. "
        .. "Use write_output to build or repair it. validate defaults false; true saves then validates the complete draft, retaining invalid edited contents. "
        .. "Validation does not complete this activation. Finish by calling submit_output({}) alone in a separate response. "
        .. 'Example: write_output({"new_string":"{\\\"count\\\":1}","validate":true}); if needed edit with old_string and new_string, then submit_output({}) in the next response. Follow the supplied schema, not this generic example.' })
    end,
    on_final = function(state, result)
      last_output = result
      local text = boundary.answer_text(result)
      if text ~= nil or state:is_streaming() then
        state:append({ role = "assistant", content = text or "" })
      end
      if state:is_draining() then state:fail("structured output drained before submission"); return end
      state:append({ role = "system", visibility = "diagnostic", content =
        "Your response ended without submitting the typed output. Continue the same assignment: finish or repair your existing draft with write_output, then call submit_output({}) as the sole tool call in its response. Do not rewrite an existing draft unnecessarily. Validation alone does not finish." })
      state:retry("output_submission_required")
    end,
    on_tool_calls = function(state, result, calls)
      last_output = result
      for _, call in ipairs(calls) do
        if call.name == "write_output" and not state:is_draining() then call.runtime_result = write_draft(call.args) end
        if call.name == "submit_output" then
          local checked
          if #calls ~= 1 then
            checked = { status = "not_requested", error = { code = "standalone_submission_required", message =
              "submit_output must be the sole tool call in its response. No submission occurred; other permitted calls execute normally. Use write_output with validate:true, then submit separately." } }
          elseif next(call.args) ~= nil then
            checked = { status = "not_requested", error = { code = "invalid_arguments", message = "submit_output takes an empty object {}." } }
          else checked = validate_draft() end
          call.runtime_result = { draft = draft, operation = "submit", submission = checked.status == "valid" and "accepted" or "rejected",
            validation = checked }
          if checked.status == "valid" then
            local value = checked.value
            checked.value = nil
            state:record_calls(result, calls)
            state:append({ role = "tool", tool_call_id = call.id, name = call.name,
              content = json_encode(call.runtime_result) })
            if params.dynamic then finish_dynamic(state, value) else finish_result(state, value) end
            return true
          end
        end
      end
      for _, call in ipairs(calls) do
        if call.runtime_result then
          local path = draft .. ".receipt-" .. nefor.opaque_id() .. ".json"
          local persisted = nefor.fs.write_file_atomic(path, json_encode(call.runtime_result))
          if persisted.ok then call.runtime_output_path = path end
        end
      end
      return false
    end,
    on_error = function(state, detail)
      finish_error(state, "ProviderError", provider_error(detail))
    end,
  })
end

return M
