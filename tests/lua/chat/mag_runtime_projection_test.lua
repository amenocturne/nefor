package.preload["nefor-tui"] = function()
  local nil_sentinel = {}
  local function shallow_merge(left, right)
    local out = {}
    for key, value in pairs(left or {}) do out[key] = value end
    for key, value in pairs(right or {}) do
      if value == nil_sentinel then out[key] = nil else out[key] = value end
    end
    return out
  end
  return { util = { shallow_merge = shallow_merge, NIL = nil_sentinel,
    bordered_box = function() end } }
end

local preview_state = require("libs.chat.preview_state")
local run_panel = require("libs.chat.run_panel")

local function eq(actual, expected, message)
  if actual ~= expected then
    error((message or "values differ") .. ": expected " .. tostring(expected)
      .. ", got " .. tostring(actual), 2)
  end
end

local state = { runs = {} }
state = run_panel.mag_run_started(state, "run", "workflow", "lead", 0)
state = run_panel.actor_spawned(state, "run", "worker.entry", "entry", 100)
state = run_panel.actor_ready(state, "run", "worker.entry", 110)

local group = run_panel.build_groups(state.runs.run)[1]
eq(group.status, "pending", "constructed actor stays pending until first activation")
eq(group.first_start, nil, "spawn queue is not active time")

state = run_panel.actor_busy(state, "run", "worker.entry", 1000)
group = run_panel.build_groups(state.runs.run)[1]
eq(group.status, "running", "busy actor makes its logical node active")
eq(group.first_start, 1000, "active time begins at first busy transition")

state = run_panel.actor_idle(state, "run", "worker.entry", 1500)
state = run_panel.actor_busy(state, "run", "worker.entry", 2000)
state = run_panel.mag_run_complete(state, "run", "success", 2500)
group = run_panel.build_groups(state.runs.run)[1]
eq(group.last_finish, 2500, "completion closes an activation still working")
eq(group.active_ms, 1000, "logical node excludes the idle gap between yellow intervals")

local settled_state = { runs = {} }
settled_state = run_panel.mag_run_started(settled_state, "run", "workflow", "lead", 0)
settled_state = run_panel.actor_spawned(settled_state, "run", "worker.entry", "entry", 100)
settled_state = run_panel.actor_busy(settled_state, "run", "worker.entry", 1000)
settled_state = run_panel.actor_idle(settled_state, "run", "worker.entry", 1500)
settled_state = run_panel.mag_run_complete(settled_state, "run", "success", 10000)
group = run_panel.build_groups(settled_state.runs.run)[1]
eq(group.last_finish, 1500, "run completion does not add downstream wait time")
eq(group.active_ms, 500, "logical node excludes downstream wait time")

local overlap = { runs = {} }
overlap = run_panel.mag_run_started(overlap, "run", "workflow", "lead", 0)
overlap = run_panel.actor_spawned(overlap, "run", "worker.llm", "llm", 0)
overlap = run_panel.actor_spawned(overlap, "run", "worker.run-tool", "tool", 0)
overlap = run_panel.actor_busy(overlap, "run", "worker.llm", 1000)
overlap = run_panel.actor_busy(overlap, "run", "worker.run-tool", 1200)
overlap = run_panel.actor_idle(overlap, "run", "worker.llm", 1500)
overlap = run_panel.actor_idle(overlap, "run", "worker.run-tool", 1700)
group = run_panel.build_groups(overlap.runs.run)[1]
eq(group.active_ms, 700, "overlapping yellow members count as one logical interval")

local preview = {
  node_previews = {}, mag_arrivals = {}, scope_to_run = {}, capability_owners = {},
}
preview = preview_state.spawn(preview, "run", "worker.entry", "entry", {}, 100)
preview = preview_state.spawn(preview, "run", "worker.llm", "llm", {
  params = { system = "base instructions\n\n---\n\nImplement the focused change." },
}, 100)
eq(preview_state.node(preview, "run", "worker.llm").started_at_ms, nil,
  "preview spawn does not start active time")
preview = preview_state.lifecycle(preview, "run", "worker.llm", "working", 1000)
preview = preview_state.lifecycle(preview, "run", "worker.llm", "idle", 1500)
preview = preview_state.finish_run(preview, "run", "done", 10000)
local llm = preview_state.node(preview, "run", "worker.llm")
eq(llm.started_at_ms, 1000, "preview active time begins at busy")
eq(llm.settled_at_ms, 1500, "preview preserves last activity boundary")
eq(llm.active_ms, 500, "preview reports cumulative yellow time")

local is_agent, assignment = preview_state.agent_assignment(preview, "run", "worker")
eq(is_agent, true, "standard logical agent is recognized by its llm member")
eq(assignment, "Implement the focused change.", "delegated system overlay is exposed")

local source = {
  node_previews = {}, mag_arrivals = {}, scope_to_run = {}, capability_owners = {},
}
source = preview_state.spawn(source, "run", "lead.entry", "entry", {}, 0)
source = preview_state.spawn(source, "run", "lead.llm", "llm", {
  params = { system = "base instructions\n\n---\n\n# Runtime Context\n\nworkspace" },
}, 0)
source = preview_state.arrival(source, {
  run_id = "run", arrival_id = "arrival", from = "lead.entry", wire = "out",
  value = { messages = { {
    role = "user", content = { value = { prompt = "Analyze the session." } },
  } } },
}, 1)
is_agent, assignment = preview_state.agent_assignment(source, "run", "lead")
eq(is_agent, true)
eq(assignment, "Analyze the session.", "source agent assignment comes from its user arrival")

is_agent, assignment = preview_state.agent_assignment(source, "run", "plain")
eq(is_agent, false, "arbitrary MAG nodes do not get an agent assignment section")
eq(assignment, nil)

print("mag_runtime_projection_test: all assertions passed")
