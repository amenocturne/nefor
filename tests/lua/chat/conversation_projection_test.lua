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
    kind = "tool_exchange_started", turn_id = "turn-1",
    exchange = {
      id = "exchange-1", name = "read_file", status = "call_open",
    },
  },
})
eq(#actions, 1, "an open canonical exchange creates one ordinary tool entry")
eq(actions[1].exchange_id, "exchange-1", "tool UI keys by canonical exchange id")
eq(next(actions[1].arguments), nil, "an open tool entry starts with neutral arguments")

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
eq(#actions, 1, "call completion enriches the existing ordinary tool entry")
eq(actions[1].exchange_id, "exchange-1", "completion preserves canonical exchange identity")
eq(actions[1].arguments.path, "README.md", "tool arguments survive projection")

actions = reduce({
  kind = "conversation.projection.delta", conversation_id = "root",
  change = {
    kind = "tool_result_recorded", turn_id = "turn-1",
    exchange = {
      id = "exchange-1", name = "read_file", status = "result", result = "ok",
      completion_delivery = "async",
    },
  },
})
eq(actions[1].output, "ok", "tool result survives projection")
eq(actions[1].completion_delivery, "async",
  "completion delivery survives the surface projection")

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

local function structured_user_actions(chunk_data, completed)
  local structured = projection.new()
  structured = select(1, projection.reduce(structured, {
    kind = "conversation.active.changed", conversation_id = "structured",
  }))
  structured = select(1, projection.reduce(structured, {
    kind = "conversation.projection.delta", conversation_id = "structured",
    change = { kind = "message_started", message = {
      id = "structured-user", turn_id = "structured-turn", role = "user",
    } },
  }))
  structured = select(1, projection.reduce(structured, {
    kind = "conversation.projection.delta", conversation_id = "structured",
    change = { kind = "content_chunk_appended", message_id = "structured-user",
      chunk = { kind = "structured", data = chunk_data } },
  }))
  local produced
  structured, produced = projection.reduce(structured, {
    kind = "conversation.projection.delta", conversation_id = "structured",
    change = { kind = "message_completed", message = completed },
  })
  return produced
end

local task_data = { value = { prompt = "session-derived prompt" },
  mag_type = { version = 1, root = {
    kind = "named", name = "example.LeadTurnInput",
  } } }
actions = structured_user_actions(task_data, {
  id = "structured-user", turn_id = "structured-turn", role = "user", text = "",
  authored_prompt = "session-derived prompt",
})
eq(actions[1].text, "session-derived prompt",
  "live structured records prefer their producer-owned display text")
eq(actions[1].message_id, "structured-user", "live structured display preserves message identity")

actions = structured_user_actions({ value = { prompt = "not a task" },
  mag_type = { version = 1, root = { kind = "named", name = "Other" } } }, {
  id = "structured-user", turn_id = "structured-turn", role = "user", text = "",
})
eq(actions[1].text, [[{"prompt":"not a task"}]],
  "local domain records do not require a privileged nominal type")
actions = structured_user_actions({ value = { prompt = 42 },
  mag_type = { version = 1, root = {
    kind = "named", name = "example.LeadTurnInput",
  } } }, {
  id = "structured-user", turn_id = "structured-turn", role = "user", text = "",
})
eq(actions[1].text, [[{"prompt":42}]],
  "generic structured display does not impose Task field semantics")

local snapshot_state = projection.new()
snapshot_state = select(1, projection.reduce(snapshot_state, {
  kind = "conversation.active.changed", conversation_id = "snapshot",
}))
local snapshot_actions
snapshot_state, snapshot_actions = projection.reduce(snapshot_state, {
  kind = "conversation.snapshot", conversation_id = "snapshot", found = true,
  projection = {
    messages = {
      { id = "user", history_id = { 1 }, turn_id = "turn-s", role = "user", status = "completed", text = "",
        authored_prompt = "session-derived prompt", structured = { task_data },
        submission_ids = { "submission-s" } },
      { id = "generic-user", history_id = { 1, 1 }, role = "user", status = "completed",
        text = "", structured = { { value = { prompt = "not authored" } } } },
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
local snapshot_user
local generic_user
for index, item in ipairs(snapshot_actions) do
  if item.kind == "message" and item.message_id == "user" then snapshot_user = item end
  if item.kind == "message" and item.message_id == "generic-user" then generic_user = item end
  if item.kind == "tool_completed" then tool_end_index = index end
  if item.kind == "text_delta" and item.text == "done" then final_text_index = index end
end
eq(snapshot_user.text, "session-derived prompt", "snapshot uses the same authored prompt projection")
eq(snapshot_state.user_prompts[1].text, "session-derived prompt",
  "rewind picker stores only the authored prompt text")
eq(#snapshot_state.user_prompts, 1,
  "generic structured records remain visible but are not reinterpreted as rewind prompts")
eq(generic_user.text, [[{"prompt":"not authored"}]],
  "arbitrary structured records retain generic JSON transcript display")
eq(snapshot_user.submission_ids[1], "submission-s", "snapshot preserves submission identity")
assert(tool_end_index ~= nil and final_text_index ~= nil and tool_end_index < final_text_index,
  "snapshot tool completion must remain before the following assistant message")

local internal_state = projection.new()
internal_state = select(1, projection.reduce(internal_state, {
  kind = "conversation.active.changed", conversation_id = "internal",
}))
local internal_actions
internal_state, internal_actions = projection.reduce(internal_state, {
  kind = "conversation.snapshot", conversation_id = "internal", found = true,
  projection = {
    messages = {
      { id = "authored", turn_id = "turn-authored", role = "user", text = "real question" },
      {
        id = "completion", turn_id = "turn-completion", role = "user",
        input_cause = "internal_async_completion",
        text = "[mag_run(run_name=inspect, run_id=run-1) result] raw semantic output",
      },
      { id = "answer", turn_id = "turn-completion", role = "assistant", text = "result summary" },
    },
    turns = {}, exchanges = {}, compactions = {},
  },
})
local visible_users = {}
local visible_answer = false
for _, item in ipairs(internal_actions) do
  if item.kind == "message" then visible_users[#visible_users + 1] = item.text end
  if item.kind == "text_delta" and item.text == "result summary" then visible_answer = true end
end
eq(#visible_users, 1, "snapshot hides internal asynchronous completion inputs")
eq(visible_users[1], "real question", "snapshot preserves genuine user-authored messages")
eq(visible_answer, true, "snapshot preserves the agent response to an internal completion")

local live_state = projection.new()
live_state = select(1, projection.reduce(live_state, {
  kind = "conversation.active.changed", conversation_id = "internal-live",
}))
live_state = select(1, projection.reduce(live_state, {
  kind = "conversation.projection.delta", conversation_id = "internal-live",
  change = { kind = "message_started", message = {
    id = "completion-live", role = "user", input_cause = "internal_async_completion",
  } },
}))
local live_actions
live_state, live_actions = projection.reduce(live_state, {
  kind = "conversation.projection.delta", conversation_id = "internal-live",
  change = { kind = "message_completed", message = {
    id = "completion-live", role = "user", input_cause = "internal_async_completion",
    text = "[mag_run(run_id=run-live) result] raw semantic output",
  } },
})
eq(#live_actions, 0, "live projection also hides internal asynchronous completion inputs")

-- A structured-output attempt that fails validation is model context, not
-- conversation. Nothing it streamed may survive in the transcript, and the
-- correction prompt answering it is never a user bubble.
local diagnostic_state = projection.new()
local function diagnostic_reduce(body)
  local produced
  diagnostic_state, produced = projection.reduce(diagnostic_state, body)
  return produced
end
diagnostic_reduce({ kind = "conversation.active.changed", conversation_id = "typed" })
diagnostic_reduce({
  kind = "conversation.projection.delta", conversation_id = "typed",
  change = { kind = "turn_started", turn_id = "typed-turn", run_id = "typed-run" },
})
diagnostic_reduce({
  kind = "conversation.projection.delta", conversation_id = "typed",
  change = { kind = "message_started", turn_id = "typed-turn", message = {
    id = "attempt-1", turn_id = "typed-turn", role = "assistant",
  } },
})
actions = diagnostic_reduce({
  kind = "conversation.projection.delta", conversation_id = "typed",
  change = { kind = "content_chunk_appended", message_id = "attempt-1",
    chunk = { kind = "reasoning", data = "provisional thinking" } },
})
eq(actions[1].kind, "reasoning_delta", "a provisional attempt still streams while undecided")
actions = diagnostic_reduce({
  kind = "conversation.projection.delta", conversation_id = "typed",
  change = { kind = "message_completed", turn_id = "typed-turn", message = {
    id = "attempt-1", turn_id = "typed-turn", role = "assistant",
    visibility = "diagnostic", text = "not json at all",
  } },
})
eq(#actions, 1, "a retracted attempt produces no completion action")
eq(actions[1].kind, "message_discarded", "a retracted attempt is discarded from the transcript")
eq(actions[1].message_id, "attempt-1", "the discard names the retracted message")

diagnostic_reduce({
  kind = "conversation.projection.delta", conversation_id = "typed",
  change = { kind = "message_started", turn_id = "typed-turn", message = {
    id = "transport-attempt", turn_id = "typed-turn", role = "assistant",
  } },
})
diagnostic_reduce({
  kind = "conversation.projection.delta", conversation_id = "typed",
  change = { kind = "content_chunk_appended", message_id = "transport-attempt",
    chunk = { kind = "text", data = "provisional" } },
})
actions = diagnostic_reduce({
  kind = "conversation.projection.delta", conversation_id = "typed",
  change = { kind = "message_interrupted", turn_id = "typed-turn", message = {
    id = "transport-attempt", turn_id = "typed-turn", role = "assistant",
    visibility = "discarded", text = "provisional",
  } },
})
eq(actions[1].kind, "message_discarded", "an abandoned transport attempt is retracted")

diagnostic_reduce({
  kind = "conversation.projection.delta", conversation_id = "typed",
  change = { kind = "message_started", turn_id = "typed-turn", message = {
    id = "correction", turn_id = "typed-turn", role = "user", visibility = "diagnostic",
  } },
})
actions = diagnostic_reduce({
  kind = "conversation.projection.delta", conversation_id = "typed",
  change = { kind = "content_chunk_appended", message_id = "correction",
    chunk = { kind = "text", data = "Your previous response was not valid" } },
})
eq(#actions, 0, "a diagnostic correction never streams into the transcript")
actions = diagnostic_reduce({
  kind = "conversation.projection.delta", conversation_id = "typed",
  change = { kind = "message_completed", turn_id = "typed-turn", message = {
    id = "correction", turn_id = "typed-turn", role = "user", visibility = "diagnostic",
    text = "Your previous response was not valid",
  } },
})
eq(#actions, 0, "a correction that never streamed is not even discarded")

diagnostic_reduce({
  kind = "conversation.projection.delta", conversation_id = "typed",
  change = { kind = "message_started", turn_id = "typed-turn", message = {
    id = "attempt-2", turn_id = "typed-turn", role = "assistant",
  } },
})
actions = diagnostic_reduce({
  kind = "conversation.projection.delta", conversation_id = "typed",
  change = { kind = "message_completed", turn_id = "typed-turn", message = {
    id = "attempt-2", turn_id = "typed-turn", role = "assistant", text = "validated",
  } },
})
eq(actions[1].kind, "assistant_completed", "the accepted attempt completes normally")
eq(actions[1].text, "validated", "the accepted attempt carries the decoded answer")

-- Replay reaches the same transcript: retracted attempts stay out of it.
local replay_state = projection.new()
replay_state = select(1, projection.reduce(replay_state, {
  kind = "conversation.active.changed", conversation_id = "typed-replay",
}))
local replay_actions
replay_state, replay_actions = projection.reduce(replay_state, {
  kind = "conversation.snapshot", conversation_id = "typed-replay", found = true,
  projection = {
    messages = {
      { id = "input", turn_id = "r", role = "user", text = "question" },
      { id = "rejected", turn_id = "r", role = "assistant", visibility = "diagnostic",
        text = "not json at all", reasoning = "provisional thinking" },
      { id = "correction", turn_id = "r", role = "user", visibility = "diagnostic",
        text = "Your previous response was not valid" },
      { id = "accepted", turn_id = "r", role = "assistant", text = "validated" },
    },
    turns = {}, exchanges = {}, compactions = {},
  },
})
local replayed_text, replayed_users, replayed_reasoning = {}, {}, 0
for _, item in ipairs(replay_actions) do
  if item.kind == "text_delta" then replayed_text[#replayed_text + 1] = item.text end
  if item.kind == "reasoning_delta" then replayed_reasoning = replayed_reasoning + 1 end
  if item.kind == "message" then replayed_users[#replayed_users + 1] = item.text end
end
eq(#replayed_text, 1, "replay shows exactly one assistant answer")
eq(replayed_text[1], "validated", "replay shows the accepted answer")
eq(replayed_reasoning, 0, "replay shows no reasoning from a retracted attempt")
eq(#replayed_users, 1, "replay shows only the user's own input")
eq(replayed_users[1], "question", "replay preserves the genuine user message")

-- Rewind deltas rebuild the active transcript and retain the exact pending
-- draft; the same state is reconstructed from a resumed snapshot.
do
  local state = projection.new()
  state = select(1, projection.reduce(state, {
    kind = "conversation.active.changed", conversation_id = "rewind",
  }))
  local rewound = {
    messages = {
      { id = "u1", history_id = { 1 }, role = "user", status = "completed",
        text = "first", display_text = "first" },
      { id = "a1", history_id = { 1, 1 }, role = "assistant", status = "completed",
        text = "answer", display_text = "answer" },
    },
    exchanges = {}, turns = {}, compactions = {}, history_head = { 1, 1 },
    pending_edit = { source_history_id = { 1, 1, 1 }, text = "second\nline" },
  }
  local actions
  state, actions = projection.reduce(state, {
    kind = "conversation.projection.delta", conversation_id = "rewind", sequence = 9,
    change = { kind = "rewind_committed", projection = rewound,
      pending_edit = rewound.pending_edit, history_head = rewound.history_head },
  })
  eq(state.pending_edit.text, "second\nline")
  eq(#state.user_prompts, 1)
  eq(state.user_prompts[1].text, "first")
  eq(state.history_head[2], 1)
  eq(actions[1].kind, "snapshot_reset")
  eq(actions[#actions].kind, "rewind_restored")
  eq(actions[#actions].pending_edit.text, "second\nline")

  local resumed = projection.new()
  resumed = select(1, projection.reduce(resumed, {
    kind = "conversation.active.changed", conversation_id = "rewind",
  }))
  local resumed_actions
  resumed, resumed_actions = projection.reduce(resumed, {
    kind = "conversation.snapshot", conversation_id = "rewind", found = true,
    projection = rewound,
  })
  eq(resumed.pending_edit.text, state.pending_edit.text,
    "resume restores the pending source draft")
  eq(resumed_actions[#resumed_actions].kind, "rewind_restored")

  local cleared_actions
  resumed, cleared_actions = projection.reduce(resumed, {
    kind = "conversation.projection.delta", conversation_id = "rewind", sequence = 10,
    change = { kind = "message_started", message = {
      id = "u2-replacement", history_id = { 1, 2 }, role = "user",
    } },
  })
  eq(resumed.pending_edit, nil)
  eq(cleared_actions[1].kind, "rewind_cleared",
    "replacement submission clears a replay-restored draft")

  local interrupted = projection.new()
  interrupted = select(1, projection.reduce(interrupted, {
    kind = "conversation.active.changed", conversation_id = "interrupted",
  }))
  interrupted = select(1, projection.reduce(interrupted, {
    kind = "conversation.snapshot", conversation_id = "interrupted", found = true,
    projection = {
      messages = {
        { id = "u-open", history_id = { 1 }, role = "user", status = "interrupted",
          visibility = "transcript", text = "partial", display_text = "partial" },
      },
      exchanges = {}, turns = {}, compactions = {}, history_head = { 1 },
    },
  }))
  eq(#interrupted.user_prompts, 0,
    "picker excludes user messages the manager cannot rewind")
end

print("conversation_projection_test: all assertions passed")
