tui = { now_ms = function() return 1000 end }
package.preload["nefor-tui"] = function()
  local nil_sentinel = {}
  return {
    util = {
      NIL = nil_sentinel,
      shallow_merge = function(base, patch)
        local merged = {}
        for k, v in pairs(base) do merged[k] = v end
        for k, v in pairs(patch) do
          if v == nil_sentinel then merged[k] = nil else merged[k] = v end
        end
        return merged
      end,
    },
  }
end

local transcript = require("libs.chat.transcript")

local function eq(actual, expected, message)
  if actual ~= expected then
    error((message or "values differ") .. ": expected " .. tostring(expected)
      .. ", got " .. tostring(actual), 2)
  end
end

local initial = {
  entries = {},
  in_flight = nil,
  pending = true,
  turn_started_at = 900,
}
local ended, handled = transcript.reduce_assistant_event(initial, {
  kind = "chat.stream.end",
  text = "",
  model = "structured-model",
  duration_ms = 75,
})
eq(handled, true)
eq(#ended.entries, 1)
eq(ended.entries[1].text, "")
eq(ended.entries[1].model, "structured-model")
eq(ended.pending_assistant_projection, 1)

local with_stats = transcript.reduce_assistant_event(ended, {
  kind = "chat.session.stats",
  last_turn_output_tokens = 42,
  last_turn_duration_ms = 80,
})
eq(with_stats.entries[1].output_tokens, 42)
eq(with_stats.entries[1].duration_ms, 80)

local projected = transcript.reduce_assistant_event(with_stats, {
  kind = "chat.message.append",
  role = "assistant",
  text = "validated answer",
})
eq(#projected.entries, 1, "durable answer reuses the provider-owned entry")
eq(projected.entries[1].text, "validated answer")
eq(projected.entries[1].model, "structured-model")
eq(projected.entries[1].output_tokens, 42)
eq(projected.entries[1].duration_ms, 80)
eq(projected.pending_assistant_projection, nil)

local ordinary = transcript.reduce_assistant_event({ entries = {} }, {
  kind = "chat.message.append",
  role = "assistant",
  text = "ordinary answer",
})
eq(#ordinary.entries, 1, "append without projection creates a new entry")
eq(ordinary.entries[1].text, "ordinary answer")
eq(ordinary.entries[1].kind, "text")

local function graph(run_id)
  return { role = "graph", kind = "graph_result", run_id = run_id }
end

local streaming = transcript.append_assistant_delta({ entries = {}, pending = true }, "lead answer")
streaming = transcript.append_graph_result(streaming, graph("first"))
streaming = transcript.append_graph_result(streaming, graph("second"))
eq(#streaming.entries, 1, "results stay buffered during a provider stream")
eq(#streaming.pending_graph_results, 2, "multiple results retain completion order")
streaming = transcript.finalize_assistant(streaming, nil, "test-model", 10)
streaming = transcript.flush_graph_results_if_stable(streaming)
eq(#streaming.entries, 3)
eq(streaming.entries[2].run_id, "first", "buffer flushes FIFO")
eq(streaming.entries[3].run_id, "second", "buffer flushes FIFO")

local open_tool = transcript.push_entry({ entries = {}, pending = false }, {
  role = "tool", kind = "tool_call", id = "tool-1", name = "mag", v = 1,
})
open_tool = transcript.append_graph_result(open_tool, graph("tool-result"))
eq(#open_tool.entries, 1, "result stays buffered while a tool is open")
open_tool = transcript.attach_tool_end(open_tool, "tool-1", "done", false)
open_tool = transcript.flush_graph_results_if_stable(open_tool)
eq(open_tool.entries[2].run_id, "tool-result", "tool completion is a stable boundary")

local idle = transcript.append_graph_result({ entries = {}, pending = false }, graph("idle"))
eq(idle.entries[1].run_id, "idle", "idle results append immediately")
eq(idle.pending_graph_results, nil)

local projected_round = transcript.finalize_assistant({
  entries = {}, pending = true, turn_started_at = 900,
}, "", "structured-model", 10)
projected_round = transcript.append_graph_result(projected_round, graph("projected"))
eq(#projected_round.entries, 1, "empty structured answer keeps result buffered")
projected_round = transcript.reduce_assistant_event(projected_round, {
  kind = "chat.message.append", role = "assistant", text = "final answer",
})
projected_round = transcript.flush_graph_results_if_stable(projected_round)
eq(projected_round.entries[1].text, "final answer")
eq(projected_round.entries[2].run_id, "projected",
  "final-answer projection stays ahead of its graph result")

local failed = transcript.append_graph_result({
  entries = {}, pending = false, pending_assistant_projection = 1,
}, graph("failed"))
failed = transcript.close_assistant_projection(failed)
failed = transcript.flush_graph_results_if_stable(failed)
eq(failed.entries[1].run_id, "failed", "failure closure flushes buffered results")

local cancelled = transcript.push_entry({ entries = {}, pending = true }, {
  role = "tool", kind = "tool_call", id = "cancelled-tool", name = "mag", v = 1,
})
cancelled = transcript.append_graph_result(cancelled, graph("cancelled"))
cancelled = transcript.close_lead_unit(cancelled)
cancelled = transcript.flush_graph_results(cancelled)
eq(cancelled.entries[1].output, "interrupted", "cancellation closes an open tool")
eq(cancelled.entries[1].error, true, "cancelled tool is visibly interrupted")
eq(cancelled.entries[2].run_id, "cancelled", "cancellation preserves buffered results")

print("transcript_test: all assertions passed")
