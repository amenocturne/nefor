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

local recorded = last_body()
eq(recorded.kind, "conversation.fact.recorded")
eq(recorded.event.sequence, 1, "manager assigns first sequence")
eq(recorded.event.event_id, "create-1", "recorded ack preserves idempotency key")
eq(recorded.session_id, "session-1")

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
eq(last_body().event.sequence, 2, "manager owns contiguous sequencing")

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
eq(snapshot.conversation.last_sequence, 2)

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

receive({ kind = "conversation.get.request", request_id = "replayed-query", conversation_id = "replayed" })
eq(#emitted, before_command, "replayed query emits no stale projection")
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
