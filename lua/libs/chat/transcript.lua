local Entry   = require("libs.chat.entry")
local log     = require("libs.chat.log")
local common  = require("libs.chat.common")
local shallow_merge = common.shallow_merge
local NIL_SENTINEL  = common.NIL_SENTINEL

local M = {}

local function replace_entry(entries, idx, new_entry)
  local new_list = {}
  for i = 1, #entries do
    new_list[i] = (i == idx) and new_entry or entries[i]
  end
  return new_list
end

local function append_entry(entries, entry)
  local new_list = {}
  for i = 1, #entries do new_list[i] = entries[i] end
  new_list[#new_list + 1] = entry
  return new_list
end

function M.push_entry(state, entry)
  local new_entries = append_entry(state.entries, entry)
  log.log("transcript", "push role=%s kind=%s v=%d count=%d",
    entry.role or "?", entry.kind or "?", entry.v or 0, #new_entries)
  return shallow_merge(state, { entries = new_entries })
end

local function has_open_tool(entries)
  for i = #entries, 1, -1 do
    local entry = entries[i]
    if entry.kind == "tool_call" and entry.output == nil then return true end
    if entry.role == "assistant" or entry.role == "user" then return false end
  end
  return false
end

function M.lead_unit_open(state)
  return state.pending == true
    or state.in_flight ~= nil
    or state.pending_assistant_projection ~= nil
    or has_open_tool(state.entries or {})
end

function M.append_graph_result(state, entry)
  if not M.lead_unit_open(state) then return M.push_entry(state, entry) end
  local buffered = {}
  for i, value in ipairs(state.pending_graph_results or {}) do buffered[i] = value end
  buffered[#buffered + 1] = entry
  return shallow_merge(state, { pending_graph_results = buffered })
end

function M.flush_graph_results(state)
  local buffered = state.pending_graph_results or {}
  if #buffered == 0 then return state end
  local entries = state.entries
  for _, entry in ipairs(buffered) do entries = append_entry(entries, entry) end
  return shallow_merge(state, {
    entries = entries,
    pending_graph_results = NIL_SENTINEL,
  })
end

function M.flush_graph_results_if_stable(state)
  if M.lead_unit_open(state) then return state end
  return M.flush_graph_results(state)
end

function M.append_assistant_delta(state, delta)
  if state.in_flight ~= nil and state.entries[state.in_flight] then
    local e = state.entries[state.in_flight]
    local new_entry = Entry.append_text(e, delta)
    local new_entries = replace_entry(state.entries, state.in_flight, new_entry)
    log.log("transcript", "delta in_flight=%d len=%d new_v=%d",
      state.in_flight, #delta, new_entry.v)
    return shallow_merge(state, { entries = new_entries, pending = false })
  end
  local new_entry = Entry.assistant_stream()
  new_entry = Entry.append_text(new_entry, delta)
  local new_entries = append_entry(state.entries, new_entry)
  log.log("transcript", "delta new_stream v=%d count=%d",
    new_entry.v, #new_entries)
  return shallow_merge(state, {
    entries   = new_entries,
    in_flight = #new_entries,
    pending_assistant_projection = NIL_SENTINEL,
    pending   = false,
  })
end

function M.append_reasoning_delta(state, delta)
  local idx = state.in_flight
  if idx == nil then
    local new_entry = Entry.assistant_stream()
    new_entry = Entry.append_reasoning(new_entry, delta)
    local new_entries = append_entry(state.entries, new_entry)
    log.log("transcript", "reasoning_delta new_stream v=%d count=%d",
      new_entry.v, #new_entries)
    return shallow_merge(state, {
      entries = new_entries, in_flight = #new_entries,
      pending_assistant_projection = NIL_SENTINEL,
      pending = false,
    })
  end
  local e = state.entries[idx]
  local new_entry = Entry.append_reasoning(e, delta)
  local new_entries = replace_entry(state.entries, idx, new_entry)
  log.log("transcript", "reasoning_delta in_flight=%d new_v=%d",
    idx, new_entry.v)
  return shallow_merge(state, { entries = new_entries, pending = false })
end

function M.finalize_reasoning(state, duration_ms)
  if state.in_flight == nil then return state end
  local e = state.entries[state.in_flight]
  if not e then return state end
  local new_entry = Entry.finalize_reasoning(e, duration_ms)
  local new_entries = replace_entry(state.entries, state.in_flight, new_entry)
  log.log("transcript", "finalize_reasoning in_flight=%d new_v=%d",
    state.in_flight, new_entry.v)
  return shallow_merge(state, { entries = new_entries })
end

function M.finalize_assistant(state, final_text, model, duration_ms)
  local now = tui.now_ms()
  local turn_dur = duration_ms
    or (state.turn_started_at and (now - state.turn_started_at))
    or nil

  if state.in_flight == nil then
    if (final_text and #final_text > 0) or model ~= nil or duration_ms ~= nil then
      local new_entry = Entry.assistant_stream()
      new_entry = Entry.finalize(new_entry, {
        text = final_text, model = model, duration_ms = duration_ms,
      })
      local new_entries = append_entry(state.entries, new_entry)
      log.log("transcript", "finalize_assistant no_inflight new_v=%d count=%d",
        new_entry.v, #new_entries)
      return shallow_merge(state, {
        entries              = new_entries,
        pending              = false,
        turn_started_at      = NIL_SENTINEL,
        last_turn_duration_ms = turn_dur,
        pending_assistant_projection = (new_entry.text or "") == ""
          and #new_entries or NIL_SENTINEL,
      })
    end
    return shallow_merge(state, {
      pending              = false,
      turn_started_at      = NIL_SENTINEL,
      last_turn_duration_ms = turn_dur,
      pending_assistant_projection = NIL_SENTINEL,
    })
  end

  local e = state.entries[state.in_flight]
  if e then
    if e.reasoning ~= nil and e.reasoning.streaming == true then
      e = Entry.finalize_reasoning(e)
    end
    local opts = { model = model or e.model, duration_ms = duration_ms or e.duration_ms }
    if final_text and #final_text > 0 then opts.text = final_text end
    local new_entry = Entry.finalize(e, opts)
    local new_entries = replace_entry(state.entries, state.in_flight, new_entry)
    log.log("transcript", "finalize_assistant in_flight=%d new_v=%d",
      state.in_flight, new_entry.v)
    return shallow_merge(state, {
      entries              = new_entries,
      in_flight            = NIL_SENTINEL,
      pending              = false,
      turn_started_at      = NIL_SENTINEL,
      last_turn_duration_ms = turn_dur,
      pending_assistant_projection = (new_entry.text or "") == ""
        and state.in_flight or NIL_SENTINEL,
    })
  end

  return shallow_merge(state, {
    in_flight            = NIL_SENTINEL,
    pending              = false,
    turn_started_at      = NIL_SENTINEL,
    last_turn_duration_ms = turn_dur,
    pending_assistant_projection = NIL_SENTINEL,
  })
end

-- Structured-output providers finish the visible provider stream before the
-- agentic loop projects the validated final answer as chat.message.append.
-- Keep both halves in the provider entry so its reasoning, content, and stats
-- retain one chronological position even when an asynchronous graph result
-- lands between stream completion and durable projection.
function M.project_assistant_message(state, text)
  local idx = state.pending_assistant_projection
  if idx == nil then return state, false end
  local entry = idx and state.entries[idx] or nil
  if entry == nil or entry.role ~= "assistant" or (entry.text or "") ~= "" then
    return M.close_assistant_projection(state), false
  end
  local updated = Entry.set_text(entry, text)
  return shallow_merge(state, {
    entries = replace_entry(state.entries, idx, updated),
    pending_assistant_projection = NIL_SENTINEL,
  }), true
end

function M.close_assistant_projection(state)
  if state.pending_assistant_projection == nil then return state end
  return shallow_merge(state, { pending_assistant_projection = NIL_SENTINEL })
end

function M.close_lead_unit(state)
  local entries = state.entries or {}
  local closed_entries = entries
  local function close_entry(i, entry)
    if closed_entries == entries then
      closed_entries = {}
      for j = 1, #entries do closed_entries[j] = entries[j] end
    end
    closed_entries[i] = entry
  end

  local in_flight = state.in_flight
  if in_flight ~= nil and entries[in_flight] ~= nil then
    local assistant = entries[in_flight]
    if assistant.reasoning ~= nil and assistant.reasoning.streaming == true then
      assistant = Entry.finalize_reasoning(assistant)
    end
    if assistant.streaming == true then
      assistant = Entry.finalize(assistant)
    end
    if assistant ~= entries[in_flight] then close_entry(in_flight, assistant) end
  end

  for i = 1, #entries do
    local entry = entries[i]
    if entry.kind == "tool_call" and entry.output == nil then
      close_entry(i, Entry.set_output(entry, "interrupted", true))
    end
  end
  return shallow_merge(state, {
    entries = closed_entries,
    in_flight = NIL_SENTINEL,
    pending_assistant_projection = NIL_SENTINEL,
    pending = false,
    turn_started_at = NIL_SENTINEL,
  })
end

-- Canonical entry-state transition for assistant events shared by every chat
-- composition. Config reducers retain routing, capture, and aggregate-status
-- policy; this reducer alone decides whether a durable assistant append fills
-- a provider-owned empty entry or creates a new transcript entry.
function M.reduce_assistant_event(state, msg)
  if msg.kind == "chat.stream.end" then
    return M.finalize_assistant(state, msg.text, msg.model, msg.duration_ms), true
  end

  if msg.kind == "chat.session.stats" then
    local output_tokens = msg.last_turn_output_tokens or msg.completion_tokens
    local duration_ms = msg.last_turn_duration_ms or msg.duration_ms
    if output_tokens == nil and duration_ms == nil then return state, true end
    return M.attach_latest_assistant_stats(state, output_tokens, duration_ms), true
  end

  if msg.kind ~= "chat.message.append" or msg.role ~= "assistant" then
    return state, false
  end
  local text = msg.text or ""
  if text == "" then return state, true end
  local projected, matched = M.project_assistant_message(state, text)
  if matched then return projected, true end
  return M.push_entry(projected, {
    role = "assistant", text = text, kind = "text",
  }), true
end

function M.attach_tool_end(state, id, output, error_flag)
  for i = #state.entries, 1, -1 do
    local e = state.entries[i]
    if e.kind == "tool_call" and e.id == id then
      local new_entry = Entry.set_output(e, output or "", error_flag)
      local new_entries = replace_entry(state.entries, i, new_entry)
      log.log("transcript", "attach_tool_end id=%s idx=%d new_v=%d",
        id or "?", i, new_entry.v)
      return shallow_merge(state, { entries = new_entries })
    end
  end
  return state
end

function M.attach_latest_assistant_stats(state, output_tokens, duration_ms)
  for i = #state.entries, 1, -1 do
    local entry = state.entries[i]
    if entry.role == "assistant" then
      local updated = Entry.set_turn_stats(entry, output_tokens, duration_ms)
      return shallow_merge(state, { entries = replace_entry(state.entries, i, updated) })
    end
  end
  return state
end

return M
