local manager = require("libs.conversation-manager")
local domain = manager.domain

local function fail(message) error(message, 2) end
local function eq(actual, expected, message)
  if not domain.equal(actual, expected) then fail((message or "values differ") .. ": expected " .. tostring(expected) .. ", got " .. tostring(actual)) end
end
local function ok(value, e, message) if not value then fail((message or "unexpected error") .. ": " .. tostring(e and e.code)) end return value end
local function append(store, fact) return ok(store:append(fact)) end
local function rejects(store, fact, code)
  local value, e = store:append(fact)
  eq(value, nil, "rejection has no value"); eq(e.code, code, "precise error code")
  return e
end
local function fact(id, conversation_id, kind, extra)
  local out = { event_id = id, conversation_id = conversation_id, kind = kind }
  for key, value in pairs(extra or {}) do out[key] = value end
  return out
end
local function create(store, id, purpose)
  return append(store, fact(id .. ":created", id, "created", { provenance = {
    provider = "openai", model = "one", native_chat = "native:" .. id,
    session = "session", parent = "parent", run = "run:" .. id,
    actor = id, purpose = purpose,
  } }))
end

-- Every event variant participates in one canonical fold, preserving chunks as facts.
do
  local store = manager.new(); create(store, "lead", "lead")
  append(store, fact("p", "lead", "provenance_updated", { provenance = { provider = "anthropic", model = "two" } }))
  append(store, fact("m", "lead", "message_started", { message_id = "assistant:1", role = "assistant" }))
  local chunks = {
    { kind = "text", data = "hel" }, { kind = "reasoning", data = "why" },
    { kind = "structured", data = { answer = 4 } },
    { kind = "native", data = { type = "citation", index = 4 } }, { kind = "text", data = "lo" },
  }
  for index, chunk in ipairs(chunks) do append(store, fact("c" .. index, "lead", "content_chunk_appended", { message_id = "assistant:1", chunk = chunk })) end
  append(store, fact("x", "lead", "tool_exchange_started", { exchange_id = "exchange:1", message_id = "assistant:1", tool_name = "read" }))
  append(store, fact("xf1", "lead", "tool_call_fragment_appended", { exchange_id = "exchange:1", fragment = '{"pa' }))
  append(store, fact("xf2", "lead", "tool_call_fragment_appended", { exchange_id = "exchange:1", fragment = 'th":"x"}' }))
  append(store, fact("xc", "lead", "tool_call_completed", {
    exchange_id = "exchange:1", call = { id = "provider-call-7", name = "read", arguments = { path = "x" } },
  }))
  append(store, fact("xr", "lead", "tool_result_recorded", { exchange_id = "exchange:1", result = { text = "data" } }))
  append(store, fact("retry", "lead", "retry_started", { retry_id = "retry:1", message_id = "assistant:1", reason = "rate_limit", provenance = { model = "three" } }))
  append(store, fact("mc", "lead", "message_completed", { message_id = "assistant:1", completion = { finish_reason = "stop" } }))
  local done = append(store, fact("done", "lead", "conversation_completed", { detail = { usage = 12 } }))
  eq(done.status, "completed"); eq(done.provenance.provider, "anthropic"); eq(done.id, "lead", "provenance switch does not move identity")
  eq(#done.messages[1].chunks, 5, "universal content chunks remain individual")
  for index, chunk in ipairs(chunks) do eq(done.messages[1].chunks[index], chunk, "chunk order preserved") end
  eq(done.exchanges[1].tool_call_id, "provider-call-7"); eq(done.exchanges[1].arguments.path, "x")
  eq(done.exchanges[1].status, "result"); eq(#done.retries, 1)
  rejects(store, fact("late", "lead", "provenance_updated", { provenance = { model = "late" } }), "conversation_terminal")
end

-- Canonical recorded facts are the replay boundary: exact repeats are
-- idempotent, while sequence gaps and contradictory ids are rejected.
do
  local store = manager.new()
  local created = {
    event_id = "created", conversation_id = "recorded", kind = "created",
    sequence = 1, provenance = { session = "s" },
  }
  local value, e, duplicate = store:apply_recorded(created)
  ok(value, e); eq(duplicate, false)
  value, e, duplicate = store:apply_recorded(domain.copy(created))
  ok(value, e); eq(duplicate, true, "exact recorded replay is idempotent")

  local gap = {
    event_id = "message", conversation_id = "recorded", kind = "message_started",
    sequence = 3, message_id = "m", role = "user",
  }
  value, e = store:apply_recorded(gap)
  eq(value, nil); eq(e.code, "noncontiguous_sequence")
  eq(store:get("recorded").last_sequence, 1, "rejected recorded fact does not mutate")

  local conflict = domain.copy(created); conflict.provenance = { session = "other" }
  value, e = store:apply_recorded(conflict)
  eq(value, nil); eq(e.code, "event_id_conflict")
end

-- Tool errors are the other exactly-once terminal exchange outcome.
do
  local store = manager.new(); create(store, "error-chat", "agent")
  append(store, fact("m", "error-chat", "message_started", { message_id = "m", role = "assistant" }))
  append(store, fact("x", "error-chat", "tool_exchange_started", { exchange_id = "x", message_id = "m", tool_name = "shell.script" }))
  append(store, fact("xc", "error-chat", "tool_call_completed", {
    exchange_id = "x", call = { id = "external-x", name = "shell.script", arguments = {} },
  }))
  append(store, fact("xe", "error-chat", "tool_error_recorded", { exchange_id = "x", error = { code = "exit", message = "1" } }))
  rejects(store, fact("xr", "error-chat", "tool_result_recorded", { exchange_id = "x", result = {} }), "tool_exchange_terminal")
end

-- Invalid transitions are table-driven and failed facts never consume sequence.
local invalid_cases = {
  { "created required", function(s) end, fact("e", "c", "message_started", { message_id = "m", role = "user" }), "created_required" },
  { "created twice", function(s) create(s, "c", "lead") end, fact("e", "c", "created"), "created_more_than_once" },
  { "unknown kind", function(s) create(s, "c", "lead") end, fact("e", "c", "wat"), "unknown_event_kind" },
  { "bad role", function(s) create(s, "c", "lead") end, fact("e", "c", "message_started", { message_id = "m", role = "robot" }), "invalid_role" },
  { "missing message", function(s) create(s, "c", "lead") end, fact("e", "c", "content_chunk_appended", { message_id = "m", chunk = { kind = "text", data = "x" } }), "message_not_found" },
  { "bad chunk", function(s) create(s, "c", "lead"); append(s, fact("m", "c", "message_started", { message_id = "m", role = "user" })) end, fact("e", "c", "content_chunk_appended", { message_id = "m", chunk = { kind = "image" } }), "invalid_content_chunk" },
  { "tool call through generic chunk", function(s) create(s, "c", "lead"); append(s, fact("m", "c", "message_started", { message_id = "m", role = "assistant" })) end, fact("e", "c", "content_chunk_appended", { message_id = "m", chunk = { kind = "tool_call", data = "{}" } }), "invalid_content_chunk" },
  { "duplicate message id", function(s) create(s, "c", "lead"); append(s, fact("m", "c", "message_started", { message_id = "m", role = "user" })) end, fact("e", "c", "message_started", { message_id = "m", role = "user" }), "message_id_conflict" },
  { "duplicate exchange id", function(s) create(s, "c", "lead"); append(s, fact("m", "c", "message_started", { message_id = "m", role = "assistant" })); append(s, fact("x", "c", "tool_exchange_started", { exchange_id = "x", message_id = "m", tool_name = "t" })) end, fact("e", "c", "tool_exchange_started", { exchange_id = "x", message_id = "m", tool_name = "t" }), "exchange_id_conflict" },
  { "result before call complete", function(s) create(s, "c", "lead"); append(s, fact("m", "c", "message_started", { message_id = "m", role = "assistant" })); append(s, fact("x", "c", "tool_exchange_started", { exchange_id = "x", message_id = "m", tool_name = "t" })) end, fact("e", "c", "tool_result_recorded", { exchange_id = "x", result = {} }), "tool_call_incomplete" },
  { "message completion with fragmented call", function(s) create(s, "c", "lead"); append(s, fact("m", "c", "message_started", { message_id = "m", role = "assistant" })); append(s, fact("x", "c", "tool_exchange_started", { exchange_id = "x", message_id = "m", tool_name = "t" })) end, fact("e", "c", "message_completed", { message_id = "m" }), "tool_call_incomplete" },
  { "append after exact completion", function(s) create(s, "c", "lead"); append(s, fact("m", "c", "message_started", { message_id = "m", role = "user" })); append(s, fact("mc", "c", "message_completed", { message_id = "m" })) end, fact("e", "c", "content_chunk_appended", { message_id = "m", chunk = { kind = "text", data = "late" } }), "message_not_open" },
  { "complete with open message", function(s) create(s, "c", "lead"); append(s, fact("m", "c", "message_started", { message_id = "m", role = "user" })) end, fact("e", "c", "conversation_completed"), "open_message_at_completion" },
  { "dangling retry message", function(s) create(s, "c", "lead") end, fact("e", "c", "retry_started", { retry_id = "r", message_id = "missing" }), "message_not_found" },
}
for _, case in ipairs(invalid_cases) do
  local store = manager.new(); case[2](store); local before = store:get("c"); rejects(store, case[3], case[4]); eq(store:get("c"), before, case[1] .. " is atomic")
end

-- Creation validates provenance at the boundary, while retries may deliberately be conversation-level.
do
  local store = manager.new()
  rejects(store, fact("created", "bad-provenance", "created", { provenance = "not-a-table" }), "invalid_provenance")
  create(store, "conversation-retry", "lead")
  local retried = append(store, fact("retry", "conversation-retry", "retry_started", { retry_id = "r", reason = "transport" }))
  eq(retried.retries[1].message_id, nil); eq(#retried.retries, 1)
end

-- Idempotency is exact fact equality; event ids are globally unique.
do
  local store = manager.new(); local created = fact("same", "a", "created", { provenance = { purpose = "lead" } })
  local first = append(store, created); local second, e, duplicate = store:append(domain.copy(created))
  ok(second, e); eq(duplicate, true); eq(second, first); eq(second.last_sequence, 1)
  rejects(store, fact("same", "a", "created", { provenance = { purpose = "agent" } }), "event_id_conflict")
  rejects(store, fact("same", "b", "created"), "event_id_conflict")
  rejects(store, { event_id = "sequenced", conversation_id = "a", kind = "provenance_updated", sequence = 2, provenance = {} }, "sequence_manager_owned")
end

-- Interruption and failure retain partial open state; retry remains visible before terminality.
for _, terminal in ipairs({ "conversation_interrupted", "conversation_failed" }) do
  local store = manager.new(); create(store, terminal, "agent")
  append(store, fact("m", terminal, "message_started", { message_id = "partial", role = "assistant" }))
  append(store, fact("chunk", terminal, "content_chunk_appended", { message_id = "partial", chunk = { kind = "text", data = "partial" } }))
  append(store, fact("retry", terminal, "retry_started", { retry_id = "r", message_id = "partial", reason = "transient" }))
  local ended = append(store, fact("end", terminal, terminal, { detail = { reason = "stop" } }))
  eq(ended.messages[1].status, "open"); eq(ended.messages[1].chunks[1].data, "partial"); eq(#ended.retries, 1)
  rejects(store, fact("late", terminal, "message_completed", { message_id = "partial" }), "conversation_terminal")
end

-- Serialized replay reconstructs the complete modeled domain, including interleaved chats and partial state.
do
  local live = manager.new(); local events = {}
  local function record(value)
    local conversation, e, _, event = live:append(value); ok(conversation, e); events[#events + 1] = event
  end
  record(fact("serialized:created", "serialized", "created", { provenance = { purpose = "temporary" } }))
  record(fact("partial:created", "partial", "created", { provenance = { purpose = "agent" } }))
  record(fact("p", "serialized", "provenance_updated", { provenance = { provider = "anthropic", model = "two" } }))
  record(fact("m", "serialized", "message_started", { message_id = "m", role = "assistant" }))
  record(fact("native", "serialized", "content_chunk_appended", { message_id = "m", chunk = { kind = "native", data = { citation = 1 } } }))
  record(fact("x", "serialized", "tool_exchange_started", { exchange_id = "x", message_id = "m", tool_name = "read" }))
  record(fact("xf", "serialized", "tool_call_fragment_appended", { exchange_id = "x", fragment = '{"path":"x"}' }))
  record(fact("xc", "serialized", "tool_call_completed", { exchange_id = "x", call = { id = "external", name = "read", arguments = { path = "x" } } }))
  record(fact("xr", "serialized", "tool_result_recorded", { exchange_id = "x", result = { text = "data" } }))
  record(fact("retry", "serialized", "retry_started", { retry_id = "retry", message_id = "m", reason = "rate_limit" }))
  record(fact("mc", "serialized", "message_completed", { message_id = "m" }))
  record(fact("done", "serialized", "conversation_completed", { detail = { usage = 12 } }))
  record(fact("pm", "partial", "message_started", { message_id = "m", role = "assistant" }))
  record(fact("pc", "partial", "content_chunk_appended", { message_id = "m", chunk = { kind = "reasoning", data = "unfinished" } }))
  local encoded = nefor.json.encode(events); local decoded = nefor.json.decode(encoded)
  local replayed = manager.new(); ok(replayed:replay(decoded))
  eq(replayed:list(), live:list(), "serialized replay equals live state deeply")

  local existing = manager.new()
  local bad = domain.copy(decoded); bad[#bad].sequence = 99
  local value, e = existing:replay(bad)
  eq(value, nil); eq(e.code, "noncontiguous_sequence")
end

-- Heavily interleaved chats never share messages, exchanges, sequence, or provenance.
do
  local store = manager.new()
  local chats = {
    { "lead", "lead" }, { "agent-a", "agent" }, { "agent-b", "agent" },
    { "temp", "temporary" }, { "compact", "compaction" },
  }
  for _, chat in ipairs(chats) do create(store, chat[1], chat[2]) end
  for round = 1, 4 do
    for index = #chats, 1, -1 do
      local id = chats[index][1]; local mid = id .. ":m:" .. round
      append(store, fact(id .. ":start:" .. round, id, "message_started", { message_id = mid, role = round % 2 == 0 and "assistant" or "user" }))
    end
    for index, chat in ipairs(chats) do
      local id = chat[1]; local mid = id .. ":m:" .. round
      append(store, fact(id .. ":chunk:" .. round, id, "content_chunk_appended", { message_id = mid, chunk = { kind = "text", data = id .. ":" .. round } }))
      append(store, fact(id .. ":complete:" .. round, id, "message_completed", { message_id = mid }))
    end
  end
  for _, chat in ipairs(chats) do
    local conversation = store:get(chat[1]); eq(conversation.last_sequence, 13); eq(#conversation.messages, 4)
    eq(conversation.provenance.purpose, chat[2]); eq(conversation.messages[3].chunks[1].data, chat[1] .. ":3")
    for _, message in ipairs(conversation.messages) do if message.id:sub(1, #chat[1]) ~= chat[1] then fail("cross-chat message leakage") end end
  end
  local listed = store:list(); eq(#listed, 5); eq(listed[1].id, "agent-a", "neutral list is deterministic")
  listed[1].messages[1].chunks[1].data = "mutated"
  eq(store:get("agent-a").messages[1].chunks[1].data, "agent-a:1", "reads cannot mutate store")
end

-- Conversation-local entity indexes permit the same message, exchange, and retry ids in different chats.
do
  local store = manager.new(); create(store, "local-a", "agent"); create(store, "local-b", "agent")
  for _, id in ipairs({ "local-a", "local-b" }) do
    append(store, fact(id .. ":m", id, "message_started", { message_id = "shared-message", role = "assistant" }))
    append(store, fact(id .. ":x", id, "tool_exchange_started", { exchange_id = "shared-exchange", message_id = "shared-message", tool_name = "read" }))
    append(store, fact(id .. ":xf", id, "tool_call_fragment_appended", { exchange_id = "shared-exchange", fragment = id }))
    append(store, fact(id .. ":xc", id, "tool_call_completed", {
      exchange_id = "shared-exchange", call = { id = id .. ":call", name = "read", arguments = { source = id } },
    }))
    append(store, fact(id .. ":xr", id, "tool_result_recorded", { exchange_id = "shared-exchange", result = { source = id } }))
    append(store, fact(id .. ":retry", id, "retry_started", { retry_id = "shared-retry", message_id = "shared-message" }))
  end
  eq(store:get("local-a").exchanges[1].call_chunks[1], "local-a")
  eq(store:get("local-b").exchanges[1].call_chunks[1], "local-b")
  eq(store:get("local-a").retries[1].id, "shared-retry"); eq(store:get("local-b").retries[1].id, "shared-retry")
end


-- Fold work is one validated in-place application per canonical event. The
-- aggregate contains current indexes and state, never a second events array.
do
  local store = manager.new()
  create(store, "linear", "lead")
  local event_count = 1
  for index = 1, 2000 do
    local message_id = "m:" .. index
    append(store, fact("s:" .. index, "linear", "message_started", { message_id = message_id, role = "user" }))
    append(store, fact("c:" .. index, "linear", "content_chunk_appended", {
      message_id = message_id, chunk = { kind = "text", data = "x" },
    }))
    append(store, fact("d:" .. index, "linear", "message_completed", { message_id = message_id }))
    event_count = event_count + 3
  end
  local stats = store:stats()
  eq(stats.fold_count, event_count, "fold count scales exactly with accepted events")
  eq(stats.event_ids, event_count, "idempotency index stores one compact entry per event")
  eq(store:get("linear").events, nil, "aggregate does not duplicate canonical history")
end

-- Transcript disposition is an explicit, validated distinction. A message may
-- declare it up front, or narrow to diagnostic at its terminal fact; it can
-- never be promoted back into the transcript, and every disposition stays in
-- the model context projection.
do
  local projection = require("libs.conversation-manager.projection")
  local store = manager.new(); create(store, "visible", "lead")
  append(store, fact("t", "visible", "turn_started", { turn_id = "turn", run_id = "run" }))

  append(store, fact("m1", "visible", "message_started", {
    message_id = "declared", role = "user", visibility = "diagnostic", turn_id = "turn",
  }))
  append(store, fact("c1", "visible", "content_chunk_appended", {
    message_id = "declared", chunk = { kind = "text", data = "correction" },
  }))
  append(store, fact("d1", "visible", "message_completed", { message_id = "declared" }))
  eq(store:get("visible").messages[1].visibility, "diagnostic",
    "a declared diagnostic message keeps its disposition")

  append(store, fact("m2", "visible", "message_started", {
    message_id = "streamed", role = "assistant", turn_id = "turn",
  }))
  eq(store:get("visible").messages[2].visibility, "transcript",
    "a message with no declared disposition is ordinary transcript")
  append(store, fact("d2", "visible", "message_completed", {
    message_id = "streamed", visibility = "diagnostic",
  }))
  eq(store:get("visible").messages[2].visibility, "diagnostic",
    "a terminal fact may narrow a streamed message into diagnostic")

  append(store, fact("m3", "visible", "message_started", {
    message_id = "promoted", role = "assistant", visibility = "diagnostic", turn_id = "turn",
  }))
  rejects(store, fact("d3", "visible", "message_completed", {
    message_id = "promoted", visibility = "transcript",
  }), "invalid_visibility_promotion")
  rejects(store, fact("m4", "visible", "message_started", {
    message_id = "bogus", role = "assistant", visibility = "hidden", turn_id = "turn",
  }), "invalid_visibility")
  append(store, fact("d4", "visible", "message_completed", { message_id = "promoted" }))

  local context = projection.context(store:peek("visible"))
  eq(#context.messages, 3, "every disposition remains available as model context")
  eq(context.messages[1].visibility, "diagnostic",
    "the context projection reports each message's disposition")
end

print("conversation_manager_domain_test: all assertions passed")
