-- Transcript state mutators. Mutate state.entries in place for
-- single-entry updates (the hot path during streaming). Only allocate
-- a new entries table when structurally appending a new entry.

local common = require("chat.common")
local shallow_merge = common.shallow_merge
local NIL_SENTINEL  = common.NIL_SENTINEL

local M = {}

function M.push_entry(state, entry)
  local entries = state.entries
  entries[#entries + 1] = entry
  return shallow_merge(state, { entries = entries })
end

function M.append_assistant_delta(state, delta)
  if state.in_flight ~= nil and state.entries[state.in_flight] then
    local e = state.entries[state.in_flight]
    e.text = (e.text or "") .. delta
    e.streaming = true
    return shallow_merge(state, { pending = false })
  end
  local entries = state.entries
  entries[#entries + 1] = {
    role = "assistant", text = delta, kind = "stream", streaming = true,
  }
  return shallow_merge(state, {
    entries   = entries,
    in_flight = #entries,
    pending   = false,
  })
end

function M.append_reasoning_delta(state, delta)
  local idx = state.in_flight
  local entries = state.entries
  if idx == nil then
    entries[#entries + 1] = {
      role = "assistant", text = "", kind = "stream", streaming = true,
      reasoning = { text = delta, streaming = true },
    }
    return shallow_merge(state, {
      entries = entries, in_flight = #entries, pending = false,
    })
  end
  local cur = entries[idx]
  local prev = cur.reasoning or { text = "", streaming = true }
  prev.text = (prev.text or "") .. delta
  prev.streaming = true
  cur.reasoning = prev
  cur.streaming = true
  return shallow_merge(state, { pending = false })
end

function M.finalize_reasoning(state, duration_ms)
  if state.in_flight == nil then return state end
  local e = state.entries[state.in_flight]
  if not e then return state end
  local prev = e.reasoning or { text = "", streaming = true }
  prev.streaming = false
  prev.duration_ms = duration_ms or prev.duration_ms
  e.reasoning = prev
  return state
end

function M.finalize_assistant(state, final_text, model, duration_ms)
  local now = tui.now_ms()
  local turn_dur = duration_ms or (state.turn_started_at and (now - state.turn_started_at)) or nil
  if state.in_flight == nil then
    if final_text and #final_text > 0 then
      local entries = state.entries
      entries[#entries + 1] = {
        role        = "assistant",
        text        = final_text,
        kind        = "stream",
        streaming   = false,
        model       = model,
        duration_ms = duration_ms,
      }
      return shallow_merge(state, {
        entries          = entries,
        pending          = false,
        turn_started_at  = NIL_SENTINEL,
        last_turn_duration_ms = turn_dur,
      })
    end
    return shallow_merge(state, {
      pending = false, turn_started_at = NIL_SENTINEL,
      last_turn_duration_ms = turn_dur,
    })
  end
  local e = state.entries[state.in_flight]
  if e then
    if final_text and #final_text > 0 then e.text = final_text end
    e.model = model or e.model
    e.duration_ms = duration_ms or e.duration_ms
    e.streaming = false
  end
  return shallow_merge(state, {
    in_flight        = NIL_SENTINEL,
    pending          = false,
    turn_started_at  = NIL_SENTINEL,
    last_turn_duration_ms = turn_dur,
  })
end

function M.attach_tool_end(state, id, output, error_flag)
  for i = #state.entries, 1, -1 do
    local v = state.entries[i]
    if v.kind == "tool_call" and v.id == id then
      v.output = output or ""
      v.error = error_flag
      return state
    end
  end
  return state
end

return M
