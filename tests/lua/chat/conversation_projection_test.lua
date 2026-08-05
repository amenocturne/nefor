local projection = require("libs.chat.conversation_projection")

local function eq(actual, expected, message)
  if actual ~= expected then
    error(string.format("%s: expected %s, got %s", message, tostring(expected), tostring(actual)))
  end
end

local state = projection.new()
local function reduce(body)
  local actions
  state, actions = projection.reduce(state, body)
  return actions
end

eq(#reduce({
  kind = "conversation.projection.delta",
  conversation_id = "foreign",
  change = { kind = "turn_started", turn_id = "ignored" },
}), 0, "unbound projections stay invisible")

local actions = reduce({ kind = "conversation.active.changed", conversation_id = "root" })
eq(actions[1].kind, "active_changed", "manager activation selects the root conversation")

eq(#reduce({
  kind = "conversation.projection.delta",
  conversation_id = "worker",
  change = { kind = "turn_started", turn_id = "foreign" },
}), 0, "foreign conversations stay invisible")

actions = reduce({
  kind = "conversation.projection.delta",
  conversation_id = "root",
  change = { kind = "turn_started", turn_id = "turn-1", run_id = "run-1" },
})
eq(actions[1].turn_id, "turn-1", "turn identity remains universal")

reduce({
  kind = "conversation.projection.delta", conversation_id = "root",
  change = {
    kind = "message_started", turn_id = "turn-1",
    message = { id = "assistant", turn_id = "turn-1", role = "assistant" },
  },
})
actions = reduce({
  kind = "conversation.projection.delta", conversation_id = "root",
  change = {
    kind = "content_chunk_appended", message_id = "assistant",
    chunk = { kind = "reasoning", data = "think" },
  },
})
eq(actions[1].kind, "reasoning_delta", "reasoning remains distinct")
actions = reduce({
  kind = "conversation.projection.delta", conversation_id = "root",
  change = {
    kind = "content_chunk_appended", message_id = "assistant",
    chunk = { kind = "text", data = "answer" },
  },
})
eq(actions[1].text, "answer", "text streams from the universal chunk")

actions = reduce({
  kind = "conversation.projection.delta", conversation_id = "root",
  change = {
    kind = "tool_call_completed", turn_id = "turn-1",
    exchange = {
      id = "exchange-1", name = "read_file", status = "call_completed",
      arguments = { id = "provider-call", arguments = { path = "README.md" } },
    },
  },
})
eq(actions[1].exchange_id, "exchange-1", "tool UI keys by canonical exchange id")
eq(actions[1].arguments.path, "README.md", "tool arguments survive projection")

actions = reduce({
  kind = "conversation.projection.delta", conversation_id = "root",
  change = {
    kind = "tool_result_recorded", turn_id = "turn-1",
    exchange = { id = "exchange-1", name = "read_file", status = "result", result = "ok" },
  },
})
eq(actions[1].output, "ok", "tool result survives projection")

actions = reduce({
  kind = "conversation.projection.delta", conversation_id = "root",
  change = {
    kind = "message_completed", turn_id = "turn-1",
    message = {
      id = "assistant", turn_id = "turn-1", role = "assistant",
      text = "answer", terminal = {},
    },
  },
})
eq(actions[1].kind, "assistant_completed", "message completion closes the stream")

actions = reduce({
  kind = "conversation.projection.delta", conversation_id = "root",
  change = {
    kind = "turn_completed", turn_id = "turn-1", run_id = "run-1",
    terminal = {
      model = "universal-model", duration_ms = 42,
      usage = { input_tokens = 7, output_tokens = 3 },
    },
  },
})
eq(actions[1].answer, "answer", "terminal turn carries accumulated answer")
eq(actions[1].terminal.model, "universal-model", "terminal model reaches the surface")
eq(actions[1].terminal.duration_ms, 42, "terminal duration reaches the surface")
eq(actions[1].terminal.usage.output_tokens, 3, "terminal usage reaches the surface")

actions = reduce({
  kind = "conversation.projection.delta", conversation_id = "root",
  change = {
    kind = "context_compaction_pending",
    compaction = { request_id = "compact-1", status = "pending" },
  },
})
eq(actions[1].kind, "compaction_pending", "compaction begins through manager projection")
actions = reduce({
  kind = "conversation.projection.delta", conversation_id = "root",
  change = {
    kind = "context_compaction_completed",
    compaction = { request_id = "compact-1", status = "completed" },
  },
})
eq(actions[1].kind, "compaction_completed", "compaction completes without provider artifact")

local snapshot_state = projection.new()
snapshot_state = select(1, projection.reduce(snapshot_state, {
  kind = "conversation.active.changed", conversation_id = "snapshot",
}))
local snapshot_actions
snapshot_state, snapshot_actions = projection.reduce(snapshot_state, {
  kind = "conversation.snapshot", conversation_id = "snapshot", found = true,
  projection = {
    messages = {
      { id = "user", turn_id = "turn-s", role = "user", text = "inspect" },
      {
        id = "assistant-call", turn_id = "turn-s", role = "assistant", text = "",
        tool_calls = {
          { id = "exchange-s", name = "read_file", arguments = { id = "call-s", arguments = { path = "x" } } },
        },
      },
      { id = "tool", turn_id = "turn-s", role = "tool", tool_call_id = "call-s", text = "data" },
      { id = "assistant-final", turn_id = "turn-s", role = "assistant", text = "done" },
    },
    exchanges = {
      {
        id = "exchange-s", name = "read_file", status = "result", result = "data",
        arguments = { id = "call-s", arguments = { path = "x" } },
      },
    },
    turns = {
      { id = "turn-s", run_id = "run-s", status = "completed", terminal = { model = "m" } },
    },
  },
})
local tool_end_index
local final_text_index
for index, item in ipairs(snapshot_actions) do
  if item.kind == "tool_completed" then tool_end_index = index end
  if item.kind == "text_delta" and item.text == "done" then final_text_index = index end
end
assert(tool_end_index ~= nil and final_text_index ~= nil and tool_end_index < final_text_index,
  "snapshot tool completion must remain before the following assistant message")

print("conversation_projection_test: all assertions passed")
