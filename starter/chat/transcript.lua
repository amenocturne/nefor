-- Transcript state mutators. Each function takes the current state
-- and returns the next state. Kept pure — `tui.now_ms` is the only
-- side-effect reach (read-only frame clock) and only when a turn
-- finalises and we record a per-turn duration on the fly.

local common = require("chat.common")
local shallow_merge = common.shallow_merge
local NIL_SENTINEL  = common.NIL_SENTINEL

local M = {}

function M.push_entry(state, entry)
  local entries = {}
  for i, v in ipairs(state.entries) do entries[i] = v end
  entries[#entries + 1] = entry
  return shallow_merge(state, { entries = entries })
end

function M.append_assistant_delta(state, delta)
  if state.in_flight ~= nil and state.entries[state.in_flight] then
    local entries = {}
    for i, v in ipairs(state.entries) do
      entries[i] = (i == state.in_flight)
        and shallow_merge(v, { text = (v.text or "") .. delta, streaming = true })
        or v
    end
    return shallow_merge(state, { entries = entries, pending = false })
  end
  local entries = {}
  for i, v in ipairs(state.entries) do entries[i] = v end
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
  -- Ensure we have an in-flight assistant entry; reasoning rides above it.
  local idx = state.in_flight
  local entries = {}
  for i, v in ipairs(state.entries) do entries[i] = v end
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
  entries[idx] = shallow_merge(cur, {
    streaming = true,
    reasoning = shallow_merge(prev, {
      text      = (prev.text or "") .. delta,
      streaming = true,
    }),
  })
  return shallow_merge(state, { entries = entries, pending = false })
end

function M.finalize_reasoning(state, duration_ms)
  if state.in_flight == nil then return state end
  local entries = {}
  for i, v in ipairs(state.entries) do
    if i == state.in_flight then
      local prev = v.reasoning or { text = "", streaming = true }
      entries[i] = shallow_merge(v, {
        reasoning = shallow_merge(prev, {
          streaming   = false,
          duration_ms = duration_ms or prev.duration_ms,
        }),
      })
    else
      entries[i] = v
    end
  end
  return shallow_merge(state, { entries = entries })
end

function M.finalize_assistant(state, final_text, model, duration_ms)
  local now = tui.now_ms()
  local turn_dur = duration_ms or (state.turn_started_at and (now - state.turn_started_at)) or nil
  if state.in_flight == nil then
    -- No in-flight entry. Two cases:
    --   1. Resume replay dropped the per-token deltas; this finalizer
    --      is the only event carrying the assistant text. Push a
    --      fully-formed entry from final_text so the message lands.
    --   2. Empty turn (e.g. error) — final_text is nil/empty; record
    --      only the durations.
    if final_text and #final_text > 0 then
      local entries = {}
      for i, v in ipairs(state.entries) do entries[i] = v end
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
  local entries = {}
  for i, v in ipairs(state.entries) do
    if i == state.in_flight then
      entries[i] = shallow_merge(v, {
        text        = final_text and #final_text > 0 and final_text or v.text,
        model       = model or v.model,
        duration_ms = duration_ms or v.duration_ms,
        streaming   = false,
      })
    else
      entries[i] = v
    end
  end
  return shallow_merge(state, {
    entries          = entries,
    in_flight        = NIL_SENTINEL,
    pending          = false,
    turn_started_at  = NIL_SENTINEL,
    last_turn_duration_ms = turn_dur,
  })
end

function M.attach_tool_end(state, id, output, error_flag)
  local entries = {}
  local matched = false
  for i, v in ipairs(state.entries) do
    if not matched and v.kind == "tool_call" and v.id == id then
      entries[i] = shallow_merge(v, { output = output or "", error = error_flag })
      matched = true
    else
      entries[i] = v
    end
  end
  if not matched then return state end
  return shallow_merge(state, { entries = entries })
end

return M
