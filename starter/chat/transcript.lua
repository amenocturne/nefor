local Entry   = require("chat.entry")
local log     = require("chat.log")
local common  = require("chat.common")
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
      entries = new_entries, in_flight = #new_entries, pending = false,
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
  -- Finalizes the streaming assistant entry only.  Lifecycle fields
  -- (pending, turn_started_at, last_turn_duration_ms) are owned by
  -- `chat.turn.idle` so that the TUI's busy-state tracks the
  -- orchestrator run, not the visible stream.
  if state.in_flight == nil then
    if final_text and #final_text > 0 then
      local new_entry = Entry.assistant_stream()
      new_entry = Entry.finalize(new_entry, {
        text = final_text, model = model, duration_ms = duration_ms,
      })
      local new_entries = append_entry(state.entries, new_entry)
      log.log("transcript", "finalize_assistant no_inflight new_v=%d count=%d",
        new_entry.v, #new_entries)
      return shallow_merge(state, {
        entries = new_entries,
      })
    end
    return state
  end

  local e = state.entries[state.in_flight]
  if e then
    local opts = { model = model or e.model, duration_ms = duration_ms or e.duration_ms }
    if final_text and #final_text > 0 then opts.text = final_text end
    local new_entry = Entry.finalize(e, opts)
    local new_entries = replace_entry(state.entries, state.in_flight, new_entry)
    log.log("transcript", "finalize_assistant in_flight=%d new_v=%d",
      state.in_flight, new_entry.v)
    return shallow_merge(state, {
      entries   = new_entries,
      in_flight = NIL_SENTINEL,
    })
  end

  return shallow_merge(state, {
    in_flight = NIL_SENTINEL,
  })
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

return M
