-- starter/agentic-loop/results.lua — result-payload formatting helpers.
--
-- Pure helpers; no module-level state. Safe to require from anywhere.

local M = {}

local json = nefor.json

-- Serialise sub-graph results into a tool-friendly string. Preference:
--   1. results.terminal.output.text — canonical agent-style exit.
--   2. Any node whose key contains "terminal", "out", or "final".
--   3. Any node's output.text (Lua pairs() order; last-resort).
--   4. JSON-encoded results map.
function M.extract_text(entry)
  if type(entry) ~= "table" or type(entry.output) ~= "table" then return nil end
  local out = entry.output
  return out.text or (out.final_answer and out.final_answer.text) or nil
end

function M.serialise_results(results)
  if type(results) ~= "table" then return tostring(results) end

  local terminal_text = M.extract_text(results.terminal)
  if type(terminal_text) == "string" then return terminal_text end

  for nid, entry in pairs(results) do
    if type(nid) == "string"
        and (string.find(nid, "terminal") or string.find(nid, "out") or string.find(nid, "final")) then
      local txt = M.extract_text(entry)
      if type(txt) == "string" then return txt end
    end
  end

  for _, entry in pairs(results) do
    local txt = M.extract_text(entry)
    if type(txt) == "string" then return txt end
  end

  return json.encode(results)
end

-- Format a deferred spawn_graph completion into a user-role message.
function M.format_deferred(completion)
  local run_id = completion.run_id or "?"
  if completion.status == "success" then
    return "[spawn_graph(run_id=" .. tostring(run_id) .. ") result]\n" ..
           "The sub-graph you submitted earlier has finished. " ..
           "Present the output below to the user as your reply to their " ..
           "original prompt. You may lightly reformat for readability; " ..
           "do not re-spawn the graph, do not fabricate missing content, " ..
           "do not re-analyse whether the result is complete — the " ..
           "sub-graph is the source of truth.\n\n" ..
           "--- output ---\n" ..
           tostring(completion.output or "")
  else
    return "[spawn_graph(run_id=" .. tostring(run_id) .. ") FAILED]\n" ..
           "The sub-graph you submitted earlier failed. Tell the user " ..
           "the sub-graph errored and offer to retry; do not silently " ..
           "re-spawn or fabricate a result.\n\n" ..
           "--- error ---\n" ..
           tostring(completion.error or completion.status or "unknown error")
  end
end

return M
