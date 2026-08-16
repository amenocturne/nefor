local llm = require("factories.llm")

local function assert_eq(actual, expected, message)
  if actual ~= expected then
    error(string.format("assertion failed: %s\n  expected: %s\n  actual:   %s",
      message, tostring(expected), tostring(actual)), 2)
  end
end

local emitted = {}
local facts = {}
local function find_kind(messages, kind)
  for _, message in ipairs(messages) do
    if message.kind == kind then return message end
  end
end
local function find_last_kind(messages, kind)
  for index = #messages, 1, -1 do
    if messages[index].kind == kind then return messages[index] end
  end
end
local function turn(message)
  return {
    shape = "single",
    messages = { { from = "upstream", tag = "generic-provider.ProviderOut", message = message } },
  }
end

local instance = assert(llm.construct("tool-footer.llm", {
  provider = "p",
  output_type = "nefor.contracts.TextAnswer",
  error_type = "nefor.contracts.AgentError",
  provider_error_type = "nefor.contracts.ProviderError",
}, function(message) emitted[#emitted + 1] = message end, {
  conversation = {
    id = "tool-footer:conversation",
    turn_id = "tool-footer:turn",
    provenance = { actor_id = "tool-footer.llm", run_id = "tool-footer:run" },
    emit = function(fact) facts[#facts + 1] = fact end,
  },
}))

instance.deliver(turn({ messages = { { role = "user", content = "inspect" } } }))
instance.handle_observation({ binding = "conversation", value = {
  kind = "usage", prompt_tokens = 12, completion_tokens = 7,
  model = "round-model", duration_ms = 31,
} })
instance.deliver({ kind = "reply", ref = find_kind(emitted, "capability.invoke").ref,
  result = {
    text = "checking", finish_reason = "tool_calls",
    tool_calls = { { id = "call-footer", name = "read_file", arguments = { path = "x" } } },
  } })

local tool_completion
for _, fact in ipairs(facts) do
  if fact.kind == "message_completed" and fact.finish_reason == "tool_calls" then
    tool_completion = fact
  end
end
assert(tool_completion, "tool-call assistant entry has a terminal completion fact")
assert_eq(tool_completion.model, "round-model", "tool-call completion keeps the provider model")
assert_eq(tool_completion.duration_ms, 31, "tool-call completion keeps provider duration")
assert_eq(tool_completion.usage.output_tokens, 7, "tool-call completion normalizes output tokens")

instance.deliver(turn({ messages = { {
  role = "tool", tool_call_id = "call-footer", name = "read_file", content = "result",
} } }))
local second = find_last_kind(emitted, "capability.invoke")
instance.deliver({ kind = "reply", ref = second.ref,
  result = { text = "done", finish_reason = "stop" } })
assert(find_kind(emitted, "nefor.agent.Result"), "the final provider round still emits its answer")
assert_eq(facts[#facts].kind, "turn_completed", "the final provider round still completes the turn")
