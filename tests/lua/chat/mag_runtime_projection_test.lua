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

local durations = {
  { 0, "00s" }, { 999, "00s" }, { 1000, "01s" }, { 9000, "09s" },
  { 99000, "99s" }, { 100000, "01m" }, { 5999000, "99m" },
  { 6000000, "01h" }, { 359999000, "99h" }, { 360000000, "04d" },
  { 99 * 86400000, "99d" }, { 1000 * 86400000, "99d" },
}
for _, case in ipairs(durations) do
  eq(run_panel.fmt_elapsed_ms(case[1]), case[2], "fixed-width sidebar duration")
end

eq(run_panel.member_label("lead", "lead.entry"), "entry",
  "member label is relative to its exact parent")
eq(run_panel.member_label("lead", "leader.entry"), "leader.entry",
  "similarly named unrelated ids retain their canonical label")
eq(run_panel.member_label("lead", "other.lead.entry"), "other.lead.entry",
  "arbitrary prefixes are not stripped")

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

local function capability_values(projected)
  return preview_state.node(projected, "run", "worker.run-tool").streams.capability or {}
end

local function invocation(capability_id)
  return {
    run_id = "run", actor_id = "worker.run-tool", run_scope = "scope",
    capability_id = capability_id, principal = "subagent",
  }
end

local capabilities = {
  node_previews = {}, mag_arrivals = {}, scope_to_run = {}, capability_owners = {},
  capability_phases = {},
}
capabilities = preview_state.spawn(capabilities, "run", "worker.run-tool", "tool", {}, 0)
local public_process = {
  kind = "tool-gate.tool.invoke", id = "scope/process-1", name = "process.exec",
  args = { command = { "printf", "same" } }, invocation = invocation("scope/process-1"),
}
capabilities = preview_state.observe_capability(capabilities, public_process, 1)
capabilities = preview_state.observe_capability(capabilities, {
  kind = "basic-tools.tool.invoke", id = "private-forward-1", name = "process.exec",
  args = public_process.args, invocation = invocation("scope/process-1"),
}, 2)
eq(#capability_values(capabilities), 1,
  "public and Rust-backed forwarded envelopes project one causal start")
eq(capabilities.capability_owners["private-forward-1"], nil,
  "private forwarding IDs never become owner aliases")

capabilities = preview_state.observe_capability(capabilities, public_process, 3)
eq(#capability_values(capabilities), 1, "repeated canonical starts are idempotent")
capabilities = preview_state.observe_capability(capabilities, {
  kind = "tool.result", id = "scope/process-1", result = "same",
  invocation = invocation("scope/process-1"),
}, 4)
capabilities = preview_state.observe_capability(capabilities, {
  kind = "tool-gate.tool.result", id = "private-result-1", result = "same",
  invocation = invocation("scope/process-1"),
}, 5)
eq(#capability_values(capabilities), 2,
  "mirrored terminal envelopes project one causal result")
capabilities = preview_state.observe_capability(capabilities, {
  kind = "tool.result", id = "scope/process-1", result = "same",
  invocation = invocation("scope/process-1"),
}, 6)
eq(#capability_values(capabilities), 2, "repeated canonical terminals are idempotent")

local public_read = {
  kind = "tool-gate.tool.invoke", id = "scope/read-1", name = "read_file",
  args = { path = "same" }, invocation = invocation("scope/read-1"),
}
capabilities = preview_state.observe_capability(capabilities, public_read, 7)
capabilities = preview_state.observe_capability(capabilities, {
  kind = "read-only-tools.tool.invoke", id = "private-forward-2", name = "read_file",
  args = public_read.args, invocation = invocation("scope/read-1"),
}, 8)
capabilities = preview_state.observe_capability(capabilities, {
  kind = "tool-gate.tool.invoke", id = "scope/read-2", name = "read_file",
  args = { path = "same" }, invocation = invocation("scope/read-2"),
}, 9)
eq(#capability_values(capabilities), 4,
  "Lua-backed forwarding is hidden while distinct identical-looking calls remain visible")

capabilities = preview_state.observe_capability(capabilities, {
  kind = "tool.stream", id = "scope/read-2", stream = "stdout", text = "same",
}, 10)
capabilities = preview_state.observe_capability(capabilities, {
  kind = "tool.stream", id = "scope/read-2", stream = "stdout", text = "same",
}, 11)
eq(#capability_values(capabilities), 5,
  "adjacent chunks from the same terminal stream coalesce")
eq(capability_values(capabilities)[5].value.text, "samesame",
  "coalescing preserves byte order across chunks")
capabilities = preview_state.observe_capability(capabilities, {
  kind = "tool.stream", id = "scope/read-2", stream = "stderr", text = "warn",
}, 12)
capabilities = preview_state.observe_capability(capabilities, {
  kind = "tool.stream", id = "scope/read-2", stream = "stdout", text = "tail",
}, 13)
eq(#capability_values(capabilities), 7,
  "stream changes remain separate chronological activity")
eq(capability_values(capabilities)[6].value.kind, "stderr")
eq(capability_values(capabilities)[7].value.text, "tail")

print("mag_runtime_projection_test: all assertions passed")
