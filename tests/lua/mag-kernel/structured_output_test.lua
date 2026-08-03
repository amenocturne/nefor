-- Focused regressions for validated-only structured-output visibility.
local structured = require("factories.structured-output")

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format("assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ", tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(value, msg)
  if not value then error("assertion failed: " .. (msg or "expected truthy"), 2) end
end

local function find_last(messages, kind)
  for i = #messages, 1, -1 do if messages[i].kind == kind then return messages[i] end end
end

local function turn()
  return { shape = "single", messages = {{ message = { messages = {
    { role = "user", content = "answer" },
  } } }} }
end

local validations = {}
nefor.typed_json = {
  provider_schema = function(_) return { schema = { type = "object" }, wrapped = true } end,
  validate_provider = function(_, _)
    local next_validation = table.remove(validations, 1)
    assert_true(next_validation ~= nil, "test supplied a validation result")
    return next_validation
  end,
}

local function make(schema)
  local messages, previews = {}, {}
  local instance, err = structured.construct("typed", {
    provider = "provider", schema = schema, max_corrections = 1,
    output_type = "direct-id", error_type = "error-id",
    provider_error_type = "provider-error-id", validation_error_type = "validation-error-id",
  }, function(message) messages[#messages + 1] = message end, {
    preview = function(operation, binding, value)
      previews[#previews + 1] = { operation = operation, binding = binding, value = value }
      return true
    end,
  })
  assert_true(instance ~= nil, err)
  return instance, messages, previews
end

-- A rejected candidate remains in provider history for correction, but preview
-- telemetry exposes diagnostics only. The validated retry result is emitted.
do
  validations = {
    { ok = false, violations = {{ path = "$.content", message = "required" }} },
    { ok = true, value = { content = "validated" } },
  }
  local instance, messages, previews = make({ root = { kind = "record" } })
  instance.deliver(turn())
  local first = find_last(messages, "capability.invoke")
  local rejected = '{"raw_machine_secret":true}'
  instance.deliver({ kind = "reply", ref = first.ref, result = { text = rejected } })

  local correction = find_last(messages, "capability.invoke")
  local history = correction.request.input.messages
  assert_eq(history[#history - 1].content, rejected,
    "rejected candidate stays in internal correction history")
  assert_true(history[#history].content:find(
    'Return only a JSON object of the form {"value": <corrected value>}.', 1, true) ~= nil,
    "retry prompt preserves wrapped provider envelope guidance")
  assert_true(history[#history].content:find(rejected, 1, true) == nil,
    "retry prompt does not repeat the rejected candidate")

  assert_eq(#previews, 1, "rejection emits one diagnostic preview item")
  assert_eq(previews[1].value.kind, "validation", "preview item is validation diagnostics")
  assert_true(previews[1].value.value.output == nil,
    "rejected raw candidate is absent from user-visible preview telemetry")

  instance.deliver({ kind = "reply", ref = correction.ref,
    result = { text = '{"content":"validated"}' } })
  local result = find_last(messages, "nefor.agent.Result")
  assert_eq(result.semantic_type_id, "direct-id", "direct result identity remains compiler-derived")
  assert_eq(result.value.content, "validated", "validated result remains visible")
end

-- Root unions retain the selected named constructor identity while applying the
-- same validated-only preview rule across a correction attempt.
do
  validations = {
    { ok = false, violations = {{ path = "$.value", message = "wrong branch" }} },
    { ok = true, value = { type = "named-branch-id", value = { content = "union ok" } } },
  }
  local instance, messages, previews = make({ root = { kind = "union", items = {} } })
  instance.deliver(turn())
  local first = find_last(messages, "capability.invoke")
  instance.deliver({ kind = "reply", ref = first.ref,
    result = { text = '{"value":{"unvalidated":"candidate"}}' } })
  local correction = find_last(messages, "capability.invoke")
  instance.deliver({ kind = "reply", ref = correction.ref,
    result = { text = '{"value":{"content":"union ok"}}' } })

  assert_true(previews[1].value.value.output == nil,
    "union rejection also omits candidate preview data")
  local result = find_last(messages, "nefor.agent.Result")
  assert_eq(result.semantic_type_id, "named-branch-id",
    "named union branch identity remains selected from validated value")
  assert_eq(result.value.content, "union ok", "validated union payload remains visible")
end
