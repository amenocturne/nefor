local controls = require("libs.chat.workflow_controls")

local function eq(actual, expected, message)
  if actual ~= expected then
    error((message or "values differ") .. ": expected " .. tostring(expected)
      .. ", got " .. tostring(actual), 2)
  end
end

local function state(extra)
  local out = {
    entries = {}, input_value = "", runs = {}, escape_token_seq = 0,
  }
  for key, value in pairs(extra or {}) do out[key] = value end
  return out
end

local armed, decisions = controls.escape(state(), 100)
eq(#decisions, 1, "first Esc schedules one transition")
eq(decisions[1].kind, "schedule_escape_timeout")
eq(decisions[1].delay_ms, 600)
eq(armed.escape_token, 1)

local stale, stale_decisions = controls.escape_timeout(armed, 999)
eq(stale, armed, "stale timeout preserves state identity")
eq(#stale_decisions, 0, "stale timeout is a no-op")

local no_queue, no_queue_decisions = controls.escape_timeout(armed, 1)
eq(no_queue.escape_token, nil, "matching timeout clears token")
eq(#no_queue_decisions, 0, "timeout without queue is a no-op")

local queued = state({ queued_entry_idx = 2, pending = true })
queued.escape_token, queued.last_esc_ms = 4, 100
local timed, timed_decisions = controls.escape_timeout(queued, 4)
eq(timed.escape_token, nil)
eq(timed_decisions[1].kind, "steer_queued")

local double = state({
  entries = { { text = "lead" }, { text = "queued" }, { text = "tail" } },
  queued_entry_idx = 2,
  in_flight = 3,
  input_value = "draft",
  escape_token = 7,
  escape_token_seq = 7,
  last_esc_ms = 100,
})
local stopped, stop_decisions, stop_meta = controls.escape(double, 650)
eq(stop_decisions[1].kind, "hard_stop_lead")
eq(stopped.input_value, "queued draft", "queue is restored before draft with one space")
eq(#stopped.entries, 2)
eq(stopped.entries[2].text, "tail")
eq(stopped.in_flight, 2, "entry removal adjusts in-flight index")
eq(stopped.queued_entry_idx, nil)
eq(stopped.pending_user_echo_idx, nil, "restoration clears durable echo ownership")
eq(stop_meta.restored_queue, true)

local runs = {
  lead = { principal = "lead" },
  worker = { principal = "subagent" },
  done = { principal = "subagent", completed_at_ms = 42 },
}
local one_lead, one_lead_decisions = controls.confirm_termination(
  state({ runs = runs }), { scope = "one", run_id = "lead" })
eq(one_lead_decisions[1].kind, "hard_stop_lead")

local _, one_worker_decisions = controls.confirm_termination(
  state({ runs = runs }), { scope = "one", run_id = "worker" })
eq(one_worker_decisions[1].kind, "terminate_workflow")
eq(one_worker_decisions[1].run_id, "worker")

local _, done_decisions = controls.confirm_termination(
  state({ runs = runs }), { scope = "one", run_id = "done" })
eq(#done_decisions, 0, "completed workflow is not terminated")

local _, all_decisions = controls.confirm_termination(
  state({ runs = runs }), { scope = "all" })
local seen = {}
for _, decision in ipairs(all_decisions) do
  seen[decision.kind .. ":" .. tostring(decision.run_id or "")] = true
end
eq(seen["hard_stop_lead:"], true, "all hard-stops active lead")
eq(seen["terminate_all_non_lead_workflows:"], true, "all terminates active non-leads")
eq(seen["terminate_workflow:lead"], nil, "lead is not killed through workflow adapter")
eq(seen["terminate_workflow:done"], nil, "completed workflow remains untouched")

print("workflow_controls_test: all assertions passed")
