local runtime = require("libs.conversation-manager.runtime")
local json = nefor.json

local emitted = {}
local replaying = false
local actor = runtime.build({
  json = json,
  now = function() return "2026-08-05T00:00:00Z" end,
  emit = function(payload) emitted[#emitted + 1] = payload end,
  replay_active = function() return replaying end,
})

local function envelope(body, origin, target)
  return {
    origin = origin or "test",
    target = target,
    payload = json.encode({ type = "event", from = origin or "test", ts = "t", body = body }),
  }
end

local function receive(body, origin, target)
  actor.receive_msg(envelope(body, origin, target))
end

local function last_body()
  local decoded = json.decode(emitted[#emitted])
  return decoded.body
end

local function body_at(index)
  return json.decode(emitted[index]).body
end

local function eq(actual, expected, message)
  assert(actual == expected, (message or "values differ")
    .. ": expected " .. tostring(expected) .. ", got " .. tostring(actual))
end

receive({ kind = "sessions.session_start", session_id = "session-1" })
receive({
  kind = "conversation.fact.append",
  fact = {
    event_id = "create-1",
    conversation_id = "conversation-1",
    kind = "created",
    provenance = { session = "session-1", purpose = "lead" },
  },
})

local recorded = body_at(#emitted - 1)
eq(recorded.kind, "conversation.fact.recorded")
eq(recorded.event.sequence, 1, "manager assigns first sequence")
eq(recorded.event.event_id, "create-1", "recorded ack preserves idempotency key")
eq(recorded.session_id, "session-1")
eq(last_body().kind, "conversation.projection.delta")
eq(last_body().change.kind, "conversation_created")

-- The manager sees its own canonical bus echo; applying it is an exact no-op.
receive(recorded, "conversation-manager")
eq(actor._internals.get("conversation-1").last_sequence, 1)

receive({
  kind = "conversation.fact.append",
  fact = {
    event_id = "message-1",
    conversation_id = "conversation-1",
    kind = "message_started",
    message_id = "user-1",
    role = "user",
  },
})
eq(body_at(#emitted - 1).event.sequence, 2, "manager owns contiguous sequencing")

-- Exact producer retries acknowledge the existing canonical event.
receive({
  kind = "conversation.fact.append",
  fact = {
    event_id = "message-1",
    conversation_id = "conversation-1",
    kind = "message_started",
    message_id = "user-1",
    role = "user",
  },
})
eq(last_body().duplicate, true)
eq(last_body().event.sequence, 2)

receive({
  kind = "conversation.fact.append",
  fact = {
    event_id = "message-1",
    conversation_id = "conversation-1",
    kind = "message_started",
    message_id = "different",
    role = "user",
  },
})
eq(last_body().kind, "conversation.fact.rejected")
eq(last_body().code, "event_id_conflict")
eq(actor._internals.get("conversation-1").last_sequence, 2)

receive({ kind = "conversation.get.request", request_id = "get-1", conversation_id = "conversation-1" })
local snapshot = last_body()
eq(snapshot.kind, "conversation.snapshot")
eq(snapshot.found, true)
eq(snapshot.projection.last_sequence, 2)

receive({ kind = "conversation.list.request", request_id = "list-1" })
eq(last_body().kind, "conversation.list")
eq(#last_body().conversations, 1)

-- Fan-out copies are not a second logical delivery.
local before_fanout = #emitted
actor.receive_msg(envelope({ kind = "conversation.list.request", request_id = "ignored" }, "step", "peer"))
eq(#emitted, before_fanout)

-- Session transition clears the projection before incoming replay rebuilds it.
receive({ kind = "sessions.session_end", session_id = "session-1" })
eq(#actor._internals.list(), 0)
receive({ kind = "sessions.session_start", session_id = "session-2" })
eq(actor._internals.session_id(), "session-2")

replaying = true
local before_command = #emitted
receive({
  kind = "conversation.fact.append",
  fact = { event_id = "ignored", conversation_id = "old", kind = "created" },
})
eq(#emitted, before_command, "replayed command is side-effect free")

receive({
  kind = "conversation.fact.recorded",
  event = {
    event_id = "replayed-create", conversation_id = "replayed", kind = "created",
    sequence = 1, provenance = { session = "session-2" },
  },
}, "conversation-manager")
eq(actor._internals.get("replayed").last_sequence, 1, "recorded facts rebuild during replay")
eq(last_body().kind, "conversation.projection.delta")
eq(last_body().replay, true)

local after_replay_projection = #emitted
receive({ kind = "conversation.get.request", request_id = "replayed-query", conversation_id = "replayed" })
eq(#emitted, after_replay_projection, "replayed query emits no stale projection")
replaying = false

-- Corrupt canonical history is visible and does not partially mutate state.
receive({
  kind = "conversation.fact.recorded",
  event = {
    event_id = "gap", conversation_id = "replayed", kind = "message_started",
    sequence = 3, message_id = "m", role = "user",
  },
}, "conversation-manager")
local diagnostic = last_body()
eq(diagnostic.kind, "conversation-manager.diagnostic")
eq(diagnostic.code, "noncontiguous_sequence")
eq(diagnostic.context.expected, 2)
eq(actor._internals.get("replayed").last_sequence, 1)

receive({
  kind = "conversation.fact.recorded",
  event = {
    event_id = "forged", conversation_id = "forged", kind = "created",
    sequence = 1,
  },
}, "other-actor")
eq(last_body().kind, "conversation-manager.diagnostic")
eq(last_body().code, "recorded_fact_source_mismatch")
eq(actor._internals.get("forged"), nil, "only manager-authored facts are canonical")

local function append_fact(event_id, kind, fields)
  local fact = { event_id = event_id, conversation_id = "replayed", kind = kind }
  for key, value in pairs(fields or {}) do fact[key] = value end
  receive({ kind = "conversation.fact.append", fact = fact })
  eq(body_at(#emitted - 1).kind, "conversation.fact.recorded")
  eq(last_body().kind, "conversation.projection.delta")
  return last_body()
end

local delta = append_fact("turn", "turn_started", { turn_id = "turn-1", run_id = "run-1" })
eq(delta.change.kind, "turn_started")
eq(delta.change.run_id, "run-1")
append_fact("user", "message_started", {
  turn_id = "turn-1", message_id = "user", role = "user",
})
delta = append_fact("user-text", "content_chunk_appended", {
  message_id = "user", chunk = { kind = "text", data = "question" },
})
eq(delta.change.chunk.data, "question")
append_fact("user-done", "message_completed", { message_id = "user" })

append_fact("assistant", "message_started", {
  turn_id = "turn-1", message_id = "assistant", role = "assistant",
})
append_fact("reasoning", "content_chunk_appended", {
  message_id = "assistant", chunk = { kind = "reasoning", data = "think" },
})
append_fact("answer", "content_chunk_appended", {
  message_id = "assistant", chunk = { kind = "text", data = "answer" },
})
append_fact("tool-start", "tool_exchange_started", {
  exchange_id = "call-1", message_id = "assistant", tool_name = "read_file",
})
append_fact("tool-call", "tool_call_completed", {
  exchange_id = "call-1", call = { path = "x" },
})
append_fact("tool-result", "tool_result_recorded", {
  exchange_id = "call-1", result = { text = "data" },
})
delta = append_fact("assistant-done", "message_completed", {
  message_id = "assistant", model = "universal-model", duration_ms = 42,
  usage = { input_tokens = 7, output_tokens = 3 }, finish_reason = "tool_calls",
})
eq(delta.change.message.text, "answer")
eq(delta.change.message.reasoning, "think")
eq(delta.change.message.tool_calls[1].id, "call-1")
eq(delta.change.message.terminal.duration_ms, 42)
eq(#delta.change.context_messages, 2, "assistant plus linked tool result enter context")

delta = append_fact("turn-done", "turn_completed", {
  turn_id = "turn-1", run_id = "run-1", terminal = { output = "answer" },
})
eq(delta.change.kind, "turn_completed")
eq(delta.change.turn_id, "turn-1")
eq(delta.change.run_id, "run-1")
eq(delta.change.message_start, 1)
eq(delta.change.message_end, 2)
eq(delta.change.watermark, body_at(#emitted - 1).event.sequence)

receive({ kind = "conversation.context.request", request_id = "context-1", conversation_id = "replayed" })
local context = last_body()
eq(context.kind, "conversation.context.snapshot")
eq(context.context.history_length, 3, "user, assistant, and tool result are universal context")
eq(context.context.messages[2].tool_calls[1].name, "read_file")

receive({ kind = "conversation.context.compact.request", request_id = "compact-1", conversation_id = "replayed" })
delta = last_body()
eq(delta.change.kind, "context_compaction_pending")
eq(delta.change.compaction.history_cutoff, 3)
receive({
  kind = "conversation.context.compact.complete",
  request_id = "compact-1",
  conversation_id = "replayed",
  checkpoint = { opaque = "provider-owned" },
  compatibility = { family = "universal-v1" },
})
delta = last_body()
eq(delta.change.kind, "context_compaction_completed")
eq(delta.change.compaction.checkpoint, nil, "opaque checkpoint stays out of broadcast deltas")

receive({
  kind = "conversation.context.request",
  request_id = "context-2",
  conversation_id = "replayed",
  compatibility = { family = "universal-v1" },
})
context = last_body().context
eq(#context.messages, 3, "full neutral history remains available for incompatible providers")
eq(#context.tail_messages, 0, "compatible checkpoint tail begins after its cutoff")
eq(context.compaction.checkpoint.opaque, "provider-owned")
