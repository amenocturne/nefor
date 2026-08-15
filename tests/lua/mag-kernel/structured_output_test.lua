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
      local message = {
        role = fact.role, content = "", completed = false,
        visibility = fact.visibility or "transcript",
      }
      messages[#messages + 1] = message
      by_id[fact.message_id] = message
    elseif fact.kind == "content_chunk_appended" and fact.chunk.kind == "text" then
      local message = by_id[fact.message_id]
      if message then message.content = message.content .. fact.chunk.data end
    elseif fact.kind == "message_completed" then
      local message = by_id[fact.message_id]
      if message then
        message.completed = true
        if fact.visibility ~= nil then message.visibility = fact.visibility end
      end
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

  assert_eq(history[#history - 1].visibility, "diagnostic",
    "the rejected candidate is model context, never an ordinary transcript message")
  assert_eq(history[#history].visibility, "diagnostic",
    "the correction prompt is model context, never an ordinary transcript message")

  instance.deliver({ kind = "reply", ref = correction.ref,
    result = { text = '{"content":"validated"}' } })
  local result = find_last(messages, "nefor.agent.Result")
  assert_eq(result.semantic_type_id, "direct-id", "direct result identity remains compiler-derived")
  assert_eq(result.value.content, "validated", "validated result remains visible")

  local settled = conversation_messages(facts)
  local visible = {}
  for _, message in ipairs(settled) do
    if message.visibility == "transcript" then visible[#visible + 1] = message end
  end
  assert_eq(#visible, 2, "one user input and one accepted answer stay in the transcript")
  assert_eq(visible[1].role, "user", "the user's input remains the first transcript message")
  assert_eq(visible[2].role, "assistant", "exactly one accepted answer reaches the transcript")
  assert_eq(visible[2].content, "validated",
    "the accepted answer carries the decoded value, not the rejected candidate")
end

-- Streamed content of a rejected attempt is retracted rather than left behind:
-- the message the provider streamed into is closed as diagnostic.
do
  validations = {
    { ok = false, violations = {{ path = "$.content", message = "required" }} },
    { ok = true, value = { content = "second try" } },
  }
  local instance, messages, _, facts = make({ root = { kind = "record" } })
  instance.deliver(turn())
  local first = find_last(messages, "capability.invoke")
  instance.handle_observation({ binding = "transcript",
    value = { kind = "reasoning", text = "provisional thinking" } })
  instance.deliver({ kind = "reply", ref = first.ref, result = { text = "not json at all" } })

  local streamed
  for _, message in ipairs(conversation_messages(facts)) do
    if message.role == "assistant" then streamed = streamed or message end
  end
  assert_eq(streamed.visibility, "diagnostic",
    "a streamed attempt narrows to diagnostic when its content fails validation")

  local correction = find_last(messages, "capability.invoke")
  instance.deliver({ kind = "reply", ref = correction.ref,
    result = { text = '{"content":"second try"}' } })
  local accepted = 0
  for _, message in ipairs(conversation_messages(facts)) do
    if message.role == "assistant" and message.visibility == "transcript" then
      accepted = accepted + 1
    end
  end
  assert_eq(accepted, 1, "only the accepted attempt is an ordinary assistant message")
end

-- Exhausting the correction budget leaves no accepted answer behind: every
-- attempt is diagnostic and the failure rides the typed error result alone.
do
  validations = {
    { ok = false, violations = {{ path = "$", message = "bad" }} },
    { ok = false, violations = {{ path = "$", message = "still bad" }} },
  }
  local instance, messages, diagnostics, facts = make({ root = { kind = "record" } })
  instance.deliver(turn())
  local first = find_last(messages, "capability.invoke")
  instance.deliver({ kind = "reply", ref = first.ref, result = { text = "garbage one" } })
  local correction = find_last(messages, "capability.invoke")
  instance.deliver({ kind = "reply", ref = correction.ref, result = { text = "garbage two" } })

  for _, message in ipairs(conversation_messages(facts)) do
    if message.role == "assistant" then
      assert_eq(message.visibility, "diagnostic",
        "no rejected attempt survives as an ordinary assistant message")
    end
  end
  assert_eq(#diagnostics, 2, "each rejected attempt reports one validation diagnostic")
  local result = find_last(messages, "nefor.agent.Result")
  assert_eq(result.semantic_type_id, "error-id", "exhaustion settles as one typed error result")
  local completions = 0
  for _, fact in ipairs(facts) do
    if fact.kind == "turn_completed" or fact.kind == "turn_failed" then
      completions = completions + 1
    end
  end
  assert_eq(completions, 1, "exhaustion produces exactly one terminal turn fact")
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
