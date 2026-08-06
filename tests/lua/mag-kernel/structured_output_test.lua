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

local function conversation_messages(facts)
  local messages, by_id = {}, {}
  for _, fact in ipairs(facts) do
    if fact.kind == "message_started" then
      local message = { role = fact.role, content = "", completed = false }
      messages[#messages + 1] = message
      by_id[fact.message_id] = message
    elseif fact.kind == "content_chunk_appended" and fact.chunk.kind == "text" then
      local message = by_id[fact.message_id]
      if message then message.content = message.content .. fact.chunk.data end
    elseif fact.kind == "message_completed" then
      local message = by_id[fact.message_id]
      if message then message.completed = true end
    end
  end
  local completed = {}
  for _, message in ipairs(messages) do
    if message.completed then completed[#completed + 1] = message end
  end
  return completed
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
  local messages, diagnostics, facts = {}, {}, {}
  local instance, err = structured.construct("typed", {
    provider = "provider", schema = schema, max_corrections = 1,
    output_type = "direct-id", error_type = "error-id",
    provider_error_type = "provider-error-id", validation_error_type = "validation-error-id",
  }, function(message) messages[#messages + 1] = message end, {
    conversation = {
      id = "typed:conversation",
      turn_id = "typed:turn",
      emit = function(fact) facts[#facts + 1] = fact end,
    },
    diagnostic = function(value)
      diagnostics[#diagnostics + 1] = value
      return true
    end,
  })
  assert_true(instance ~= nil, err)
  return instance, messages, diagnostics, facts
end

-- A rejected candidate remains in provider history for correction, but the
-- generic diagnostic fact exposes validation details only.
do
  validations = {
    { ok = false, violations = {{ path = "$.content", message = "required" }} },
    { ok = true, value = { content = "validated" } },
  }
  local instance, messages, diagnostics, facts = make({ root = { kind = "record" } })
  instance.deliver(turn())
  local first = find_last(messages, "capability.invoke")
  local rejected = '{"raw_machine_secret":true}'
  instance.deliver({ kind = "reply", ref = first.ref, result = { text = rejected } })

  local correction = find_last(messages, "capability.invoke")
  local history = conversation_messages(facts)
  assert_eq(history[#history - 1].content, rejected,
    "rejected candidate stays in canonical correction history")
  assert_true(history[#history].content:find(
    'Return only a JSON object of the form {"value": <corrected value>}.', 1, true) ~= nil,
    "retry prompt preserves wrapped provider envelope guidance")
  assert_true(history[#history].content:find(rejected, 1, true) == nil,
    "retry prompt does not repeat the rejected candidate")
  assert_true(correction.request.input == nil,
    "correction invocation does not duplicate canonical history")

  assert_eq(#diagnostics, 1, "rejection emits one generic diagnostic fact")
  assert_eq(diagnostics[1].kind, "validation", "diagnostic identifies validation")
  assert_true(diagnostics[1].output == nil,
    "rejected raw candidate is absent from the diagnostic interface")

  instance.deliver({ kind = "reply", ref = correction.ref,
    result = { text = '{"content":"validated"}' } })
  local result = find_last(messages, "nefor.agent.Result")
  assert_eq(result.semantic_type_id, "direct-id", "direct result identity remains compiler-derived")
  assert_eq(result.value.content, "validated", "validated result remains visible")
end

-- Root unions retain the selected named constructor identity while applying the
-- same validated-only diagnostic rule across a correction attempt.
do
  validations = {
    { ok = false, violations = {{ path = "$.value", message = "wrong branch" }} },
    { ok = true, value = { type = "named-branch-id", value = { content = "union ok" } } },
  }
  local instance, messages, diagnostics = make({ root = { kind = "union", items = {} } })
  instance.deliver(turn())
  local first = find_last(messages, "capability.invoke")
  instance.deliver({ kind = "reply", ref = first.ref,
    result = { text = '{"value":{"unvalidated":"candidate"}}' } })
  local correction = find_last(messages, "capability.invoke")
  instance.deliver({ kind = "reply", ref = correction.ref,
    result = { text = '{"value":{"content":"union ok"}}' } })

  assert_true(diagnostics[1].output == nil,
    "union rejection also omits candidate diagnostic data")
  local result = find_last(messages, "nefor.agent.Result")
  assert_eq(result.semantic_type_id, "named-branch-id",
    "named union branch identity remains selected from validated value")
  assert_eq(result.value.content, "union ok", "validated union payload remains visible")
end
