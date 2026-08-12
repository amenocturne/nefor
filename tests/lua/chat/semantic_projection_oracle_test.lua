package.preload["nefor-tui"] = function()
  local nil_sentinel = {}
  local function shallow_merge(left, right)
    local out = {}
    for key, value in pairs(left or {}) do out[key] = value end
    for key, value in pairs(right or {}) do
      if value == nil_sentinel then out[key] = nil else out[key] = value end
    end
    return out
  end
  return { util = { shallow_merge = shallow_merge, NIL = nil_sentinel,
    bordered_box = function() end } }
end

tui = { now_ms = function() return 0 end }

local manager = require("libs.conversation-manager")
local canonical_projection = require("libs.conversation-manager.projection")
local surface_projection = require("libs.chat.conversation_projection")
local semantic = require("libs.chat.semantic_projection")
local queued_input = require("libs.chat.queued_input")
local transcript = require("libs.chat.transcript")
local Entry = require("libs.chat.entry")

local function fail(message) error(message, 2) end
local function eq(actual, expected, message)
  if not manager.domain.equal(actual, expected) then
    fail((message or "values differ") .. "\nactual=" .. nefor.json.encode(actual)
      .. "\nexpected=" .. nefor.json.encode(expected))
  end
end

local function expected_rows(projection)
  local rows = {}
  local exchange_by_message = {}
  for _, exchange in ipairs(projection.exchanges or {}) do
    local list = exchange_by_message[exchange.message_id] or {}
    list[#list + 1] = exchange
    exchange_by_message[exchange.message_id] = list
  end
  for _, message in ipairs(projection.messages or {}) do
    if message.role == "user" or message.role == "assistant" then
      rows[#rows + 1] = {
        kind = "message", role = message.role, message_id = message.id,
        turn_id = message.turn_id, text = message.text or "",
        reasoning = message.role == "assistant" and (message.reasoning ~= "" and message.reasoning or nil) or nil,
      }
      if message.role == "assistant" then
        for _, exchange in ipairs(exchange_by_message[message.id] or {}) do
          rows[#rows + 1] = {
            kind = "exchange", exchange_id = exchange.id, turn_id = message.turn_id,
            name = exchange.name, arguments = exchange.arguments,
            result = exchange.result, error = exchange.status == "error",
          }
        end
      end
    end
  end
  return rows
end

local function apply_actions(state, actions)
  for _, item in ipairs(actions) do
    if item.kind == "snapshot_reset" then
      state.entries = {}
      state.in_flight = nil
    elseif item.kind == "turn_started" then
      state.active_turn_id = item.turn_id
    elseif item.kind == "message" then
      local reconciled, matched = queued_input.reconcile_echo(
        state, item.submission_ids, item.message_id, item.turn_id)
      if matched then state = reconciled else
        state.entries[#state.entries + 1] = Entry.bind_canonical(
          Entry.user(item.text), item.message_id, item.turn_id)
      end
    elseif item.kind == "text_delta" then
      state = transcript.append_assistant_delta(
        state, item.text, item.message_id, item.turn_id)
    elseif item.kind == "reasoning_delta" then
      state = transcript.append_reasoning_delta(
        state, item.text, item.message_id, item.turn_id)
    elseif item.kind == "assistant_completed" then
      state = transcript.finalize_assistant(
        state, item.text, nil, nil, item.message_id, item.turn_id)
    elseif item.kind == "tool_started" then
      state.entries[#state.entries + 1] = Entry.tool_call(
        item.exchange_id, item.name, "", nil, nil, item.arguments, item.turn_id)
    elseif item.kind == "tool_completed" then
      state = transcript.attach_tool_end(state, item.exchange_id, item.output, item.error)
    end
  end
  return state
end

local function snapshot_body(conversation_id, projection)
  return { kind = "conversation.snapshot", conversation_id = conversation_id,
    found = true, projection = projection }
end

local function actions_for(events, projection)
  local state = surface_projection.new()
  state = select(1, surface_projection.reduce(state, {
    kind = "conversation.active.changed", conversation_id = projection.id,
  }))
  local actions = {}
  for _, event in ipairs(events) do
    local produced
    state, produced = surface_projection.reduce(state, {
      kind = "conversation.projection.delta", conversation_id = projection.id,
      change = event.change,
    })
    for _, item in ipairs(produced) do actions[#actions + 1] = item end
  end
  return actions
end

local function fixture()
  local store = manager.new()
  local events = {}
  local sequence = 0
  local current
  local function append(kind, fields)
    sequence = sequence + 1
    local fact = { event_id = "e" .. sequence, conversation_id = "790e3e94-b8c4-4c2d-92ad-a061f3a0c237", kind = kind }
    for key, value in pairs(fields or {}) do fact[key] = value end
    local conversation, err, _, recorded = store:append(fact)
    if not conversation then fail(err.code) end
    current = store:peek(conversation.id)
    local internal = current
    events[#events + 1] = { change = canonical_projection.change(internal, recorded) }
  end
  append("created")
  append("turn_started", { turn_id = "turn-1", run_id = "run-1" })
  append("message_started", { message_id = "user-1", turn_id = "turn-1", role = "user", submission_ids = {} })
  append("content_chunk_appended", { message_id = "user-1", turn_id = "turn-1", chunk = { kind = "text", data = "same" } })
  append("message_completed", { message_id = "user-1", turn_id = "turn-1" })
  append("message_started", { message_id = "assistant-1", turn_id = "turn-1", role = "assistant" })
  append("content_chunk_appended", { message_id = "assistant-1", turn_id = "turn-1", chunk = { kind = "reasoning", data = "thinking" } })
  append("content_chunk_appended", { message_id = "assistant-1", turn_id = "turn-1", chunk = { kind = "text", data = "working" } })
  append("tool_exchange_started", { exchange_id = "exchange-1", message_id = "assistant-1", turn_id = "turn-1", tool_name = "mag" })
  append("tool_call_completed", { exchange_id = "exchange-1", turn_id = "turn-1", call = { id = "provider-1", name = "mag", arguments = { action = "execute" } } })
  append("tool_result_recorded", { exchange_id = "exchange-1", turn_id = "turn-1", result = "done" })
  append("tool_exchange_started", { exchange_id = "exchange-2", message_id = "assistant-1", turn_id = "turn-1", tool_name = "mag" })
  append("tool_call_completed", { exchange_id = "exchange-2", turn_id = "turn-1", call = { id = "provider-2", name = "mag", arguments = { action = "execute" } } })
  append("tool_result_recorded", { exchange_id = "exchange-2", turn_id = "turn-1", result = "done" })
  append("message_completed", { message_id = "assistant-1", turn_id = "turn-1" })
  append("turn_completed", { turn_id = "turn-1", run_id = "run-1" })
  append("turn_started", { turn_id = "turn-2", run_id = "run-2" })
  append("message_started", { message_id = "user-2", turn_id = "turn-2", role = "user", submission_ids = { "submit-same" } })
  append("content_chunk_appended", { message_id = "user-2", turn_id = "turn-2", chunk = { kind = "text", data = "same" } })
  append("message_completed", { message_id = "user-2", turn_id = "turn-2" })
  append("message_started", { message_id = "assistant-2", turn_id = "turn-2", role = "assistant" })
  append("content_chunk_appended", { message_id = "assistant-2", turn_id = "turn-2", chunk = { kind = "text", data = "next" } })
  append("message_completed", { message_id = "assistant-2", turn_id = "turn-2" })
  append("turn_completed", { turn_id = "turn-2", run_id = "run-2" })
  local projection = canonical_projection.conversation(
    store:peek("790e3e94-b8c4-4c2d-92ad-a061f3a0c237"))
  return events, projection
end

local events, projection = fixture()
local expected = expected_rows(projection)

local live = { entries = {}, input_value = "" }
live = queued_input.submit(live, "same", false, "submit-same")
live = apply_actions(live, actions_for(events, projection))
eq(semantic.normalize(live).canonical, expected,
  "live deltas preserve each canonical identity once in manager order")

local replay_state = surface_projection.new()
replay_state = select(1, surface_projection.reduce(replay_state, {
  kind = "conversation.active.changed", conversation_id = projection.id,
}))
local snapshot_actions
replay_state, snapshot_actions = surface_projection.reduce(replay_state,
  snapshot_body(projection.id, projection))
local replay = apply_actions({ entries = {} }, snapshot_actions)
eq(semantic.normalize(replay).canonical, expected,
  "snapshot/replay converges with live semantic transcript")

local decorated = { entries = {} }
for _, entry in ipairs(live.entries) do decorated.entries[#decorated.entries + 1] = entry end
for _, entry in ipairs({
  Entry.graph_result("mag-run", "success", {}, "ok"),
  Entry.compaction({ request_id = "compact", status = "complete" }),
  { kind = "toast", text = "notice" }, { kind = "popup" }, { kind = "sidebar" },
}) do table.insert(decorated.entries, 2, entry) end
eq(semantic.normalize(decorated).canonical, expected,
  "irrelevant MAG/status/popup/sidebar rows cannot alter canonical subsequence")

local classified = semantic.normalize(decorated)
eq(#classified.ephemeral, 5, "explicit graph/compaction/toast/popup/sidebar rows are classified ephemeral")
local unknown = semantic.normalize({ entries = { { kind = "future-semantic-row" } } })
eq(#unknown.ephemeral, 0, "unknown semantic rows are not silently blessed")
eq(#unknown.unclassified, 1, "unknown semantic rows remain visibly unclassified")

for _, entry in ipairs(decorated.entries) do
  entry.expanded = not entry.expanded
  entry.raw = not entry.raw
end
eq(semantic.normalize(decorated).canonical, expected,
  "collapsed expanded and raw presentation normalize identically")

local exchanges = {}
for _, row in ipairs(expected) do
  if row.kind == "exchange" then exchanges[row.exchange_id] = (exchanges[row.exchange_id] or 0) + 1 end
end
eq(exchanges["exchange-1"], 1, "tool result settles the same exchange row")
eq(exchanges["exchange-2"], 1,
  "identical tool calls retain distinct canonical exchange identities")
eq(expected[2].reasoning, "thinking", "reasoning remains nested assistant metadata")
local user_ids = {}
for _, row in ipairs(expected) do
  if row.role == "user" then user_ids[#user_ids + 1] = row.message_id end
end
eq(user_ids[1] == user_ids[2], false,
  "repeated identical user text retains distinct canonical identity")

local delayed = queued_input.submit({ entries = {} }, "delayed", false, "submit-delayed")
eq(#semantic.normalize(delayed).local_rows, 1, "delayed acknowledgement stays a local overlay")
local unrelated, claimed = queued_input.reconcile_echo(
  delayed, { "other-submission" }, "unrelated-same-text", "turn-earlier")
eq(claimed, false, "an unrelated canonical echo cannot claim equal optimistic text")
eq(unrelated.entries[1].message_id, nil)
delayed = select(1, queued_input.reconcile_echo(
  delayed, { "submit-delayed" }, "user-delayed", "turn-delayed"))
eq(#semantic.normalize(delayed).local_rows, 0)
eq(semantic.normalize(delayed).canonical[1].message_id, "user-delayed",
  "causal acknowledgement transfers local overlay to canonical identity")

local queued = queued_input.submit({ entries = {} }, "one", true, "submit-one")
queued = queued_input.submit(queued, "two", true, "submit-two")
local queued_local_id = queued.entries[1].local_id
queued = select(1, queued_input.accept_steered(queued))
eq(queued.entries[1].local_id, queued_local_id, "queue promotion preserves stable local identity")
queued = select(1, queued_input.reconcile_echo(
  queued, { "submit-one", "submit-two" }, "user-queued", "turn-queued"))
eq(queued.entries[1].message_id, "user-queued", "coalesced queue binds through all causal submissions")

local canceled = queued_input.submit(
  { entries = {}, input_value = "draft" }, "restore", true, "submit-restore")
canceled = select(1, queued_input.restore(canceled))
eq(#semantic.normalize(canceled).canonical, 0, "cancellation does not manufacture canonical rows")
eq(canceled.input_value, "restore draft", "cancellation restores optimistic input")

local compact_store = manager.new()
local compact_sequence = 0
local function compact_append(kind, fields)
  compact_sequence = compact_sequence + 1
  local fact = { event_id = "compact-" .. compact_sequence,
    conversation_id = "compact-conversation", kind = kind }
  for key, value in pairs(fields or {}) do fact[key] = value end
  local value, err = compact_store:append(fact)
  if not value then fail(err.code) end
end
compact_append("created")
compact_append("message_started", { message_id = "compact-user", role = "user" })
compact_append("content_chunk_appended", { message_id = "compact-user",
  chunk = { kind = "text", data = "preserved" } })
compact_append("message_completed", { message_id = "compact-user" })
local before_compaction = expected_rows(canonical_projection.conversation(
  compact_store:peek("compact-conversation")))
compact_append("context_compaction_requested", { request_id = "compact-request",
  provider = "test-provider", history_cutoff = 1 })
compact_append("context_compaction_completed", { request_id = "compact-request",
  checkpoint = { opaque = "artifact" } })
local after_compaction = canonical_projection.conversation(
  compact_store:peek("compact-conversation"))
eq(expected_rows(after_compaction), before_compaction,
  "real compaction facts preserve transcript projection")
eq(after_compaction.compactions[1].status, "completed")

local function terminal_case(result_before_message)
  local store = manager.new()
  local n = 0
  local function add(kind, fields)
    n = n + 1
    local fact = { event_id = "terminal-" .. n, conversation_id = "terminal", kind = kind }
    for key, value in pairs(fields or {}) do fact[key] = value end
    return store:append(fact)
  end
  assert(add("created")); assert(add("turn_started", { turn_id = "turn", run_id = "run" }))
  assert(add("message_started", { message_id = "assistant", turn_id = "turn", role = "assistant" }))
  assert(add("tool_exchange_started", { exchange_id = "exchange", message_id = "assistant",
    turn_id = "turn", tool_name = "read" }))
  assert(add("tool_call_completed", { exchange_id = "exchange", turn_id = "turn",
    call = { id = "provider-call", name = "read", arguments = {} } }))
  if result_before_message then assert(add("tool_result_recorded", {
    exchange_id = "exchange", turn_id = "turn", result = "done" })) end
  assert(add("message_completed", { message_id = "assistant", turn_id = "turn" }))
  if not result_before_message then assert(add("tool_result_recorded", {
    exchange_id = "exchange", turn_id = "turn", result = "done" })) end
  assert(add("turn_completed", { turn_id = "turn", run_id = "run" }))
  return expected_rows(canonical_projection.conversation(store:peek("terminal")))
end
eq(terminal_case(true), terminal_case(false),
  "manager-accepted tool result placement around message completion converges")

local rejected_store = manager.new()
local function rejected(fact) return rejected_store:append(fact) end
assert(rejected({ event_id = "r1", conversation_id = "rejected", kind = "created" }))
assert(rejected({ event_id = "r2", conversation_id = "rejected", kind = "turn_started",
  turn_id = "turn", run_id = "run" }))
assert(rejected({ event_id = "r3", conversation_id = "rejected", kind = "message_started",
  message_id = "assistant", turn_id = "turn", role = "assistant" }))
local value, boundary = rejected({ event_id = "r4", conversation_id = "rejected",
  kind = "turn_completed", turn_id = "turn", run_id = "run" })
eq(value, nil); eq(boundary.code, "open_message_at_turn_terminal",
  "turn result before message terminal is rejected by manager contract")

print("semantic_projection_oracle_test: all assertions passed")
