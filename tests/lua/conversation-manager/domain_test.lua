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
  append(store, fact("xr", "lead", "tool_result_recorded", {
    exchange_id = "exchange:1", result = { text = "data" },
    completion_delivery = "sync",
  }))
  append(store, fact("retry", "lead", "retry_started", { retry_id = "retry:1", message_id = "assistant:1", reason = "rate_limit", provenance = { model = "three" } }))
  append(store, fact("mc", "lead", "message_completed", { message_id = "assistant:1", completion = { finish_reason = "stop" } }))
  local done = append(store, fact("done", "lead", "conversation_completed", { detail = { usage = 12 } }))
  eq(done.status, "completed"); eq(done.provenance.provider, "anthropic"); eq(done.id, "lead", "provenance switch does not move identity")
  eq(#done.messages[1].chunks, 5, "universal content chunks remain individual")
  for index, chunk in ipairs(chunks) do eq(done.messages[1].chunks[index], chunk, "chunk order preserved") end
  eq(done.exchanges[1].tool_call_id, "provider-call-7"); eq(done.exchanges[1].arguments.path, "x")
  eq(done.exchanges[1].completion_delivery, "sync",
    "completion delivery is canonical exchange metadata")
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

-- Producer-owned presentation is durable metadata beside, not instead of,
-- the typed canonical content used to reconstruct model context.
do
  local projection = require("libs.conversation-manager.projection")
  local store = manager.new(); create(store, "display", "lead")
  local prompt = "resume me\nexactly"
  append(store, fact("m", "display", "message_started", {
    message_id = "user", role = "user", authored_prompt = prompt,
  }))
  local envelope = {
    mag_type = { version = 2, root = { kind = "named", name = "main.LeadInput" } },
    value = { prompt = prompt },
  }
  append(store, fact("chunk", "display", "content_chunk_appended", {
    message_id = "user", chunk = { kind = "structured", data = envelope },
  }))
  append(store, fact("done", "display", "message_completed", { message_id = "user" }))

  local conversation = store:peek("display")
  local resumed = projection.conversation(conversation).messages[1]
  eq(resumed.display_text, prompt, "resume projection retains exact authored presentation")
  eq(resumed.structured[1], envelope, "resume projection retains the typed envelope")
  eq(resumed.content, envelope, "public canonical projection retains the typed value")
  local provider = projection.context(conversation).messages[1]
  eq(provider.content, prompt, "provider context receives plain authored text")
  eq(provider.structured[1], envelope, "provider projection retains canonical type evidence")
  append(store, fact("rewind-display", "display", "rewind_committed", {
    target_history_id = resumed.history_id, expected_head = resumed.history_id,
  }))
  eq(store:peek("display").pending_edit.text, prompt,
    "rewind restores the exact multiline authored prompt")

  local generic = manager.new(); create(generic, "generic-structured", "lead")
  append(generic, fact("generic-start", "generic-structured", "message_started", {
    message_id = "generic-user", role = "user",
  }))
  append(generic, fact("generic-chunk", "generic-structured", "content_chunk_appended", {
    message_id = "generic-user", chunk = { kind = "structured", data = { prompt = "not authored" } },
  }))
  local completed = append(generic, fact("generic-done", "generic-structured", "message_completed", {
    message_id = "generic-user",
  }))
  rejects(generic, fact("generic-rewind", "generic-structured", "rewind_committed", {
    target_history_id = completed.messages[1].history_id,
    expected_head = completed.head,
  }), "invalid_rewind_target")
end

-- Provider context needs a valid assistant message after an interruption even
-- when only private reasoning streamed. Public projections preserve the exact
-- text and reasoning, while text and tool-bearing interruptions pass through.
do
  local projection = require("libs.conversation-manager.projection")
  local store = manager.new(); create(store, "interrupt-context", "lead")

  append(store, fact("empty-start", "interrupt-context", "message_started", {
    message_id = "empty", role = "assistant",
  }))
  append(store, fact("empty-reasoning", "interrupt-context", "content_chunk_appended", {
    message_id = "empty", chunk = { kind = "reasoning", data = "private reasoning" },
  }))
  append(store, fact("empty-space", "interrupt-context", "content_chunk_appended", {
    message_id = "empty", chunk = { kind = "text", data = "  \n" },
  }))
  append(store, fact("empty-stop", "interrupt-context", "message_interrupted", {
    message_id = "empty",
  }))

  append(store, fact("text-start", "interrupt-context", "message_started", {
    message_id = "text", role = "assistant",
  }))
  append(store, fact("text-chunk", "interrupt-context", "content_chunk_appended", {
    message_id = "text", chunk = { kind = "text", data = "partial answer" },
  }))
  append(store, fact("text-stop", "interrupt-context", "message_interrupted", {
    message_id = "text",
  }))

  append(store, fact("tool-start", "interrupt-context", "message_started", {
    message_id = "tool", role = "assistant",
  }))
  append(store, fact("tool-exchange", "interrupt-context", "tool_exchange_started", {
    exchange_id = "tool-exchange", message_id = "tool", tool_name = "read_file",
  }))
  append(store, fact("tool-call", "interrupt-context", "tool_call_completed", {
    exchange_id = "tool-exchange",
    call = { id = "call-1", name = "read_file", arguments = { path = "README.md" } },
  }))
  append(store, fact("tool-stop", "interrupt-context", "message_interrupted", {
    message_id = "tool",
  }))

  local conversation = store:peek("interrupt-context")
  local public = projection.conversation(conversation).messages
  eq(public[1].content, "  \n", "public interruption preserves whitespace text")
  eq(public[1].reasoning, "private reasoning", "public interruption preserves reasoning")
  local context = projection.context(conversation).messages
  eq(context[1].content, "[interrupted by user]",
    "provider context replaces an empty interrupted assistant turn")
  eq(context[1].reasoning, "private reasoning", "provider context preserves interrupted reasoning")
  eq(context[2].content, "partial answer", "provider context preserves interrupted text")
  eq(context[3].content, "", "provider context preserves tool-only interrupted content")
  eq(#context[3].tool_calls, 1, "provider context preserves interrupted tool calls")
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

-- A discarded provisional provider attempt may reuse the provider's stable
-- tool-call id in its replacement attempt; ordinary transcript conflicts remain rejected.
do
  local store = manager.new(); create(store, "replayed-native", "agent")
  append(store, fact("m1", "replayed-native", "message_started", { message_id = "m1", role = "assistant" }))
  append(store, fact("x1", "replayed-native", "tool_exchange_started", {
    exchange_id = "x1", message_id = "m1", tool_name = "web_search",
  }))
  append(store, fact("x1c", "replayed-native", "tool_call_completed", {
    exchange_id = "x1", call = { id = "ws-stable", name = "web_search", arguments = { query = "old" } },
  }))
  append(store, fact("x1e", "replayed-native", "tool_error_recorded", {
    exchange_id = "x1", error = "attempt discarded",
  }))
  append(store, fact("m1i", "replayed-native", "message_interrupted", {
    message_id = "m1", visibility = "discarded",
  }))
  append(store, fact("m2", "replayed-native", "message_started", { message_id = "m2", role = "assistant" }))
  append(store, fact("x2", "replayed-native", "tool_exchange_started", {
    exchange_id = "x2", message_id = "m2", tool_name = "web_search",
  }))
  append(store, fact("x2c", "replayed-native", "tool_call_completed", {
    exchange_id = "x2", call = { id = "ws-stable", name = "web_search", arguments = { query = "new" } },
  }))
  local current = append(store, fact("x2r", "replayed-native", "tool_result_recorded", {
    exchange_id = "x2", result = { status = "completed" },
  }))
  eq(current.exchange_by_tool_call_id["ws-stable"].id, "x2",
    "stable id resolves to the surviving replacement exchange")
end

-- Completion delivery is a closed optional value: old records may omit it,
-- while new delayed-tool settlements must use one of the canonical variants.
do
  local store = manager.new(); create(store, "delivery", "agent")
  append(store, fact("m", "delivery", "message_started", { message_id = "m", role = "assistant" }))
  append(store, fact("x", "delivery", "tool_exchange_started", {
    exchange_id = "x", message_id = "m", tool_name = "mag",
  }))
  append(store, fact("xc", "delivery", "tool_call_completed", {
    exchange_id = "x", call = { id = "external-x", name = "mag", arguments = {} },
  }))
  rejects(store, fact("xr", "delivery", "tool_result_recorded", {
    exchange_id = "x", result = {}, completion_delivery = "legacy",
  }), "invalid_completion_delivery")
end

-- Invalid transitions are table-driven and failed facts never consume sequence.
local invalid_cases = {
  { "created required", function(s) end, fact("e", "c", "message_started", { message_id = "m", role = "user" }), "created_required" },
  { "created twice", function(s) create(s, "c", "lead") end, fact("e", "c", "created"), "created_more_than_once" },
  { "unknown kind", function(s) create(s, "c", "lead") end, fact("e", "c", "wat"), "unknown_event_kind" },
  { "bad role", function(s) create(s, "c", "lead") end, fact("e", "c", "message_started", { message_id = "m", role = "robot" }), "invalid_role" },
  { "bad authored prompt", function(s) create(s, "c", "lead") end, fact("e", "c", "message_started", { message_id = "m", role = "user", authored_prompt = { "not", "text" } }), "invalid_authored_prompt" },
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
  record(fact("mc", "serialized", "message_completed", {
    message_id = "m",
    model = "gpt-5.6-sol",
    duration_ms = 42,
    usage = {
      prompt_tokens = 544007, input_tokens = 544007,
      completion_tokens = 4, output_tokens = 4, total_tokens = 544011,
      context_input_tokens = 7,
      cache_read_input_tokens = 0, reasoning_tokens = 2,
      input_tokens_include_cache_read = true,
      provider = "chatgpt", model = "gpt-5.6-sol",
      billing_components_complete = true, aggregate_totals_exact = true,
      billing_components = {
        { usage_available = true, input_tokens = 271999, output_tokens = 1,
          service_tier = "standard" },
        { usage_available = true, input_tokens = 272001, output_tokens = 2,
          cache_write_input_tokens = 0, service_tier = "priority" },
        { usage_available = true, input_tokens = 7, output_tokens = 1,
          cache_write_input_tokens = 9, service_tier = "priority" },
      },
    },
    provider_context = {
      provider = "chatgpt",
      format = "chatgpt.responses.output_items.v1",
      model = "gpt-5.6-sol",
      artifact = { items = { { type = "reasoning", encrypted_content = "sealed" } } },
    },
  }))
  record(fact("done", "serialized", "conversation_completed", { detail = { usage = 12 } }))
  record(fact("pm", "partial", "message_started", { message_id = "m", role = "assistant" }))
  record(fact("pc", "partial", "content_chunk_appended", { message_id = "m", chunk = { kind = "reasoning", data = "unfinished" } }))
  local encoded = nefor.json.encode(events); local decoded = nefor.json.decode(encoded)
  local replayed = manager.new(); ok(replayed:replay(decoded))
  eq(replayed:list(), live:list(), "serialized replay equals live state deeply")
  local replayed_message = replayed:get("serialized").messages[1]
  eq(replayed_message.provider_context.artifact.items[1].encrypted_content,
    "sealed", "serialized replay reconstructs opaque provider context")
  eq(replayed_message.terminal.model, "gpt-5.6-sol")
  eq(replayed_message.terminal.duration_ms, 42)
  eq(replayed_message.terminal.usage.cache_read_input_tokens, 0,
    "explicit zero cache evidence survives JSONL-shaped serialization and replay")
  eq(replayed_message.terminal.usage.reasoning_tokens, 2)
  eq(replayed_message.terminal.usage.input_tokens_include_cache_read, true)
  eq(replayed_message.terminal.usage.billing_components[1].service_tier, "standard")
  eq(replayed_message.terminal.usage.billing_components[1].cache_write_input_tokens, nil,
    "absent per-request cache writes survive replay")
  eq(replayed_message.terminal.usage.billing_components[2].input_tokens, 272001)
  eq(replayed_message.terminal.usage.billing_components[2].service_tier, "priority")
  eq(replayed_message.terminal.usage.billing_components[2].cache_write_input_tokens, 0,
    "explicit zero per-request cache writes survive replay")
  eq(replayed_message.terminal.usage.billing_components[3].cache_write_input_tokens, 9,
    "nonzero per-request cache writes survive replay")
  eq(replayed_message.terminal.usage.service_tier, nil,
    "mixed tiers do not acquire a synthetic aggregate tier")

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
-- never be promoted back into the transcript. Diagnostic messages remain model
-- context; discarded transport attempts remain audit-only.
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

  append(store, fact("m5", "visible", "message_started", {
    message_id = "abandoned", role = "assistant", turn_id = "turn",
  }))
  append(store, fact("c5", "visible", "content_chunk_appended", {
    message_id = "abandoned", chunk = { kind = "text", data = "partial" },
  }))
  append(store, fact("d5", "visible", "message_interrupted", {
    message_id = "abandoned", visibility = "discarded",
  }))

  local context = projection.context(store:peek("visible"))
  eq(#context.messages, 3, "discarded attempts stay out of model context")
  eq(context.messages[1].visibility, "diagnostic",
    "the context projection reports each message's disposition")
  local public = projection.conversation(store:peek("visible"))
  eq(#public.messages, 4, "discarded attempts remain available for audit")
  eq(public.messages[4].visibility, "discarded", "audit projection names the disposition")
end

-- Provider-native continuation state is durable model context, not public
-- conversation data. Compatible providers receive the opaque artifact after
-- replay while surfaces and ordinary conversation readers never do.
do
  local projection = require("libs.conversation-manager.projection")
  local store = manager.new(); create(store, "native-context", "lead")
  append(store, fact("start", "native-context", "message_started", {
    message_id = "assistant", role = "assistant",
  }))
  append(store, fact("tool-start", "native-context", "tool_exchange_started", {
    exchange_id = "native-exchange", message_id = "assistant",
    tool_call_id = "web-1", tool_name = "web_search",
  }))
  append(store, fact("tool-args", "native-context", "tool_call_fragment_appended", {
    exchange_id = "native-exchange",
    fragment = { arguments = { action = "search", query = "sanitized query" } },
  }))
  append(store, fact("tool-call", "native-context", "tool_call_completed", {
    exchange_id = "native-exchange",
    call = {
      tool_call_id = "web-1", name = "web_search",
      arguments = { action = "search", query = "sanitized query" },
    },
  }))
  append(store, fact("tool-result", "native-context", "tool_result_recorded", {
    exchange_id = "native-exchange", result = { status = "completed" },
  }))
  local provider_context = {
    provider = "chatgpt",
    format = "chatgpt.responses.output_items.v1",
    model = "gpt-5.6-sol",
    artifact = { items = { { type = "reasoning", encrypted_content = "sealed" } } },
  }
  append(store, fact("complete", "native-context", "message_completed", {
    message_id = "assistant", provider_context = provider_context,
  }))

  local private_context = projection.context(store:peek("native-context"))
  eq(private_context.messages[1].provider_context.artifact.items[1].encrypted_content,
    "sealed", "provider context projection preserves the opaque item")
  local public = projection.conversation(store:peek("native-context"))
  eq(public.messages[1].provider_context, nil,
    "public conversation projection does not expose provider context")
  eq(public.exchanges[1].tool_call_id, "web-1",
    "public conversation retains the sanitized native exchange identity")
  eq(public.exchanges[1].arguments.query, "sanitized query",
    "public conversation retains only the canonical tool arguments")
  eq(public.exchanges[1].result.status, "completed",
    "public conversation retains the sanitized native receipt")
  eq(nefor.json.encode(public):find("sealed", 1, true), nil,
    "public conversation contains no opaque provider artifact bytes")

  local invalid = manager.new(); create(invalid, "invalid-native-context", "lead")
  append(invalid, fact("start-invalid", "invalid-native-context", "message_started", {
    message_id = "assistant", role = "assistant",
  }))
  rejects(invalid, fact("complete-invalid", "invalid-native-context", "message_completed", {
    message_id = "assistant",
    provider_context = { provider = "chatgpt", artifact = {} },
  }), "invalid_provider_context")
end

-- Materialized history paths make rewind an append-only head movement. New
-- messages allocate collision-free siblings while inactive branches remain
-- immutable and replay reconstructs the same active head.
do
  local projection = require("libs.conversation-manager.projection")
  local history_path = require("libs.conversation-manager.history_path")
  local store = manager.new()
  local recorded = {}
  local function recorded_append(value)
    local conversation, e, _, event = store:append(value); ok(conversation, e)
    recorded[#recorded + 1] = event
    return conversation, event
  end
  recorded_append(fact("branch:created", "branch", "created", { provenance = {
    provider = "openai", model = "one", native_chat = "native:branch",
    session = "session", parent = "parent", run = "run:branch",
    actor = "branch", purpose = "lead",
  } }))
  local function message(id, role, text)
    local conversation, event = recorded_append(fact(id .. ":start", "branch", "message_started", {
      message_id = id, role = role,
    }))
    recorded_append(fact(id .. ":chunk", "branch", "content_chunk_appended", {
      message_id = id, chunk = { kind = "text", data = text },
    }))
    recorded_append(fact(id .. ":done", "branch", "message_completed", { message_id = id }))
    return event.history_id, conversation
  end

  local system_id = message("system", "system", "rules")
  local first_id = message("m1", "user", "first")
  message("a1", "assistant", "answer one")
  local second_id = message("m2", "user", "second\nline")
  message("a2", "assistant", "answer two")
  local before_rewind = store:get("branch")
  eq(history_path.parent(second_id), { 1, 1, 1 }, "parent drops the final component")
  eq(history_path.prefixes(second_id), { { 1 }, { 1, 1 }, { 1, 1, 1 }, { 1, 1, 1, 1 } },
    "prefixes represent complete ancestry")

  recorded_append(fact("rewind", "branch", "rewind_committed", {
    target_history_id = second_id,
    expected_head = before_rewind.head,
  }))
  local rewound = store:get("branch")
  eq(rewound.head, { 1, 1, 1 }, "head moves to the selected prompt parent")
  eq(rewound.pending_edit.text, "second\nline", "exact multiline text is retained")
  rejects(store, fact("stale", "branch", "rewind_committed", {
    target_history_id = first_id, expected_head = before_rewind.head,
  }), "stale_rewind_target")

  local sibling_id = message("m3", "user", "replacement")
  eq(sibling_id, { 1, 1, 1, 2 }, "new submission allocates a sibling component")
  message("a3", "assistant", "replacement answer")
  local public = projection.conversation(store:peek("branch"))
  eq(#public.messages, 5, "inactive sibling messages stay out of active projection")
  eq(public.messages[4].content, "replacement")
  eq(public.messages[5].content, "replacement answer")
  eq(store:get("branch").messages[4].chunks[1].data, "second\nline",
    "inactive branch remains immutable")

  local replayed = manager.new(); ok(replayed:replay(nefor.json.decode(nefor.json.encode(recorded))))
  eq(replayed:get("branch"), store:get("branch"), "append-only replay is equivalent")
  eq(system_id, { 1 }); eq(first_id, { 1, 1 })
end

-- Completed compactions are checkpoints on one ancestry path only. Rewinding
-- before a checkpoint bypasses it, and a later sibling checkpoint coexists.
do
  local projection = require("libs.conversation-manager.projection")
  local store = manager.new(); create(store, "compact-branch", "lead")
  local function message(id, role, text)
    append(store, fact(id .. ":s", "compact-branch", "message_started", {
      message_id = id, role = role,
    }))
    append(store, fact(id .. ":c", "compact-branch", "content_chunk_appended", {
      message_id = id, chunk = { kind = "text", data = text },
    }))
    append(store, fact(id .. ":d", "compact-branch", "message_completed", { message_id = id }))
    return store:get("compact-branch").messages[#store:get("compact-branch").messages].history_id
  end
  message("system", "system", "rules")
  local old_prompt = message("old-user", "user", "old")
  message("old-answer", "assistant", "old answer")
  append(store, fact("old-compact-request", "compact-branch", "context_compaction_requested", {
    request_id = "old-compact", history_cutoff = 3, provider = "p",
  }))
  append(store, fact("old-compact-done", "compact-branch", "context_compaction_completed", {
    request_id = "old-compact", checkpoint = { opaque = "old" },
  }))
  local old_head = store:get("compact-branch").head
  append(store, fact("rewind-old", "compact-branch", "rewind_committed", {
    target_history_id = old_prompt, expected_head = old_head,
  }))
  message("new-user", "user", "new")
  message("new-answer", "assistant", "new answer")
  local bypassed = projection.context(store:peek("compact-branch"))
  eq(bypassed.compaction, nil, "branch-incompatible checkpoint is bypassed")
  eq(#bypassed.messages, 3, "inactive sibling history never reaches provider context")

  append(store, fact("new-compact-request", "compact-branch", "context_compaction_requested", {
    request_id = "new-compact", history_cutoff = 3, provider = "p",
  }))
  append(store, fact("new-compact-done", "compact-branch", "context_compaction_completed", {
    request_id = "new-compact", checkpoint = { opaque = "new" },
  }))
  local current = projection.context(store:peek("compact-branch"))
  eq(current.compaction.checkpoint.opaque, "new")
  eq(#store:get("compact-branch").compactions, 2, "branch-local checkpoints coexist")
end

print("conversation_manager_domain_test: all assertions passed")
