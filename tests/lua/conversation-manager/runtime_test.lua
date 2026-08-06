local runtime = require("libs.conversation-manager.runtime")
local service = require("libs.conversation-manager.service").new()
local json = nefor.json

local emitted = {}
local replaying = false
local actor = runtime.build({
  json = json,
  now = function() return "2026-08-05T00:00:00Z" end,
  emit = function(payload) emitted[#emitted + 1] = payload end,
  replay_active = function() return replaying end,
  service = service,
})
local reader = service:reader()

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
eq(reader:watermark("conversation-1"), 1, "injected reader sees manager writes")

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

-- A targeted engine send is itself the sole logical Step entry. The actor
-- runtime exposes it to every Lua actor; the addressed actor must not mistake
-- it for a duplicate fan-out delivery and drop the command.
actor.receive_msg(envelope({
  kind = "conversation.list.request",
  request_id = "targeted-list",
}, "step", "conversation-manager"))
eq(last_body().kind, "conversation.list")
eq(last_body().request_id, "targeted-list")

-- Session transition clears the projection before incoming replay rebuilds it.
receive({ kind = "sessions.session_end", session_id = "session-1" })
eq(#actor._internals.list(), 0)
eq(reader:watermark("conversation-1"), nil, "session reset updates the existing reader")
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
eq(reader:watermark("replayed"), 1, "replay rebuilds the shared read service")
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
delta = append_fact("tool-call", "tool_call_completed", {
  exchange_id = "call-1",
  call = { id = "provider-call-42", name = "read_file", arguments = { path = "x" } },
})
eq(delta.change.exchange.id, "call-1", "universal projections preserve the exchange identity")
append_fact("tool-result", "tool_result_recorded", {
  exchange_id = "call-1", result = { text = "data" },
})
delta = append_fact("assistant-done", "message_completed", {
  message_id = "assistant", model = "universal-model", duration_ms = 42,
  usage = { input_tokens = 7, output_tokens = 3 }, finish_reason = "tool_calls",
})
eq(delta.change.message.text, "answer")
eq(delta.change.message.reasoning, "think")
eq(delta.change.message.tool_calls[1].id, "provider-call-42")
eq(delta.change.message.tool_calls[1].arguments.path, "x")
eq(delta.change.message.terminal.duration_ms, 42)
eq(#delta.change.context_messages, 1, "completed assistant enters context once")

append_fact("tool-message", "message_started", {
  turn_id = "turn-1", message_id = "tool-message", role = "tool",
  tool_call_id = "provider-call-42", tool_name = "read_file",
})
append_fact("tool-content", "content_chunk_appended", {
  message_id = "tool-message", chunk = { kind = "structured", data = { text = "data" } },
})
delta = append_fact("tool-message-done", "message_completed", { message_id = "tool-message" })
eq(delta.change.message.tool_call_id, "provider-call-42")
eq(delta.change.message.content.text, "data", "structured tool content stays structured")
eq(#delta.change.context_messages, 1, "actual tool message enters context once")

delta = append_fact("turn-done", "turn_completed", {
  turn_id = "turn-1", run_id = "run-1", terminal = { output = "answer" },
})
eq(delta.change.kind, "turn_completed")
eq(delta.change.turn_id, "turn-1")
eq(delta.change.run_id, "run-1")
eq(delta.change.message_start, 1)
eq(delta.change.message_end, 3)
eq(delta.change.watermark, body_at(#emitted - 1).event.sequence)

receive({ kind = "conversation.context.request", request_id = "context-1", conversation_id = "replayed" })
local context = last_body()
eq(context.kind, "conversation.context.snapshot")
eq(context.context.history_length, 3, "user, assistant, and tool result are universal context")
eq(context.context.messages[2].tool_calls[1].name, "read_file")
eq(context.context.messages[2].tool_calls[1].id, "provider-call-42")
eq(context.context.messages[3].tool_call_id, "provider-call-42")
eq(context.context.messages[3].content.text, "data")
eq(context.context.watermark, actor._internals.get("replayed").last_sequence)

receive({
  kind = "conversation.context.compact.request", request_id = "compact-1",
  conversation_id = "replayed", provider = "chatgpt",
})
delta = last_body()
eq(delta.change.kind, "context_compaction_pending")
eq(delta.change.compaction.history_cutoff, 3)
eq(delta.change.compaction.provider, "chatgpt", "pending compaction preserves routing provider")
receive({
  kind = "conversation.context.compact.complete",
  request_id = "compact-1",
  conversation_id = "replayed",
  checkpoint = { opaque = "provider-owned" },
})
delta = last_body()
eq(delta.change.kind, "context_compaction_completed")
eq(delta.change.compaction.checkpoint, nil, "opaque checkpoint stays out of broadcast deltas")

receive({
  kind = "conversation.context.request",
  request_id = "context-2",
  conversation_id = "replayed",
})
context = last_body().context
eq(#context.messages, 3, "full neutral history remains universally available")
eq(#context.tail_messages, 0, "checkpoint tail begins after its cutoff")
eq(context.compaction.checkpoint.opaque, "provider-owned")
eq(context.compaction.provider, "chatgpt")

receive({ kind = "conversation.active.set", request_id = "active-1", conversation_id = "replayed" })
local active_changed = last_body()
eq(active_changed.kind, "conversation.active.changed")
eq(active_changed.conversation_id, "replayed")
eq(actor._internals.active_conversation_id(), "replayed")
eq(body_at(#emitted - 1).kind, "conversation.projection.delta")
eq(body_at(#emitted - 2).kind, "conversation.fact.recorded", "active selection is replay-safe canonical state")

-- Provider invocations are manager-mediated only after their preceding facts
-- have folded. The public event stays thin; providers obtain history through
-- the injected reader at the published watermark.
receive({
  kind = "conversation.provider.invoke.request",
  conversation_id = "replayed",
  provider = "chatgpt",
})
eq(last_body().kind, "conversation.provider.invoke.rejected")
eq(last_body().code, "invalid_provider_invoke_request")
receive({
  kind = "conversation.provider.invoke.request",
  request_id = "provider-missing-conversation",
  conversation_id = "missing",
  provider = "chatgpt",
})
eq(last_body().kind, "conversation.provider.invoke.rejected")
eq(last_body().code, "conversation_not_found")

local invoke_watermark = reader:watermark("replayed")
receive({
  kind = "conversation.provider.invoke.request",
  request_id = "provider-1",
  conversation_id = "replayed",
  provider = "chatgpt",
  model = "gpt-test",
  reasoning_effort = "high",
  tools = { "read_file" },
  system = "must not leak",
  messages = { { role = "user", content = "must not leak" } },
})
local invoke = last_body()
eq(invoke.kind, "conversation.provider.invoke")
eq(invoke.request_id, "provider-1")
eq(invoke.conversation_id, "replayed")
eq(invoke.provider, "chatgpt")
eq(invoke.watermark, invoke_watermark, "invoke captures the folded manager watermark")
eq(invoke.model, "gpt-test")
eq(invoke.reasoning_effort, "high")
eq(invoke.system, nil, "system is canonical conversation content, not invoke payload")
eq(invoke.messages, nil, "full history never enters the manager/provider protocol")
eq(actor._internals.active_invocations()["provider-1"].conversation_id, "replayed")

receive({
  kind = "conversation.provider.invoke.request",
  request_id = "provider-1",
  conversation_id = "replayed",
  provider = "chatgpt",
})
eq(last_body().kind, "conversation.provider.invoke.rejected")
eq(last_body().code, "provider_request_id_conflict")
eq(actor._internals.active_invocations()["provider-1"].provider, "chatgpt",
  "duplicate invoke cannot replace active correlation")

receive({
  kind = "conversation.provider.event.reported",
  request_id = "provider-1",
  provider = "other",
  event = "text_delta",
  text = "wrong",
})
eq(last_body().kind, "conversation.provider.event.rejected")
eq(last_body().code, "provider_request_mismatch")
eq(actor._internals.active_invocations()["provider-1"].provider, "chatgpt",
  "mismatched events cannot settle correlation")

receive({
  kind = "conversation.provider.event.reported",
  request_id = "provider-1",
  provider = "chatgpt",
  event = "text_delta",
  text = "hello",
  provider_detail = { opaque = true },
  messages = { { role = "user", content = "must not relay" } },
})
local provider_event = last_body()
eq(provider_event.kind, "conversation.provider.event")
eq(provider_event.conversation_id, "replayed", "manager restores correlated conversation identity")
eq(provider_event.text, "hello")
eq(provider_event.provider_detail.opaque, true, "generic provider event detail is preserved")
eq(provider_event.messages, nil, "provider event reports cannot smuggle full history")
eq(actor._internals.active_invocations()["provider-1"].provider, "chatgpt")

receive({
  kind = "conversation.provider.event.reported",
  request_id = "provider-1",
  provider = "chatgpt",
  event = "completed",
  result = { text = "hello" },
})
eq(last_body().kind, "conversation.provider.event")
eq(actor._internals.active_invocations()["provider-1"], nil,
  "terminal provider event releases correlation")

receive({
  kind = "conversation.provider.invoke.request",
  request_id = "provider-2",
  conversation_id = "replayed",
  provider = "chatgpt",
})
eq(last_body().kind, "conversation.provider.invoke")
receive({
  kind = "conversation.provider.cancel.request",
  request_id = "provider-2",
  provider = "chatgpt",
})
eq(last_body().kind, "conversation.provider.cancel")
eq(last_body().conversation_id, "replayed")
eq(actor._internals.active_invocations()["provider-2"], nil,
  "cancel releases correlation")

receive({
  kind = "conversation.provider.invoke.request",
  request_id = "provider-3",
  conversation_id = "replayed",
  provider = "chatgpt",
})
receive({ kind = "sessions.session_end", session_id = "session-2" })
eq(actor._internals.active_invocations()["provider-3"], nil,
  "session end releases every active provider correlation")
