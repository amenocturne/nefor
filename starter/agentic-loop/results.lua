-- starter/agentic-loop/results.lua — result-payload formatting helpers.
--
-- Pure helpers; no module-level state. Safe to require from anywhere.

local M = {}

local json = nefor.json

-- Serialise sub-graph results into a tool-friendly string. Preference:
--   1. When the submitted graph topology has an explicit `terminal`,
--      use that node's result.
--   2. Legacy graph-aware fallback: walk sink node(s) — nodes that no
--      other node depends on. Single sink -> that node's `output.text`.
--      Multi sink -> labeled concatenation in sorted id order.
--   3. Legacy heuristic for callers that didn't pass the graph:
--      `results.terminal.output.text` → keys containing "terminal" /
--      "out" / "final" → first node's `output.text`.
--   4. JSON-encoded results map (last resort).
--
-- The graph-aware path is what we want — node ids are caller-minted
-- and may not contain any of the legacy substrings (e.g. user picks
-- `test_explorer` / `test_reviewer`; the legacy heuristic would
-- silently surface the wrong node).
function M.extract_text(entry)
  if type(entry) ~= "table" or type(entry.output) ~= "table" then return nil end
  local out = entry.output
  return out.text or (out.final_answer and out.final_answer.text) or nil
end

-- Compute the sink-node ids from a submitted graph spec
-- (`{ nodes: [{ id, ... }], edges?: [{ from, to }] }`). A sink has no
-- outgoing edges — i.e. no other node depends on its output. Returns
-- a sorted list (deterministic order for multi-sink concatenation).
local function sink_ids(graph)
  if type(graph) ~= "table" or type(graph.nodes) ~= "table" then return nil end
  local has_successor = {}
  if type(graph.edges) == "table" then
    for _, e in ipairs(graph.edges) do
      if type(e) == "table" and type(e.from) == "string" then
        has_successor[e.from] = true
      end
    end
  end
  local sinks = {}
  for _, n in ipairs(graph.nodes) do
    if type(n) == "table" and type(n.id) == "string"
        and not has_successor[n.id] then
      sinks[#sinks + 1] = n.id
    end
  end
  table.sort(sinks)
  return sinks
end

function M.serialise_results(results, graph)
  if type(results) ~= "table" then return tostring(results) end

  -- Explicit-terminal path: reasoner-graph's canonical output contract.
  if type(graph) == "table" and type(graph.terminal) == "string" then
    local txt = M.extract_text(results[graph.terminal])
    if type(txt) == "string" then return txt end
  end

  -- Legacy graph-aware path: pick by topology, not by node name.
  local sinks = sink_ids(graph)
  if type(sinks) == "table" and #sinks > 0 then
    if #sinks == 1 then
      local txt = M.extract_text(results[sinks[1]])
      if type(txt) == "string" then return txt end
    else
      local parts = {}
      for _, sid in ipairs(sinks) do
        local txt = M.extract_text(results[sid])
        if type(txt) == "string" then
          parts[#parts + 1] = "[" .. sid .. "]\n" .. txt
        end
      end
      if #parts > 0 then return table.concat(parts, "\n\n") end
    end
  end

  -- Legacy heuristics — only reached when the caller didn't pass a
  -- graph or sink lookup didn't find usable text on the sink(s).
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
           "Give a short summary (2-3 sentences) of what was found and " ..
           "reference the output file path if one was written. " ..
           "Do NOT restate or reproduce the graph output — the user " ..
           "can read the file directly. Do not re-spawn the graph.\n\n" ..
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
