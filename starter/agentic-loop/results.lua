-- starter/agentic-loop/results.lua — result-payload formatting helpers.
--
-- Pure helpers; no module-level state. Safe to require from anywhere.

local M = {}

-- Format a deferred kernel-run completion into a user-role message.
-- The `[mag_run(run_id=…) result]` / `[mag_run(run_id=…) FAILED]` tag is
-- load-bearing: the lead saw the dispatch ack promise a tagged follow-up,
-- and surfaces (mock provider, tests) key on the marker shape.
function M.format_deferred(completion)
  local run_id = completion.run_id or "?"
  if completion.status == "success" then
    return "[mag_run(run_id=" .. tostring(run_id) .. ") result]\n" ..
           "The MAG run you submitted earlier has finished. " ..
           "If the output says a report file was written, read that " ..
           "file before replying. Reply to the user only with the " ..
           "filepath and a short summary; for full details, tell them " ..
           "they can read the report themselves. Do not re-run the " ..
           "program or reproduce the full report.\n\n" ..
           "--- output ---\n" ..
           tostring(completion.output or "")
  else
    return "[mag_run(run_id=" .. tostring(run_id) .. ") FAILED]\n" ..
           "The MAG run you submitted earlier failed. Tell the user " ..
           "the run errored and offer to retry; do not silently " ..
           "re-run or fabricate a result.\n\n" ..
           "--- error ---\n" ..
           tostring(completion.error or completion.status or "unknown error")
  end
end

return M
