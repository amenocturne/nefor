local context_usage = require("libs.chat.context_usage")

local function eq(actual, expected, label)
  assert(actual == expected,
    label .. ": expected " .. tostring(expected) .. ", got " .. tostring(actual))
end

eq(context_usage.current_input_tokens({
  input_tokens = 290,
  prompt_tokens = 290,
  context_input_tokens = 105,
}), 105, "final request usage is the current context occupancy")

eq(context_usage.current_input_tokens({ input_tokens = 80 }), 80,
  "canonical aggregate input remains the fallback for providers without context usage")
eq(context_usage.current_input_tokens({ prompt_tokens = 70 }), 70,
  "legacy provider prompt usage remains a fallback")
eq(context_usage.current_input_tokens({}), nil, "missing usage remains unknown")

print("context_usage_test: all assertions passed")
