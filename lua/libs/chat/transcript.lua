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

local function message_entry_index(state, message_id)
  if type(message_id) ~= "string" or message_id == "" then return nil end
  for index = #(state.entries or {}), 1, -1 do
    local entry = state.entries[index]
    if type(entry) == "table" and entry.message_id == message_id then return index end
  end
  return nil
end

local function live_entry_index(entries)
  for index = #(entries or {}), 1, -1 do
    local entry = entries[index]
    if type(entry) == "table" and entry.role == "assistant"
        and (entry.streaming == true
          or (type(entry.reasoning) == "table" and entry.reasoning.streaming == true)) then
      return index
    end
  end
  return nil
end

local function target_entry_index(state, message_id)
  if type(message_id) == "string" and message_id ~= "" then
    return message_entry_index(state, message_id)
  end
  return state.in_flight
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
  return state.active_turn_id ~= nil
    or state.pending == true
    or state.in_flight ~= nil
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

function M.append_assistant_delta(state, delta, message_id, turn_id)
  local idx = target_entry_index(state, message_id)
  if idx ~= nil and state.entries[idx] then
    local e = state.entries[idx]
    local new_entry = Entry.append_text(e, delta)
    local new_entries = replace_entry(state.entries, idx, new_entry)
    log.log("transcript", "delta in_flight=%d len=%d new_v=%d",
      idx, #delta, new_entry.v)
    return shallow_merge(state, {
      entries = new_entries,
      in_flight = live_entry_index(new_entries) or NIL_SENTINEL,
      pending = false,
    })
  end
  local new_entry = Entry.bind_canonical(Entry.assistant_stream(), message_id, turn_id)
  new_entry = Entry.append_text(new_entry, delta)
  local new_entries = append_entry(state.entries, new_entry)
  log.log("transcript", "delta new_stream v=%d count=%d",
    new_entry.v, #new_entries)
  return shallow_merge(state, {
    entries   = new_entries,
    in_flight = #new_entries,
    pending   = false,
  })
end

function M.append_reasoning_delta(state, delta, message_id, turn_id)
  local idx = target_entry_index(state, message_id)
  if idx == nil then
    local new_entry = Entry.bind_canonical(Entry.assistant_stream(), message_id, turn_id)
    new_entry = Entry.append_reasoning(new_entry, delta)
    local new_entries = append_entry(state.entries, new_entry)
    log.log("transcript", "reasoning_delta new_stream v=%d count=%d",
      new_entry.v, #new_entries)
    return shallow_merge(state, {
      entries = new_entries, in_flight = #new_entries,
      pending = false,
    })
  end
  local e = state.entries[idx]
  local new_entry = Entry.append_reasoning(e, delta)
  local new_entries = replace_entry(state.entries, idx, new_entry)
  log.log("transcript", "reasoning_delta in_flight=%d new_v=%d",
    idx, new_entry.v)
  return shallow_merge(state, {
    entries = new_entries,
    in_flight = live_entry_index(new_entries) or NIL_SENTINEL,
    pending = false,
  })
end

function M.finalize_assistant(state, final_text, model, duration_ms, message_id, turn_id)
  local now = tui.now_ms()
  local turn_dur = duration_ms
    or (state.turn_started_at and (now - state.turn_started_at))
    or nil
  local idx = target_entry_index(state, message_id)

  if idx == nil then
    if (final_text and #final_text > 0) or model ~= nil or duration_ms ~= nil then
      local new_entry = Entry.bind_canonical(Entry.assistant_stream(), message_id, turn_id)
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
      })
    end
    return shallow_merge(state, {
      pending              = false,
      turn_started_at      = NIL_SENTINEL,
      last_turn_duration_ms = turn_dur,
    })
  end

  local e = state.entries[idx]
  if e then
    if e.reasoning ~= nil and e.reasoning.streaming == true then
      e = Entry.finalize_reasoning(e)
    end
    local opts = { model = model or e.model, duration_ms = duration_ms or e.duration_ms }
    if final_text and #final_text > 0 then opts.text = final_text end
    if message_id ~= nil then e = Entry.bind_canonical(e, message_id, turn_id) end
    local new_entry = Entry.finalize(e, opts)
    local new_entries = replace_entry(state.entries, idx, new_entry)
    log.log("transcript", "finalize_assistant in_flight=%d new_v=%d",
      idx, new_entry.v)
    return shallow_merge(state, {
      entries              = new_entries,
      in_flight            = live_entry_index(new_entries) or NIL_SENTINEL,
      pending              = false,
      turn_started_at      = NIL_SENTINEL,
      last_turn_duration_ms = turn_dur,
    })
  end

  return shallow_merge(state, {
    in_flight            = live_entry_index(state.entries) or NIL_SENTINEL,
    pending              = false,
    turn_started_at      = NIL_SENTINEL,
    last_turn_duration_ms = turn_dur,
  })
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
    pending = false,
    turn_started_at = NIL_SENTINEL,
  })
end

-- Remove the entries bound to a canonical message the conversation authority
-- retracted (a structured-output attempt that failed validation). The next
-- attempt re-streams into the position this one occupied.
function M.discard_message(state, message_id)
  if type(message_id) ~= "string" or message_id == "" then return state end
  local entries, removed_before_in_flight, removed_in_flight = {}, 0, false
  for index, entry in ipairs(state.entries or {}) do
    if type(entry) == "table" and entry.message_id == message_id then
      if state.in_flight == index then removed_in_flight = true end
      if state.in_flight ~= nil and index < state.in_flight then
        removed_before_in_flight = removed_before_in_flight + 1
      end
    else
      entries[#entries + 1] = entry
    end
  end
  if #entries == #(state.entries or {}) then return state end
  local in_flight = state.in_flight
  if removed_in_flight or in_flight == nil then
    in_flight = NIL_SENTINEL
  else
    in_flight = in_flight - removed_before_in_flight
  end
  log.log("transcript", "discard_message id=%s count=%d", message_id, #entries)
  return shallow_merge(state, { entries = entries, in_flight = in_flight })
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

function M.attach_latest_assistant_terminal(state, terminal)
  for i = #state.entries, 1, -1 do
    local entry = state.entries[i]
    if entry.role == "assistant" then
      local updated = Entry.set_turn_terminal(entry, terminal)
      return shallow_merge(state, { entries = replace_entry(state.entries, i, updated) })
    end
  end
  return state
end

return M
