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

local estimate = context_usage.current_input_tokens({ context_input_tokens = 123 })
eq(estimate, 123, "display projection uses the provider request estimate")

local controller = require("libs.chat.controller")
local handle_context_usage = controller.default_handlers()["conversation.provider.context_usage"]
local state = {
  provider = "chatgpt",
  conversation_id = "active-conversation",
  current_context_tokens = 80,
}
local updated = handle_context_usage({
  provider = "chatgpt",
  conversation_id = "active-conversation",
  context_input_tokens = 123,
}, state)
eq(updated.current_context_tokens, 123,
  "active conversation accepts live request occupancy")
local stale = handle_context_usage({
  provider = "chatgpt",
  conversation_id = "previous-conversation",
  context_input_tokens = 999,
}, state)
eq(stale.current_context_tokens, 80,
  "late occupancy from another conversation cannot overwrite the active statusline")
local other_provider = handle_context_usage({
  provider = "other",
  conversation_id = "active-conversation",
  context_input_tokens = 999,
}, state)
eq(other_provider.current_context_tokens, 80,
  "occupancy from another provider is ignored")

local controller_path = package.searchpath("libs.chat.controller", package.path)
local source = assert(io.open(controller_path, "r")):read("*a")
assert(not source:find("gpt%-5", 1), "chat controller must not encode model names")
assert(not source:find("272000", 1, true), "chat controller must not encode context windows")

print("context_usage_test: all assertions passed")
