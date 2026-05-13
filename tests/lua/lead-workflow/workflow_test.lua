-- starter/lead_workflow_test.lua — unit tests for the lead-workflow
-- actor. Driven from
-- `crates/nefor/tests/starter_lead_workflow_test.rs`. Mirrors the
-- harness pattern in `starter_agentic_workflow_test.rs`.

local lw   = require("lead-workflow")
local json = nefor.json

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ",
      tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

local function decode_calls()
  local out = {}
  for _, c in ipairs(_test.calls()) do
    local ok, decoded = pcall(json.decode, c.payload)
    if ok and type(decoded) == "table" and type(decoded.body) == "table" then
      out[#out + 1] = { body = decoded.body, target = c.target, from = decoded.from }
    end
  end
  return out
end

local function find_call(calls, predicate)
  for _, c in ipairs(calls) do
    if predicate(c) then return c end
  end
  return nil
end

local function find_calls(calls, predicate)
  local out = {}
  for _, c in ipairs(calls) do
    if predicate(c) then out[#out + 1] = c end
  end
  return out
end

local function make_entry(origin, body)
  return {
    ts      = "2026-05-08T00:00:00.000Z",
    origin  = origin,
    payload = json.encode({ type = "event", from = origin, body = body }),
  }
end

local function feed(origin, body)
  lw.receive_msg(make_entry(origin, body))
end

local function fresh()
  lw._internals.reset()
  _test.set_plugins({ "reasoner-graph", "tool-gate", "nefor-tui" })
  _test.calls_clear()
end

-- ------------------------------------------------------------------
-- parse_approval_command — pin the command grammar
-- ------------------------------------------------------------------

do
  local parse = lw._internals.parse_approval_command
  local v, r = parse("/approve")
  assert_eq(v, true, "/approve → approved")
  assert_eq(r, nil,  "/approve → no reason")

  v, r = parse("/approve ship it")
  assert_eq(v, true,        "/approve <reason> still approved")
  assert_eq(r, "ship it",   "/approve <reason> captures reason")

  v, r = parse("/reject too risky")
  assert_eq(v, false,        "/reject → rejected")
  assert_eq(r, "too risky",  "/reject reason captured")

  v, r = parse("  /approve  ")  -- surrounding whitespace
  assert_eq(v, true, "/approve with whitespace still parses")

  assert_eq(parse("hello world"), nil, "non-command returns nil")
  assert_eq(parse("approve"),     nil, "missing slash returns nil")
end

-- ------------------------------------------------------------------
-- dispatch-graph builds + submits a graph
-- ------------------------------------------------------------------

do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-dispatch-1",
    name = "dispatch-graph",
    args = {
      nodes = {
        { id = "explore-1", role = "explorer",
          agent_args = { prompt = "Investigate auth.lua",
                         system_prompt = "You are an explorer.",
                         tool_allowlist = { "read", "grep" } } },
        { id = "build-1",   role = "builder",
          agent_args = { prompt = "Add login flow",
                         system_prompt = "You are a builder.",
                         tool_allowlist = { "read", "write", "edit" } },
          dependencies = { "explore-1" } },
      },
    },
  })

  local calls = decode_calls()
  -- The actor should emit a tool.invoke{name=spawn_graph} targeting reasoner-graph.
  local invoke = find_call(calls, function(c)
    return c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"
        and c.target == "reasoner-graph"
  end)
  assert_true(invoke ~= nil,
    "dispatch-graph must emit spawn_graph tool.invoke targeting reasoner-graph; got "
    .. json.encode(_test.calls()))
  assert_true(type(invoke.body.id) == "string" and #invoke.body.id > 0,
    "spawn_graph tool.invoke carries a non-empty id (run_id)")

  local graph = invoke.body.args and invoke.body.args.graph
  assert_true(type(graph) == "table", "args.graph present")
  assert_eq(#graph.nodes, 2, "two nodes in graph")
  assert_eq(graph.nodes[1].id, "explore-1", "first node id preserved")
  assert_eq(graph.nodes[1].reasoner, "agent",
    "role-keyed nodes become `agent` reasoners")
  assert_eq(graph.nodes[1].args.prompt, "Investigate auth.lua",
    "agent_args.prompt threaded through")
  assert_eq(graph.nodes[2].id, "build-1", "second node id preserved")
  -- Edge from explore-1 → build-1.
  local has_edge = false
  for _, e in ipairs(graph.edges or {}) do
    if e.from == "explore-1" and e.to == "build-1" then has_edge = true end
  end
  assert_true(has_edge, "dependency translates to graph edge")

  -- Active run_id tracked.
  assert_eq(lw._internals.state.active_run_id, invoke.body.id,
    "active_run_id == the dispatched run_id")

  -- The actor also replies to the lead's invocation with the run_id.
  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-dispatch-1"
  end)
  assert_true(reply ~= nil, "actor replies to dispatch-graph invocation")
  assert_eq(reply.body.output.run_id, invoke.body.id,
    "reply carries the run_id")
end

-- ------------------------------------------------------------------
-- write-review persists plan + emits envelope
-- ------------------------------------------------------------------

do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-1",
    name = "write-review",
    args = { plan = "1. Read auth.lua\n2. Add login flow\n3. Test it" },
  })

  local calls = decode_calls()

  -- Envelope on the bus.
  local sub = find_call(calls, function(c)
    return c.body.kind == "lead-workflow.plan.submitted"
  end)
  assert_true(sub ~= nil,
    "write-review must emit lead-workflow.plan.submitted; got "
    .. json.encode(_test.calls()))
  assert_true(type(sub.body.plan_id) == "string" and #sub.body.plan_id > 0,
    "plan_id minted")
  assert_eq(sub.body.plan, "1. Read auth.lua\n2. Add login flow\n3. Test it",
    "plan text in envelope")

  -- File on disk at the expected location (best-effort: only if
  -- nefor.fs.data_root() returns a writable path).
  local path = lw._internals.plan_path_for(sub.body.plan_id)
  if type(path) == "string" then
    local fh = io.open(path, "r")
    if fh then
      local contents = fh:read("*a")
      fh:close()
      assert_true(string.find(contents, "Add login flow", 1, true) ~= nil,
        "plan file contains the plan text; got: " .. tostring(contents))
      os.remove(path)
    end
    -- (we don't fail if the path isn't writable — the actor logs warn
    -- and proceeds; the envelope is the source of truth)
  end

  -- Tool reply with plan_id.
  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-plan-1"
  end)
  assert_true(reply ~= nil, "write-review replies with tool.result")
  assert_eq(reply.body.output.plan_id, sub.body.plan_id,
    "reply carries the plan_id")

  -- Active plan state.
  local ap = lw._internals.state.active_plan
  assert_true(type(ap) == "table",         "active_plan recorded")
  assert_eq(ap.plan_id, sub.body.plan_id,  "active_plan.plan_id matches submit")
  assert_eq(ap.approved, nil,              "active_plan.approved starts nil (pending)")
end

-- ------------------------------------------------------------------
-- await-approval resolves on /approve
-- ------------------------------------------------------------------

do
  fresh()
  -- Submit a plan first.
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-2",
    name = "write-review",
    args = { plan = "Plan A" },
  })
  local plan_id = lw._internals.state.active_plan.plan_id
  _test.calls_clear()

  -- Lead asks to await approval.
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-await-1",
    name = "await-approval",
    args = { plan_id = plan_id },
  })

  -- Should not have replied yet.
  local pre = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-await-1"
  end)
  assert_eq(pre, nil, "await-approval is blocking — no reply yet")

  -- User types /approve.
  _test.calls_clear()
  feed("nefor-tui", { kind = "chat.input.submit", text = "/approve" })

  local calls = decode_calls()

  local approved_env = find_call(calls, function(c)
    return c.body.kind == "lead-workflow.plan.approved"
  end)
  assert_true(approved_env ~= nil,
    "user /approve must emit lead-workflow.plan.approved; got "
    .. json.encode(_test.calls()))
  assert_eq(approved_env.body.approved, true, "approved=true on /approve")
  assert_eq(approved_env.body.plan_id, plan_id, "plan_id matches")

  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-await-1"
  end)
  assert_true(reply ~= nil, "await-approval resolves with tool.result on approve")
  assert_eq(reply.body.output.approved, true, "reply carries approved=true")
end

-- ------------------------------------------------------------------
-- await-approval resolves on /reject with reason
-- ------------------------------------------------------------------

do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-3",
    name = "write-review",
    args = { plan = "Plan B" },
  })
  local plan_id = lw._internals.state.active_plan.plan_id
  _test.calls_clear()

  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-await-2",
    name = "await-approval",
    args = { plan_id = plan_id },
  })

  _test.calls_clear()
  feed("nefor-tui", { kind = "chat.input.submit",
                      text = "/reject too aggressive timeline" })

  local calls = decode_calls()
  local approved_env = find_call(calls, function(c)
    return c.body.kind == "lead-workflow.plan.approved"
  end)
  assert_true(approved_env ~= nil, "rejection still emits plan.approved envelope")
  assert_eq(approved_env.body.approved, false, "approved=false on /reject")
  assert_eq(approved_env.body.approval_reason, "too aggressive timeline",
    "rejection reason captured")

  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-await-2"
  end)
  assert_true(reply ~= nil, "await-approval resolves on reject")
  assert_eq(reply.body.output.approved, false, "reply carries approved=false")
  assert_eq(reply.body.output.reason, "too aggressive timeline",
    "reply carries rejection reason")
end

-- ------------------------------------------------------------------
-- planApproved persists via replay — multi-plan edge case
-- ------------------------------------------------------------------

do
  fresh()
  -- Live: submit + approve plan A, then submit plan B (unapproved).
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-A",
    name = "write-review",
    args = { plan = "Plan A" },
  })
  local plan_a_id = lw._internals.state.active_plan.plan_id
  feed("nefor-tui", { kind = "chat.input.submit", text = "/approve" })
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-B",
    name = "write-review",
    args = { plan = "Plan B" },
  })
  local plan_b_id = lw._internals.state.active_plan.plan_id
  assert_true(plan_b_id ~= plan_a_id, "plan_id is fresh per submit")
  assert_eq(lw._internals.state.active_plan.approved, nil,
    "fresh-plan-B has nil (pending) approval")

  -- Now simulate /resume: clear actor state, fire replay markers, replay envelopes.
  fresh()
  local replay_window = require("core.history_replay")
  replay_window.set(true)

  -- Replayed envelopes from the on-disk session log: plan A submitted
  -- + approved, plan B submitted (no approval).
  feed("step", {
    kind         = "lead-workflow.plan.submitted",
    plan_id      = plan_a_id,
    plan         = "Plan A",
    submitted_at = "2026-05-08T00:00:00.000Z",
  })
  feed("step", {
    kind     = "lead-workflow.plan.approved",
    plan_id  = plan_a_id,
    approved = true,
  })
  feed("step", {
    kind         = "lead-workflow.plan.submitted",
    plan_id      = plan_b_id,
    plan         = "Plan B",
    submitted_at = "2026-05-08T00:01:00.000Z",
  })
  replay_window.set(false)

  -- Latest plan wins; its approval state is pending.
  local ap = lw._internals.state.active_plan
  assert_true(type(ap) == "table",      "active_plan restored after replay")
  assert_eq(ap.plan_id, plan_b_id,      "latest plan (B) wins after multi-plan replay")
  assert_eq(ap.approved, nil,           "plan B remains unapproved on replay")
end

-- planApproved survives a replay in the simpler single-plan case too.
do
  fresh()
  local replay_window = require("core.history_replay")
  replay_window.set(true)
  feed("step", {
    kind    = "lead-workflow.plan.submitted",
    plan_id = "plan-x",
    plan    = "Single plan",
    submitted_at = "2026-05-08T00:00:00.000Z",
  })
  feed("step", {
    kind     = "lead-workflow.plan.approved",
    plan_id  = "plan-x",
    approved = true,
  })
  replay_window.set(false)
  assert_eq(lw._internals.state.active_plan.plan_id, "plan-x",
    "single-plan replay restores plan_id")
  assert_eq(lw._internals.state.active_plan.approved, true,
    "single-plan replay restores approved=true")
end

-- ------------------------------------------------------------------
-- chat.plan.append re-emit: replaying lead-workflow.plan.submitted on
-- /resume must produce a chat.plan.append envelope so the chat surface
-- can re-render the yellow plan box. Without this, the actor's plan
-- state restores correctly but the visual entry is lost.
-- ------------------------------------------------------------------

do
  fresh()
  local replay_window = require("core.history_replay")
  replay_window.set(true)
  feed("step", {
    kind         = "lead-workflow.plan.submitted",
    plan_id      = "plan-replay-1",
    plan         = "Replayed plan body",
    submitted_at = "2026-05-08T00:00:00.000Z",
  })
  replay_window.set(false)

  local calls = decode_calls()
  local appended = find_call(calls, function(c)
    return c.body.kind == "chat.plan.append"
        and c.body.plan_id == "plan-replay-1"
  end)
  assert_true(appended ~= nil,
    "replayed plan.submitted must re-emit chat.plan.append for the chat surface; got "
    .. json.encode(_test.calls()))
  assert_eq(appended.body.text, "Replayed plan body",
    "replayed chat.plan.append carries the plan text")
  assert_eq(appended.body.submitted_at, "2026-05-08T00:00:00.000Z",
    "replayed chat.plan.append carries the original submitted_at")
end

-- Multi-plan replay: plan A submitted+approved, plan B submitted only.
-- chat.plan.append fires for both, plan A gets approved status via
-- chat.lua's direct subscription to lead-workflow.plan.approved (replay
-- already covers that path; we only need to assert chat.plan.append
-- re-emission here).
do
  fresh()
  local replay_window = require("core.history_replay")
  replay_window.set(true)
  feed("step", {
    kind         = "lead-workflow.plan.submitted",
    plan_id      = "plan-A",
    plan         = "Plan A body",
    submitted_at = "2026-05-08T00:00:00.000Z",
  })
  feed("step", {
    kind     = "lead-workflow.plan.approved",
    plan_id  = "plan-A",
    approved = true,
  })
  feed("step", {
    kind         = "lead-workflow.plan.submitted",
    plan_id      = "plan-B",
    plan         = "Plan B body",
    submitted_at = "2026-05-08T00:01:00.000Z",
  })
  replay_window.set(false)

  local calls = decode_calls()
  local append_calls = find_calls(calls, function(c)
    return c.body.kind == "chat.plan.append"
  end)
  assert_eq(#append_calls, 2,
    "expected chat.plan.append for both plan A and plan B on replay; got "
    .. tostring(#append_calls))
  local saw_a, saw_b = false, false
  for _, c in ipairs(append_calls) do
    if c.body.plan_id == "plan-A" then saw_a = true end
    if c.body.plan_id == "plan-B" then saw_b = true end
  end
  assert_true(saw_a, "plan A's chat.plan.append fires on replay")
  assert_true(saw_b, "plan B's chat.plan.append fires on replay")
end

-- Live path: the actor emits lead-workflow.plan.submitted from
-- write-review; the bus feeds that envelope back through receive_msg,
-- and the reducer re-emits chat.plan.append for the chat surface. The
-- test simulates the bus feedback explicitly because the test driver
-- doesn't wire actor.lua's bus subscription. Regression pin against
-- gating chat.plan.append emission on replay only.
do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-live",
    name = "write-review",
    args = { plan = "Live plan body" },
  })
  local plan_id = lw._internals.state.active_plan.plan_id

  -- Simulate the bus feedback (in production, actor.lua's bus.on_event
  -- subscriber re-dispatches the actor's own emitted envelope through
  -- receive_msg).
  feed("step", {
    kind         = "lead-workflow.plan.submitted",
    plan_id      = plan_id,
    plan         = "Live plan body",
    submitted_at = "2026-05-08T00:00:00.000Z",
  })

  local calls = decode_calls()
  local appended = find_call(calls, function(c)
    return c.body.kind == "chat.plan.append" and c.body.plan_id == plan_id
  end)
  assert_true(appended ~= nil,
    "write-review on live path (with bus feedback) must emit chat.plan.append; got "
    .. json.encode(_test.calls()))
  assert_eq(appended.body.text, "Live plan body",
    "live chat.plan.append carries the plan text")
end

-- ------------------------------------------------------------------
-- session_end terminates active graph
-- ------------------------------------------------------------------

do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-dispatch-end",
    name = "dispatch-graph",
    args = {
      nodes = {
        { id = "explore", role = "explorer",
          agent_args = { prompt = "x", system_prompt = "you are an explorer",
                         tool_allowlist = { "read" } } },
      },
    },
  })
  local run_id = lw._internals.state.active_run_id
  assert_true(type(run_id) == "string", "active_run_id set after dispatch")
  _test.calls_clear()

  -- Direct invocation matches the bus.on_event subscriber the actor
  -- installs at module load.
  lw._internals.terminate_active_graph()

  local calls = decode_calls()
  -- Broadcast (target == nil) per #53: the cancel envelope must reach
  -- every in-flight agent reasoner so it can interrupt its provider
  -- stream. reasoner-graph still receives the broadcast.
  local cancel = find_call(calls, function(c)
    return c.body.kind == "graph.cancel" and c.target == nil
  end)
  assert_true(cancel ~= nil,
    "session_end with active graph must emit graph.cancel as a broadcast; got "
    .. json.encode(_test.calls()))
  assert_eq(cancel.body.run_id, run_id, "graph.cancel carries the active run_id")

  local sysmsg = find_call(calls, function(c)
    return c.body.kind == "chat.message.append"
       and c.body.role == "system"
       and type(c.body.text) == "string"
       and string.find(c.body.text, "Graph terminated by user", 1, true) ~= nil
  end)
  assert_true(sysmsg ~= nil,
    "session_end must append a 'Graph terminated by user' system message; got "
    .. json.encode(_test.calls()))
  assert_eq(sysmsg.target, "nefor-tui", "system message targets nefor-tui")

  -- active_run_id cleared.
  assert_eq(lw._internals.state.active_run_id, nil,
    "active_run_id cleared after termination")
end

-- session_end with no active graph is a no-op (no spurious envelopes).
do
  fresh()
  lw._internals.terminate_active_graph()
  for _, c in ipairs(decode_calls()) do
    assert_true(c.body.kind ~= "graph.cancel",
      "session_end with no active graph must not emit graph.cancel")
    assert_true(not (c.body.kind == "chat.message.append"
                     and type(c.body.text) == "string"
                     and string.find(c.body.text, "Graph terminated", 1, true) ~= nil),
      "session_end with no active graph must not emit terminated system message")
  end
end

-- ------------------------------------------------------------------
-- run-close envelope clears active_run_id
-- ------------------------------------------------------------------

do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-dispatch-close",
    name = "dispatch-graph",
    args = {
      nodes = { { id = "x", role = "explorer", agent_args = { prompt = "x" } } },
    },
  })
  local run_id = lw._internals.state.active_run_id
  assert_true(type(run_id) == "string", "active_run_id set")

  feed("reasoner-graph", {
    kind   = "tool.result",
    id     = run_id,
    result = { status = "success", results = {} },
  })
  assert_eq(lw._internals.state.active_run_id, nil,
    "run-close tool.result for active run_id clears active_run_id")
end

-- A run-close for a different run_id must NOT clear active_run_id.
do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-dispatch-other",
    name = "dispatch-graph",
    args = {
      nodes = { { id = "x", role = "explorer", agent_args = { prompt = "x" } } },
    },
  })
  local run_id = lw._internals.state.active_run_id

  feed("reasoner-graph", {
    kind   = "tool.result",
    id     = "some-other-run-id",
    result = { status = "success", results = {} },
  })
  assert_eq(lw._internals.state.active_run_id, run_id,
    "tool.result for unrelated run_id must not clear our active_run_id")
end

-- ------------------------------------------------------------------
-- bad-args branches on dispatch-graph / write-review / await-approval
-- ------------------------------------------------------------------

do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "bad-1",
    name = "dispatch-graph",
    args = { nodes = {} },
  })
  local err = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "bad-1"
  end)
  assert_true(err ~= nil and type(err.body.error) == "string",
    "empty nodes list returns error")

  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "bad-2",
    name = "write-review",
    args = { plan = "" },
  })
  local err2 = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "bad-2"
  end)
  assert_true(err2 ~= nil and type(err2.body.error) == "string",
    "empty plan returns error")

  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "bad-3",
    name = "await-approval",
    args = {},
  })
  local err3 = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "bad-3"
  end)
  assert_true(err3 ~= nil and type(err3.body.error) == "string",
    "missing plan_id returns error")
end

-- ------------------------------------------------------------------
-- dispatch-graph terminal-node (sink) structural validation
--
-- A reasoner-graph sub-graph must have exactly one terminal node — one
-- node that no other node depends on, whose result becomes the graph's
-- return value. dispatch-graph is the role-aware contract layer that
-- enforces this lead-facing shape; reasoner-graph itself stays a
-- primitive that accepts any DAG.
-- ------------------------------------------------------------------

-- 0 sinks: cyclic dependency means every node has a successor. Common
-- failure mode when the lead tries to encode a loop in the graph
-- structure rather than at the agent level.
do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "dispatch_graph_rejects_zero_terminal_nodes",
    name = "dispatch-graph",
    args = {
      nodes = {
        { id = "a", role = "explorer", agent_args = { prompt = "x" },
          dependencies = { "b" } },
        { id = "b", role = "explorer", agent_args = { prompt = "y" },
          dependencies = { "a" } },
      },
    },
  })
  local err = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result"
        and c.body.id == "dispatch_graph_rejects_zero_terminal_nodes"
  end)
  assert_true(err ~= nil and type(err.body.error) == "string",
    "0-sink graph returns a tool.result error")
  assert_true(string.find(err.body.error, "0 terminal nodes", 1, true) ~= nil,
    "0-sink error message names the problem ('0 terminal nodes'); got: "
    .. tostring(err.body.error))
  -- The error should point the lead at how to fix it (cycle / loop-guard).
  assert_true(string.find(err.body.error, "dispatch-graph", 1, true) ~= nil,
    "0-sink error message identifies the validator ('dispatch-graph'); got: "
    .. tostring(err.body.error))
end

-- Disconnected components: two independent chains a→b and c→d.
-- Rejected so the lead splits them into two dispatch-graph calls.
do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "dispatch_graph_rejects_disconnected_components",
    name = "dispatch-graph",
    args = {
      nodes = {
        { id = "a", role = "explorer", agent_args = { prompt = "x" } },
        { id = "b", role = "explorer", agent_args = { prompt = "y" },
          dependencies = { "a" } },
        { id = "c", role = "explorer", agent_args = { prompt = "z" } },
        { id = "d", role = "explorer", agent_args = { prompt = "w" },
          dependencies = { "c" } },
      },
    },
  })
  local err = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result"
        and c.body.id == "dispatch_graph_rejects_disconnected_components"
  end)
  assert_true(err ~= nil and type(err.body.error) == "string",
    "disconnected graph returns a tool.result error")
  assert_true(string.find(err.body.error, "2 disconnected components", 1, true) ~= nil,
    "error names the component count; got: " .. tostring(err.body.error))
  local invoke = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"
  end)
  assert_eq(invoke, nil,
    "rejected disconnected graph must not produce a spawn_graph tool.invoke")
end

-- Connected multi-sink: explorer fans out to two siblings that share
-- the root but don't depend on each other. Accepted — reasoner-graph
-- returns result.results keyed by both sinks.
do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "dispatch_graph_accepts_connected_multi_sink",
    name = "dispatch-graph",
    args = {
      nodes = {
        { id = "root", role = "explorer", agent_args = { prompt = "x" } },
        { id = "a",    role = "builder",  agent_args = { prompt = "y" },
          dependencies = { "root" } },
        { id = "b",    role = "reviewer", agent_args = { prompt = "z" },
          dependencies = { "root" } },
      },
    },
  })
  local err = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result"
        and c.body.id == "dispatch_graph_accepts_connected_multi_sink"
        and type(c.body.error) == "string"
  end)
  assert_eq(err, nil, "connected multi-sink graph must NOT error")
  local invoke = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"
  end)
  assert_true(invoke ~= nil,
    "connected multi-sink graph must dispatch a spawn_graph tool.invoke")
end

-- Happy path: single-sink graph (chain) translates and dispatches as
-- before. Regression guard against the validator over-rejecting.
do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "dispatch_graph_accepts_single_terminal_node",
    name = "dispatch-graph",
    args = {
      nodes = {
        { id = "a", role = "explorer", agent_args = { prompt = "x" } },
        { id = "b", role = "explorer", agent_args = { prompt = "y" },
          dependencies = { "a" } },
        { id = "c", role = "explorer", agent_args = { prompt = "z" },
          dependencies = { "b" } },
      },
    },
  })
  local calls = decode_calls()
  local invoke = find_call(calls, function(c)
    return c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"
        and c.target == "reasoner-graph"
  end)
  assert_true(invoke ~= nil,
    "single-sink graph dispatches a spawn_graph tool.invoke")
  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result"
        and c.body.id == "dispatch_graph_accepts_single_terminal_node"
  end)
  assert_true(reply ~= nil and reply.body.error == nil,
    "single-sink graph replies success (no error field)")
end

-- Unknown tool name returns an error.
do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "bad-4",
    name = "no-such-tool",
    args = {},
  })
  local err = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "bad-4"
  end)
  assert_true(err ~= nil and type(err.body.error) == "string"
                          and string.find(err.body.error, "unknown tool", 1, true) ~= nil,
    "unknown tool name surfaces a clear error")
end

-- ------------------------------------------------------------------
-- await-approval resolves immediately if plan already has a verdict
-- (covers the race where the user approves before the lead asks)
-- ------------------------------------------------------------------

do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-race",
    name = "write-review",
    args = { plan = "Plan race" },
  })
  local plan_id = lw._internals.state.active_plan.plan_id
  feed("nefor-tui", { kind = "chat.input.submit", text = "/approve" })
  _test.calls_clear()

  -- await-approval AFTER the user approved.
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-await-race",
    name = "await-approval",
    args = { plan_id = plan_id },
  })

  local reply = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-await-race"
  end)
  assert_true(reply ~= nil,
    "await-approval against already-approved plan resolves immediately")
  assert_eq(reply.body.output.approved, true,
    "immediate resolution carries the approved verdict")
end

print("lead_workflow_test: ok")
