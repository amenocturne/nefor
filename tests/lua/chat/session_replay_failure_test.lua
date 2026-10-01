package.preload["nefor-tui"] = function()
  local sentinel = {}
  return { widget = {}, util = { NIL = sentinel,
    shallow_merge = function(base, patch)
      local result = {}
      for k, v in pairs(base) do result[k] = v end
      for k, v in pairs(patch) do
        if v == sentinel then result[k] = nil else result[k] = v end
      end
      return result
    end,
    bordered_box = function() end,
  } }
end
local handlers = require("libs.chat.controller").default_handlers()
local fail = handlers["sessions.transition_failed"]
local message = { operation = "resume", session_id = "selected", request_id = "resume-1",
  message = "Session reconstruction is incomplete: reader failed: EIO" }
local loading = { session_id = "selected", request_id = "resume-1", replayed = 64 }
local state = { session_id = "selected", entries = {}, pending = true,
  replay_mode = true, resume_loading = loading }
local stale = fail({ session_id = "selected", request_id = "stale" }, state)
assert(stale == state)
local next_state, effects = fail(message, state)
assert(next_state.session_id == "selected", "no fictitious rollback")
assert(next_state.resume_loading == nil and next_state.replay_mode == nil)
assert(next_state.pending == false and #effects == 0)
assert(next_state.entries[1].message == message.message)
assert(fail(message, next_state) == next_state, "duplicate failure inert")
local before = { session_id = "old", entries = {}, pending = true,
  pending_session_transition = { request_id = "resume-1" } }
local rejected = fail(message, before)
assert(rejected.session_id == "old" and rejected.pending_session_transition == nil)
assert(rejected.entries[1].message == message.message)

local cold = handlers["sessions.session_start"]({ session_id = "cold", from_resume = true },
  { entries = {}, runs = {}, usage = {} })
assert(cold.resume_loading.session_id == "cold" and cold.replay_mode == true)
local cold_failed = fail({ operation = "resume", session_id = "cold", message = "incomplete: EIO" }, cold)
assert(cold_failed.resume_loading == nil and cold_failed.replay_mode == nil)
assert(cold_failed.entries[#cold_failed.entries].message == "incomplete: EIO")
