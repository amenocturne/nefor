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
local ended = transcript.finalize_assistant(initial, "validated answer", "structured-model", 75)
eq(#ended.entries, 1)
eq(ended.entries[1].text, "validated answer")
eq(ended.entries[1].model, "structured-model")

local with_stats = transcript.attach_latest_assistant_terminal(ended, {
  model = "late-model",
  duration_ms = 80,
  usage = { output_tokens = 42 },
})
eq(with_stats.entries[1].output_tokens, 42)
eq(with_stats.entries[1].duration_ms, 80)
eq(with_stats.entries[1].model, "late-model",
  "late turn metadata updates the completed assistant footer")

local ordinary = transcript.push_entry({ entries = {} }, {
  role = "assistant", kind = "text", text = "ordinary answer",
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

local completed_round = transcript.finalize_assistant({
  entries = {}, pending = true, turn_started_at = 900,
}, "final answer", "structured-model", 10)
completed_round = transcript.append_graph_result(completed_round, graph("completed"))
eq(completed_round.entries[1].text, "final answer")
eq(completed_round.entries[2].run_id, "completed",
  "completed canonical answer is stable before later graph results")

local cancelled = transcript.push_entry({ entries = {}, pending = true }, {
  role = "tool", kind = "tool_call", id = "cancelled-tool", name = "mag", v = 1,
})
cancelled = transcript.append_graph_result(cancelled, graph("cancelled"))
cancelled = transcript.close_lead_unit(cancelled)
cancelled = transcript.flush_graph_results(cancelled)
eq(cancelled.entries[1].output, "interrupted", "cancellation closes an open tool")
eq(cancelled.entries[1].error, true, "cancelled tool is visibly interrupted")
eq(cancelled.entries[2].run_id, "cancelled", "cancellation preserves buffered results")

local terminal = transcript.finalize_assistant({
  entries = {}, pending = true, turn_started_at = 900,
}, "provider answer", "structured-model", 10)
terminal = transcript.push_entry(terminal, {
  role = "assistant", kind = "text", text = "durable answer", v = 999,
})
terminal = transcript.close_lead_unit(terminal)
eq(terminal.pending, false, "terminal close clears the thinking placeholder")
eq(terminal.entries[1].text, "provider answer", "terminal close retains streamed content")
eq(terminal.entries[2].text, "durable answer", "terminal close retains durable content")
local terminal_again = transcript.close_lead_unit(terminal)
eq(#terminal_again.entries, 2, "terminal close is idempotent")
eq(terminal_again.entries[1].text, "provider answer")
eq(terminal_again.entries[2].text, "durable answer")

local reasoning_only = transcript.append_reasoning_delta({
  entries = {}, pending = true, turn_started_at = 900,
}, "partial reasoning")
reasoning_only = transcript.finalize_assistant(reasoning_only, nil, "reasoning-model")
reasoning_only = transcript.attach_latest_assistant_stats(reasoning_only, 7, nil)
reasoning_only = transcript.close_lead_unit(reasoning_only)
eq(reasoning_only.in_flight, nil, "abnormal close clears reasoning-only in-flight ownership")
eq(reasoning_only.entries[1].text, "", "abnormal close does not synthesize provider text")
eq(reasoning_only.entries[1].reasoning.text, "partial reasoning")
eq(reasoning_only.entries[1].reasoning.streaming, false, "reasoning no longer renders as active")
eq(reasoning_only.entries[1].streaming, false, "reasoning-only assistant no longer streams")
eq(reasoning_only.entries[1].model, "reasoning-model")
eq(reasoning_only.entries[1].output_tokens, 7)
local reasoning_again = transcript.close_lead_unit(reasoning_only)
eq(reasoning_again.entries[1].v, reasoning_only.entries[1].v,
  "duplicate abnormal close does not rewrite a settled reasoning entry")

local partial_text = transcript.append_reasoning_delta({
  entries = {}, pending = true, turn_started_at = 900,
}, "why")
partial_text = transcript.append_assistant_delta(partial_text, "partial answer")
partial_text = transcript.attach_latest_assistant_stats(partial_text, 3, 12)
partial_text = transcript.finalize_assistant(partial_text, nil, "partial-model")
partial_text = transcript.close_lead_unit(partial_text)
eq(partial_text.entries[1].text, "partial answer", "partial provider text is preserved")
eq(partial_text.entries[1].reasoning.text, "why", "partial reasoning is preserved")
eq(partial_text.entries[1].reasoning.streaming, false)
eq(partial_text.entries[1].streaming, false)
eq(partial_text.entries[1].model, "partial-model")
eq(partial_text.entries[1].duration_ms, 12)
eq(partial_text.entries[1].output_tokens, 3)

print("transcript_test: all assertions passed")
