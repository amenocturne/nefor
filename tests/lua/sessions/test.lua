-- starter/sessions/test.lua — test escape hatches for the sessions actor.
--
-- Loaded ONLY from tests via `require("sessions.test")`. Production
-- code requires `require("sessions")` and never reaches this surface.

local sessions = require("sessions")
local i        = sessions._internals
local json     = nefor.json

return {
  -- Reset all module state. Used between test cases.
  _reset = i.reset_state,

  -- Drive the persistence path directly without pumping the bus.
  _persist_envelope = i.persist_envelope,

  -- Match the legacy entry-points exactly. Tests pass `nil` for the
  -- entry where the legacy callbacks didn't validate it; the resume
  -- variant decodes the envelope to extract `session_id`.
  _on_resume_request = function(entry)
    if type(entry) ~= "table" or type(entry.payload) ~= "string" then return end
    local ok, decoded = pcall(json.decode, entry.payload)
    if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
    local target = decoded.body.session_id
    if type(target) == "string" and target ~= "" then i.do_resume(target) end
  end,
  _on_new_request     = function(_entry) i.do_new() end,
  _on_engine_shutdown = function(_payload) i.do_shutdown() end,

  -- Helpers exposed for direct unit-testing.
  _uuid_v4   = i.uuid_v4,
  -- The data-root resolver delegates to nefor.fs.data_root(); tests
  -- override the binding to drive different roots.
  _data_root = i.compute_data_root,
}
