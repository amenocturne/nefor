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
local run_projection = require("libs.mag-run-projection")

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

local wall_clock_durations = {
  { 0, "0s" }, { 2999, "2s" }, { 42000, "42s" },
  { 7 * 60000 + 12000, "07m 12s" },
  { 3600000 + 5000, "01h 00m 05s" },
  { 86400000 + 3 * 3600000 + 10 * 60000 + 15000, "01d 03h 10m 15s" },
}
for _, case in ipairs(wall_clock_durations) do
  eq(require("libs.chat.common").format_wall_clock_duration_ms(case[1]), case[2],
    "detailed wall-clock duration")
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

local group = run_panel.build_nodes(state.runs.run)[1]
eq(group.status, "pending", "constructed actor stays pending until first activation")
eq(group.first_start, nil, "spawn queue is not active time")

local observed_only = run_panel.build_nodes({
  nodes = {
    ["workflow.llm"] = { status = "pending", seq = 1 },
    ["workflow.run-tool"] = { status = "pending", seq = 2 },
  },
})
eq(#observed_only, 2, "fallback projection exposes each observed actor directly")
eq(#observed_only[1].children, 0,
  "fallback projection does not invent composite helper rows")

state = run_panel.actor_busy(state, "run", "worker.entry", 1000)
group = run_panel.build_nodes(state.runs.run)[1]
eq(group.status, "running", "busy actor makes its logical node active")
eq(group.first_start, 1000, "active time begins at first busy transition")

state = run_panel.actor_idle(state, "run", "worker.entry", 1500)
state = run_panel.actor_busy(state, "run", "worker.entry", 2000)
state = run_panel.mag_run_complete(state, "run", "success", 2500)
group = run_panel.build_nodes(state.runs.run)[1]
eq(group.last_finish, 2500, "completion closes an activation still working")
eq(group.active_ms, 1000, "logical node excludes the idle gap between yellow intervals")

local settled_state = { runs = {} }
settled_state = run_panel.mag_run_started(settled_state, "run", "workflow", "lead", 0)
settled_state = run_panel.actor_spawned(settled_state, "run", "worker.entry", "entry", 100)
settled_state = run_panel.actor_busy(settled_state, "run", "worker.entry", 1000)
settled_state = run_panel.actor_idle(settled_state, "run", "worker.entry", 1500)
settled_state = run_panel.mag_run_complete(settled_state, "run", "success", 10000)
group = run_panel.build_nodes(settled_state.runs.run)[1]
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
group = run_panel.build_nodes(overlap.runs.run)[1]
eq(group.active_ms, 700, "overlapping yellow members count as one logical interval")

local hierarchy = { runs = {}, sidebar_folds = {} }
hierarchy = run_panel.mag_run_started(hierarchy, "nested", "workflow", "lead", 0)
hierarchy = run_panel.nodes_declared(hierarchy, "nested", {
  { path = { "camera-stage" }, members = {} },
  { path = { "camera-stage", "agent" }, members = {} },
  { path = { "camera-stage", "agent", "llm" }, members = { "camera.llm" } },
  { path = { "camera-stage", "retry" }, members = { "camera.retry" } },
  { path = { "result" }, members = { "workflow.result" } },
})
hierarchy = run_panel.actor_spawned(hierarchy, "nested", "camera.llm", "llm", {}, 1)
hierarchy = run_panel.actor_spawned(hierarchy, "nested", "camera.retry", "retry", {}, 2)
hierarchy = run_panel.actor_spawned(hierarchy, "nested", "workflow.result", "output", {}, 3)
local roots = run_panel.build_nodes(hierarchy.runs.nested)
eq(#roots, 2, "run header exposes direct logical children without a wrapper node")
eq(roots[1].name, "camera-stage", "best-effort linearization starts at the entry stage")
eq(roots[2].name, "result", "best-effort linearization ends at the terminal node")
eq(roots[1].children[1].name, "agent", "a compound stage preserves its agent child")
eq(roots[1].children[2].name, "retry", "cycle feedback remains after the forward agent step")
eq(roots[1].children[1].children[1].name, "llm",
  "recursive hierarchy expands beyond one level")

local function actor_node(status)
  local node = { status = status }
  if status == "working" or status == "running" then node.started_at_ms = 10 end
  if status == "idle" then node.started_at_ms, node.settled_at_ms = 10, 20 end
  if status == "done" or status == "failed" or status == "error"
      or status == "killed" or status == "skipped" then
    node.finished_at_ms = 20
  end
  return node
end

local function project(logical_nodes, actor_statuses, run_status)
  local nodes = {}
  for actor_id, status in pairs(actor_statuses or {}) do
    nodes[actor_id] = actor_node(status)
  end
  return run_panel.build_nodes({
    logical_nodes = logical_nodes,
    nodes = nodes,
    completed_at_ms = run_status and 30 or nil,
    status = run_status,
  })
end

local settled_children = project({
  { path = { "parallel" }, members = {} },
  { path = { "parallel", "left" }, members = { "left.actor" } },
  { path = { "parallel", "right" }, members = { "right.actor" } },
}, {
  ["left.actor"] = "idle",
  ["right.actor"] = "done",
})[1]
eq(settled_children.status, "done",
  "settled logical children complete an explicit composite")
eq(#settled_children.children, 2, "composite retains its direct logical child count")
eq(#settled_children.members, 2, "composite members remain its explicit actors")

local recursive = project({
  { path = { "root" }, members = {} },
  { path = { "root", "child" }, members = {} },
  { path = { "root", "child", "leaf" }, members = { "leaf.actor" } },
}, { ["leaf.actor"] = "working" })[1]
eq(recursive.status, "running", "working grandchild makes the root running")
eq(recursive.children[1].status, "running", "working grandchild makes its parent running")
eq(recursive.children[1].children[1].status, "running", "working leaf is running")

recursive = project({
  { path = { "root" }, members = {} },
  { path = { "root", "child" }, members = {} },
  { path = { "root", "child", "leaf" }, members = { "leaf.actor" } },
}, { ["leaf.actor"] = "idle" })[1]
eq(recursive.status, "done", "settled grandchild completes every ancestor")
eq(recursive.children[1].status, "done", "settled grandchild completes its parent")
eq(recursive.children[1].children[1].status, "done", "settled leaf is done")

local composite_cases = {
  { "done+pending", "done", "pending", "pending" },
  { "failed+working", "failed", "working", "running" },
  { "killed+working", "killed", "working", "running" },
  { "done+done", "done", "done", "done" },
  { "done+killed", "done", "killed", "killed" },
  { "done+failed", "done", "failed", "failed" },
  { "failed+killed", "failed", "killed", "failed" },
}
for _, case in ipairs(composite_cases) do
  local root = project({
    { path = { "root" }, members = {} },
    { path = { "root", "one" }, members = { "one" } },
    { path = { "root", "two" }, members = { "two" } },
  }, { one = case[2], two = case[3] })[1]
  eq(root.status, case[4], "composite state " .. case[1])
end

for _, case in ipairs({
  { "idle", "done" }, { "failed", "failed" }, { "killed", "killed" },
}) do
  local leaf = project({
    { path = { "leaf" }, members = { "executed", "inactive-alternative" } },
  }, { executed = case[1], ["inactive-alternative"] = "pending" })[1]
  eq(leaf.status, case[2],
    "never-started alternative does not override executed leaf outcome " .. case[1])
end

local looping = { runs = {} }
looping = run_panel.mag_run_started(looping, "loop", "loop", "lead", 0)
looping = run_panel.nodes_declared(looping, "loop", {
  { path = { "root" }, members = {} },
  { path = { "root", "leaf" }, members = { "loop.actor" } },
})
looping = run_panel.actor_spawned(looping, "loop", "loop.actor", "fixture", 1)
looping = run_panel.actor_busy(looping, "loop", "loop.actor", 10)
looping = run_panel.actor_idle(looping, "loop", "loop.actor", 20)
eq(run_panel.build_nodes(looping.runs.loop)[1].status, "done",
  "idle loop actor settles leaf and ancestors")
looping = run_panel.actor_busy(looping, "loop", "loop.actor", 30)
eq(run_panel.build_nodes(looping.runs.loop)[1].status, "running",
  "later busy event reopens leaf and ancestors")
looping = run_panel.actor_idle(looping, "loop", "loop.actor", 40)
eq(run_panel.build_nodes(looping.runs.loop)[1].status, "done",
  "later idle event settles leaf and ancestors again")

local function terminal_fixture(id)
  local fixture = { runs = {} }
  fixture = run_panel.mag_run_started(fixture, id, id, "lead", 0)
  fixture = run_panel.nodes_declared(fixture, id, {
    { path = { "leaf" }, members = { id .. ".actor" } },
  })
  return run_panel.actor_spawned(fixture, id, id .. ".actor", "fixture", 1)
end

local successful = terminal_fixture("successful")
successful = run_panel.mag_run_complete(successful, "successful", "success", 10)
eq(run_panel.build_nodes(successful.runs.successful)[1].status, "done",
  "successful run completes an untouched leaf")
successful = run_panel.actor_killed(successful, "successful", "successful.actor", 11,
  "run_complete")
eq(run_panel.build_nodes(successful.runs.successful)[1].status, "done",
  "successful teardown cannot repaint a completed leaf")

local failed_run = terminal_fixture("failed-run")
failed_run = run_panel.mag_run_failed(failed_run, "failed-run", "failed", 10)
eq(run_panel.build_nodes(failed_run.runs["failed-run"])[1].status, "failed",
  "failed run fails an untouched leaf")

for _, reason in ipairs({ "killed", "reaped" }) do
  local killed_run = terminal_fixture(reason)
  killed_run = run_panel.actor_killed(killed_run, reason, reason .. ".actor", 10, reason)
  eq(run_panel.build_nodes(killed_run.runs[reason])[1].status, "killed",
    reason .. " teardown keeps the leaf killed")
end

local stage_key = run_panel.path_key({ "camera-stage" })
local agent_key = run_panel.path_key({ "camera-stage", "agent" })
hierarchy.sidebar_folds.nested = { [stage_key] = true, [agent_key] = true }
local hierarchy_rows = run_panel.row_model(hierarchy, 4)
eq(#hierarchy_rows, 6, "expanded recursive hierarchy contributes one row per visible node")
eq(hierarchy_rows[2].logical_node.name, "camera-stage", "first workflow row is the stage")
eq(hierarchy_rows[3].logical_node.name, "agent", "expansion reveals local child names")
eq(hierarchy_rows[4].logical_node.name, "llm", "nested expansion reveals the agent internals")
eq(hierarchy_rows[5].logical_node.name, "retry", "the stage sibling follows the expanded agent")
eq(hierarchy_rows[6].logical_node.name, "result", "the terminal root remains visible")

local preview = {
  node_previews = {}, mag_arrivals = {}, scope_to_run = {}, capability_owners = {},
}
preview = preview_state.spawn(preview, "run", "worker.entry", "entry", {}, 100)
preview = preview_state.spawn(preview, "run", "worker.llm", "llm", {
  params = { system = "base instructions\n\n---\n\nImplement the focused change." },
}, 100)
eq(preview_state.node(preview, "run", "worker.llm").started_at_ms, nil,
  "preview spawn does not start active time")
preview = preview_state.arrival(preview, {
  run_id = "run", arrival_id = "agent-result", from = "worker.llm",
  wire = "nefor.agent.Result", semantic_type_id = "sha256:answer",
  semantic_type = { kind = "named", name = "nefor.contracts.FinalAnswer" },
  constructor_id = "sha256:answer", value = { content = "Canonical answer" },
}, 900)
local typed_result = preview_state.agent_result(preview, "run", "worker")
eq(typed_result.value.content, "Canonical answer", "agent typed result is retained")
eq(typed_result.arrival_id, "agent-result", "agent result keeps provenance")
preview = preview_state.run_failure(preview, "run", {
  from = "worker.llm", failure = "actor", error = "late run failure",
}, 950)
eq(preview_state.agent_result(preview, "run", "worker").value.content, "Canonical answer",
  "typed output takes precedence over retained run failure")
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

local task = {
  node_previews = {}, mag_arrivals = {}, scope_to_run = {}, capability_owners = {},
}
local local_request_type = { kind = "named", name = "workflow.LocalRequest",
  body = { kind = "record", fields = {
    { name = "prompt", type = { kind = "primitive", name = "String" } },
  } },
}
task = preview_state.spawn(task, "task-run", "task", "source", {
  params = { value = { prompt = "Full human task prompt" }, value_type = "sha256:task" },
  output_type = local_request_type,
}, 0)
local authored_task = preview_state.node(task, "task-run", "task").source_fact
eq(authored_task.value.prompt, "Full human task prompt", "typed source value is retained before firing")
eq(authored_task.semantic_type_id, "sha256:task")
eq(authored_task.semantic_type, local_request_type,
  "source preview preserves the compiler-supplied local type descriptor")

local untyped = preview_state.spawn(task, "task-run", "untyped", "source", {
  params = { value = { prompt = "looks task-shaped" }, value_type = "sha256:unknown" },
}, 0)
eq(preview_state.node(untyped, "task-run", "untyped").source_fact.semantic_type, nil,
  "prompt-shaped values never fabricate nominal Task evidence")
task = preview_state.arrival(task, {
  run_id = "task-run", arrival_id = "task-arrival", from = "task",
  wire = "nefor.graph.Value", semantic_type_id = "sha256:task",
  semantic_type = local_request_type,
  constructor_id = "sha256:task", value = { prompt = "Full human task prompt" },
}, 1)
task = preview_state.finish_run(task, "task-run", "done", 2)
local emitted_task = preview_state.node(task, "task-run", "task").source_fact
eq(emitted_task.value.prompt, "Full human task prompt", "typed Task survives arrival and finish")
eq(emitted_task.arrival_id, "task-arrival", "source keeps exact emitted provenance")

local failed_agent = {
  node_previews = {}, mag_arrivals = {}, scope_to_run = {}, capability_owners = {},
}
failed_agent = preview_state.spawn(failed_agent, "failed", "worker.llm", "llm", {}, 0)
failed_agent = preview_state.run_failure(failed_agent, "failed", {
  from = "worker.llm", failure = "actor", error = "provider disconnected",
}, 1)
eq(preview_state.agent_result(failed_agent, "failed", "worker").value,
  "provider disconnected", "run failure is the no-result fallback")

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


local batches = {
  node_previews = {}, mag_arrivals = {}, scope_to_run = {}, capability_owners = {},
  capability_phases = {},
}
batches = preview_state.spawn(batches, "run", "worker.run-tool", "nefor.factory.run-tool", {}, 0)
local function fire_batch(projected, arrival_id, at_ms)
  projected = preview_state.arrival(projected, {
    run_id = "run", arrival_id = arrival_id, from = "worker.llm",
    wire = "generic-tool.ToolCalls", value = { calls = {} },
  }, at_ms)
  return preview_state.firing(projected, {
    run_id = "run", id = "worker.run-tool", port = "calls", shape = "single",
    arrivals = { { arrival_id = arrival_id, wire = "generic-tool.ToolCalls" } },
  }, at_ms)
end
local function observe(projected, body, at_ms)
  body.invocation = body.invocation or invocation(body.id)
  return preview_state.observe_capability(projected, body, at_ms)
end

eq(#preview_state.latest_tool_batch(batches, "run", "worker.run-tool"), 0,
  "never-fired run-tool has an explicit empty latest batch")
batches = fire_batch(batches, "tool-calls:1", 1)
batches = observe(batches, {
  kind = "tool-gate.tool.invoke", id = "old-1", name = "old-tool", args = {},
}, 2)
eq(#preview_state.latest_tool_batch(batches, "run", "worker.run-tool"), 1,
  "one invocation renders normally")
batches = fire_batch(batches, "tool-calls:2", 4)
batches = observe(batches, {
  kind = "tool-gate.tool.invoke", id = "new-1", name = "first", args = {},
}, 5)
batches = observe(batches, {
  kind = "tool-gate.tool.invoke", id = "new-2", name = "second", args = {},
}, 6)
batches = observe(batches, {
  kind = "tool.stream", id = "new-2", stream = "stdout", text = "second-stream",
}, 7)
batches = observe(batches, { kind = "tool.result", id = "new-2", output = "second-result" }, 8)
local latest = preview_state.latest_tool_batch(batches, "run", "worker.run-tool")
eq(#latest, 4, "latest batch includes every invocation and matching activity")
eq(latest[1].item.value.value.id, "new-1", "invocation order selects the first call")
eq(latest[2].item.value.value.id, "new-2", "invocation order selects the second call")
eq(latest[3].item.value.text, "second-stream", "stream follows its invocation")
eq(latest[4].item.value.value.output, "second-result", "result follows its invocation")
batches = observe(batches, { kind = "tool.result", id = "old-1", output = "late-old" }, 9)
latest = preview_state.latest_tool_batch(batches, "run", "worker.run-tool")
eq(#latest, 4, "late prior result cannot change the selected latest batch")
eq(latest[1].item.value.value.id, "new-1", "prior batch remains hidden after late result")

-- Model receipts use the same projection as the sidebar. Successful receipts
-- show top-level active time only; failed receipts retain the branch that was
-- active when termination landed.
local terminal = {
  status = "completed",
  logical_nodes = {
    { path = { "build" }, members = {} },
    { path = { "build", "worker" }, members = { "worker.llm" } },
  },
  nodes = { ["worker.llm"] = { status = "done", started_at_ms = 100, completed_at = 600 } },
  group_activity = {
    [run_projection.path_key({ "build" })] = { active_ms = 500 },
    [run_projection.path_key({ "build", "worker" })] = { active_ms = 500 },
  },
}
local success_tree = run_projection.format_tree(terminal, 600, true)
eq(success_tree:find("build", 1, true) ~= nil, true, "success receipt includes top-level node")
eq(success_tree:find("worker", 1, true), nil, "success receipt omits nested timings")
terminal.status = "killed"
terminal.nodes["worker.llm"].status = "killed"
terminal.active_at_terminal = {
  [run_projection.path_key({ "build" })] = true,
  [run_projection.path_key({ "build", "worker" })] = true,
}
local killed_tree = run_projection.format_tree(terminal, 600, true)
eq(killed_tree:find("worker", 1, true) ~= nil, true,
  "terminated receipt includes the branch active at termination")

print("mag_runtime_projection_test: all assertions passed")
