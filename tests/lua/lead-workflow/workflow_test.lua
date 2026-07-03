-- starter/lead_workflow_test.lua — unit tests for the lead-workflow
-- actor. Driven from
-- `crates/nefor/tests/starter_lead_workflow_test.rs`. Mirrors the
-- harness pattern in `starter_agentic_workflow_test.rs`.

local lw   = require("lead-workflow")
local json = nefor.json
local agentic_loop = require("agentic-loop")
local sessions = require("sessions")

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
  agentic_loop._internals.reset()
  sessions._internals.reset_state()
  sessions.init()
  _test.set_plugins({ "reasoner-graph", "tool-gate", "nefor-tui" })
  _test.calls_clear()
end

local READ_ONLY_MAG = [[
(type Input)
(type Out)

(let [run (node "bash_command" {:command "echo ok"} : Input -> Out)
      out (node "sink" {} : Out -> Out)]
  (graph run -> out :terminal out))
]]

local WRITER_MAG = [[
(type Task)
(type Patch)

(let [build (node "agent" {:prompt "implement feature X"
                           :profile "fast"
                           :tools ["read_file" "write_file"]}
              : Task -> Patch)
      out   (node "sink" {} : Patch -> Patch)]
  (graph build -> out :terminal out))
]]

local function invoke_tool(id, name, args)
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = id,
    name = name,
    args = args or {},
  })
end

local function write_mag_file(id, file, content)
  invoke_tool(id, "mag", {
    action = "write",
    file = file,
    content = content,
  })
  local reply = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == id
  end)
  assert_true(reply ~= nil and reply.body.output and reply.body.output.status == "written",
    "mag write must create " .. file .. "; got " .. json.encode(_test.calls()))
end

local function execute_mag(id, file)
  invoke_tool(id, "mag", {
    action = "execute",
    file = file,
  })
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
-- mag write/execute builds + submits a graph
-- ------------------------------------------------------------------

do
  fresh()
  write_mag_file("firing-mag-write-1", "auth-login-map.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-mag-execute-1", "auth-login-map.mag")

  local calls = decode_calls()
  -- The actor should submit through agentic-loop, which flushes a
  -- tool.invoke{name=spawn_graph} targeting reasoner-graph.
  local invoke = find_call(calls, function(c)
    return c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"
        and c.target == "reasoner-graph"
  end)
  assert_true(invoke ~= nil,
    "mag execute must emit spawn_graph tool.invoke targeting reasoner-graph; got "
    .. json.encode(_test.calls()))
  assert_true(type(invoke.body.id) == "string" and #invoke.body.id > 0,
    "spawn_graph tool.invoke carries a non-empty id (run_id)")

  local graph = invoke.body.args and invoke.body.args.graph
  assert_true(type(graph) == "table", "args.graph present")
  assert_eq(#graph.nodes, 2, "two nodes in graph")
  assert_eq(graph.terminal, "out", "sink node becomes terminal")
  assert_eq(graph.nodes[1].id, "run", "first node id preserved")
  assert_eq(graph.nodes[1].reasoner, "bash_command",
    "MAG node reasoner is forwarded")
  assert_eq(graph.nodes[1].args.command, "echo ok",
    "MAG node args are forwarded")
  assert_eq(graph.nodes[2].id, "out", "sink node id preserved")
  local has_edge = false
  for _, e in ipairs(graph.edges or {}) do
    if e.from == "run" and e.to == "out" then has_edge = true end
  end
  assert_true(has_edge, "MAG graph edge is forwarded")

  -- Active run_id tracked.
  assert_true(type(lw._internals.state.active_runs[invoke.body.id]) == "table",
    "active_runs contains the dispatched run_id")

  feed("reasoner-graph", {
    kind      = "graph.node.fired",
    run_id    = invoke.body.id,
    node_id   = "run",
    firing_id = "firing-node-run",
    reasoner  = "bash_command",
  })
  feed("bash_command", {
    kind   = "tool.result",
    id     = "firing-node-run",
    result = {
      stdout = "ok\n",
      exit_code = 0,
      output_path = "/tmp/nefor-test/sessions/session-1/mag/runs/auth-login-map/run.output",
      output_relpath = "runs/auth-login-map/run.output",
    },
  })
  _test.calls_clear()
  invoke_tool("firing-graph-status-paths", "graph-status", { run_id = invoke.body.id })
  local status = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-graph-status-paths"
  end)
  assert_true(status ~= nil, "graph-status returns the active run")
  local node = status.body.output.run.nodes[1]
  assert_eq(node.output_path,
    "/tmp/nefor-test/sessions/session-1/mag/runs/auth-login-map/run.output",
    "graph-status exposes absolute output_path")
  assert_eq(node.output_relpath, "runs/auth-login-map/run.output",
    "graph-status exposes MAG-relative output_relpath")

  -- The actor also replies to the lead's invocation with the run_id.
  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-mag-execute-1"
  end)
  assert_true(reply ~= nil, "actor replies to mag execute invocation")
  assert_eq(reply.body.output.run_id, invoke.body.id,
    "reply carries the run_id")
  assert_eq(reply.body.output.status, "executing",
    "reply reports the graph is executing")
end

do
  fresh()
  invoke_tool("firing-bad-path", "mag", {
    action = "write",
    file = "../bad.mag",
    content = READ_ONLY_MAG,
  })
  local err = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result"
       and c.body.id == "firing-bad-path"
       and type(c.body.error) == "string"
  end)
  assert_true(err ~= nil, "invalid MAG path returns a tool.result error")
  assert_true(err.body.error:find("path traversal", 1, true) ~= nil,
    "invalid MAG path error explains path traversal")
end

do
  fresh()
  write_mag_file("firing-mag-write-compile", "deterministic-check.mag", READ_ONLY_MAG)
  _test.calls_clear()
  invoke_tool("firing-mag-compile", "mag", {
    action = "compile",
    file = "deterministic-check.mag",
  })
  local calls = decode_calls()
  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-mag-compile"
  end)
  assert_true(reply ~= nil, "mag compile returns a tool.result")
  assert_eq(reply.body.output.status, "compiled", "mag compile reports compiled status")
  assert_true(type(reply.body.output.preview) == "string"
              and reply.body.output.preview:find("bash_command", 1, true) ~= nil,
    "compile preview includes the compiled reasoner")
  local leaked = find_call(calls, function(c)
    return c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"
  end)
  assert_eq(leaked, nil, "mag compile previews only and does not submit spawn_graph")
end

-- ------------------------------------------------------------------
-- Per-execute switch: mag_execute_kernel routes to the MAG actor
-- kernel (mag.load + mag.execute over the bus) instead of
-- reasoner-graph, and mag.run_result closes the run. Default-off path
-- (spawn_graph) is covered by the block above.
-- ------------------------------------------------------------------

do
  fresh()
  write_mag_file("firing-kernel-write", "kernel-run.mag", READ_ONLY_MAG)
  _test.calls_clear()

  local config = require("config")
  local prev = config.active.mag_execute_kernel
  config.active.mag_execute_kernel = true

  execute_mag("firing-kernel-exec", "kernel-run.mag")

  -- Synchronous handshake: mag.load is sent first; mag.execute is withheld
  -- until the mag.loaded reply validates against the registry.
  local calls = decode_calls()
  local load = find_call(calls, function(c)
    return c.body.kind == "mag.load" and c.target == "mag"
  end)
  assert_true(load ~= nil,
    "kernel switch emits mag.load to the mag plugin; got " .. json.encode(_test.calls()))
  assert_eq(load.body.entry, "kernel-run.mag", "mag.load names the .mag entry file")

  local premature = find_call(calls, function(c) return c.body.kind == "mag.execute" end)
  assert_eq(premature, nil,
    "mag.execute must NOT be sent before mag.loaded (synchronous handshake)")
  local pre_reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-kernel-exec"
  end)
  assert_eq(pre_reply, nil, "no executing reply until the load handshake resolves")

  -- The load reply carries the registry (bash_command + sink cover READ_ONLY_MAG);
  -- it validates and releases mag.execute.
  _test.calls_clear()
  feed("mag", {
    kind        = "mag.loaded",
    in_reply_to = load.body.id,
    factories   = { "bash_command", "sink" },
  })
  calls = decode_calls()

  local exec = find_call(calls, function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
  end)
  assert_true(exec ~= nil,
    "mag.loaded releases mag.execute; got " .. json.encode(_test.calls()))
  assert_true(type(exec.body.session_id) == "string" and #exec.body.session_id > 0,
    "lead injects session_id on mag.execute")

  -- The reasoner-graph path must NOT run on the kernel switch.
  local leaked = find_call(calls, function(c)
    return c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"
  end)
  assert_eq(leaked, nil, "kernel path does not submit spawn_graph to reasoner-graph")

  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-kernel-exec"
  end)
  assert_true(reply ~= nil and reply.body.output ~= nil, "kernel execute replies executing")
  assert_eq(reply.body.output.engine, "mag-kernel", "reply reports the kernel engine")
  assert_eq(exec.body.run_id, reply.body.output.run_id,
    "mag.execute run_id matches the reply run_id")
  assert_true(type(lw._internals.state.active_runs[reply.body.output.run_id]) == "table",
    "kernel run tracked in active_runs")

  -- Terminal mag.run_result closes the run AND relays a fresh model turn
  -- carrying the sink output content read from the path (item 2 parity).
  local out_path = os.tmpname()
  local ofh = io.open(out_path, "w")
  ofh:write("SINK OUTPUT CONTENT")
  ofh:close()
  _test.calls_clear()
  feed("mag", {
    kind        = "mag.run_result",
    run_id      = reply.body.output.run_id,
    status      = "completed",
    output_path = out_path,
  })
  assert_eq(lw._internals.state.active_runs[reply.body.output.run_id], nil,
    "run archived after mag.run_result closes it")

  -- The completion is relayed as a fresh orchestrator turn (parity with the
  -- reasoner-graph completion path — a spawn_graph tool.invoke re-prompt).
  local turn = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"
        and c.target == "reasoner-graph"
  end)
  assert_true(turn ~= nil,
    "mag.run_result relays a fresh orchestrator turn; got " .. json.encode(_test.calls()))
  local prompt = turn.body.args and turn.body.args.graph
    and turn.body.args.graph.nodes[1].args.prompt
  assert_true(type(prompt) == "string"
              and prompt:find("SINK OUTPUT CONTENT", 1, true) ~= nil,
    "the relayed turn carries the sink output content read from the path")
  assert_true(prompt:find(out_path, 1, true) ~= nil,
    "the relayed turn carries the sink output path")

  -- The visible run-result block is appended to the chat surface — kernel
  -- parity with the reasoner-graph close_sub_graph → chat.graph_result.append.
  -- It carries status + run id + the sink output PATH, but NOT the output
  -- content (that rides the relayed turn above; no double-render).
  local block = find_call(decode_calls(), function(c)
    return c.body.kind == "chat.graph_result.append" and c.target == "nefor-tui"
  end)
  assert_true(block ~= nil,
    "mag.run_result appends a chat.graph_result block; got " .. json.encode(_test.calls()))
  assert_eq(block.body.run_id, reply.body.output.run_id,
    "result block names the run id")
  assert_eq(block.body.status, "success", "result block status is success")
  assert_true(type(block.body.output) == "string"
              and block.body.output:find(out_path, 1, true) ~= nil,
    "result block surfaces the sink output path")
  assert_true(block.body.output:find("SINK OUTPUT CONTENT", 1, true) == nil,
    "result block must NOT duplicate the relayed output content")

  os.remove(out_path)
  config.active.mag_execute_kernel = prev
end

-- A failed kernel run appends a failed run-result block carrying the error.
do
  fresh()
  write_mag_file("firing-kernel-fail-write", "kernel-fail.mag", READ_ONLY_MAG)
  _test.calls_clear()
  local config = require("config")
  local prev = config.active.mag_execute_kernel
  config.active.mag_execute_kernel = true

  execute_mag("firing-kernel-fail", "kernel-fail.mag")
  local load = find_call(decode_calls(), function(c)
    return c.body.kind == "mag.load" and c.target == "mag"
  end)
  assert_true(load ~= nil, "kernel switch emits mag.load")
  _test.calls_clear()
  feed("mag", {
    kind        = "mag.loaded",
    in_reply_to = load.body.id,
    factories   = { "bash_command", "sink" },
  })
  local reply = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-kernel-fail"
  end)
  assert_true(reply ~= nil and reply.body.output ~= nil, "kernel execute replies executing")

  _test.calls_clear()
  feed("mag", {
    kind   = "mag.run_result",
    run_id = reply.body.output.run_id,
    status = "failed",
    error  = "kernel boom",
  })
  local block = find_call(decode_calls(), function(c)
    return c.body.kind == "chat.graph_result.append" and c.target == "nefor-tui"
  end)
  assert_true(block ~= nil,
    "failed mag.run_result appends a chat.graph_result block; got " .. json.encode(_test.calls()))
  assert_eq(block.body.status, "failed", "result block status is failed")
  assert_true(type(block.body.error) == "string"
              and block.body.error:find("kernel boom", 1, true) ~= nil,
    "failed result block carries the error")

  config.active.mag_execute_kernel = prev
end

-- Kernel handshake rejects an unknown factory against the load-reply registry
-- BEFORE mag.execute is sent.
do
  fresh()
  write_mag_file("firing-kernel-badfactory-write", "kernel-bad.mag", READ_ONLY_MAG)
  _test.calls_clear()
  local config = require("config")
  local prev = config.active.mag_execute_kernel
  config.active.mag_execute_kernel = true

  execute_mag("firing-kernel-badfactory", "kernel-bad.mag")
  local load = find_call(decode_calls(), function(c)
    return c.body.kind == "mag.load" and c.target == "mag"
  end)
  assert_true(load ~= nil, "kernel switch emits mag.load")

  _test.calls_clear()
  -- Registry omits bash_command → validation must reject before execute.
  feed("mag", {
    kind        = "mag.loaded",
    in_reply_to = load.body.id,
    factories   = { "sink", "llm" },
  })
  local calls = decode_calls()
  local exec = find_call(calls, function(c) return c.body.kind == "mag.execute" end)
  assert_eq(exec, nil, "unknown factory blocks mag.execute")
  local err = find_call(calls, function(c)
    return c.body.kind == "tool.result"
       and c.body.id == "firing-kernel-badfactory"
       and type(c.body.error) == "string"
  end)
  assert_true(err ~= nil and err.body.error:find("unknown reasoner type", 1, true) ~= nil,
    "validation rejects the unknown factory with a clear error; got " .. json.encode(_test.calls()))
  assert_true(err.body.error:find("bash_command", 1, true) ~= nil,
    "rejection names the offending reasoner")

  config.active.mag_execute_kernel = prev
end

-- mag.loaded snapshots the kernel factory registry for reasoner validation.
do
  fresh()
  feed("mag", {
    kind      = "mag.loaded",
    factories = { "sink", "llm", "stub", "run-tool" },
  })
  local set = lw._internals.state.kernel_factories
  assert_true(type(set) == "table" and set.sink == true and set.llm == true,
    "mag.loaded populates the kernel factory registry snapshot")
end

-- ------------------------------------------------------------------
-- Approval gate: builder/writer roles are rejected without an
-- approved plan.
-- ------------------------------------------------------------------

do
  fresh()
  write_mag_file("firing-writer-write-no-plan", "feature-build.mag", WRITER_MAG)
  _test.calls_clear()
  execute_mag("firing-writer-no-plan", "feature-build.mag")
  local calls = decode_calls()
  local err = find_call(calls, function(c)
    return c.body.kind == "tool.result"
       and c.body.id == "firing-writer-no-plan"
       and type(c.body.error) == "string"
  end)
  assert_true(err ~= nil,
    "write-capable MAG execute without plan must return a tool.result error")
  assert_true(err.body.error:find("write%-capable agents") ~= nil
              and err.body.error:find("write%-review") ~= nil,
    "gate-error message names the write-review precondition")
  -- No spawn_graph should leak through.
  local leaked = find_call(calls, function(c)
    return c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"
  end)
  assert_true(leaked == nil,
    "gate rejection must NOT emit spawn_graph to reasoner-graph")
end

-- After /approve, the same writer graph is accepted.
do
  fresh()
  write_mag_file("firing-writer-write-with-plan", "feature-build.mag", WRITER_MAG)
  _test.calls_clear()
  -- Submit a plan + approve it via the live path.
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-pre",
    name = "write-review",
    args = { plan = "test plan", view = "inline" },
  })
  feed("nefor-tui", { kind = "chat.review.respond", text = "/approve" })
  _test.calls_clear()

  execute_mag("firing-writer-with-plan", "feature-build.mag")
  local calls = decode_calls()
  local invoke = find_call(calls, function(c)
    return c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"
        and c.target == "reasoner-graph"
  end)
  assert_true(invoke ~= nil,
    "after plan approval, write-capable MAG execute must emit spawn_graph; got "
    .. json.encode(_test.calls()))
end

-- ------------------------------------------------------------------
-- write-review is BLOCKING — no tool.result yet, plan slot records
-- the pending firing_id.
-- ------------------------------------------------------------------

do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-1",
    name = "write-review",
    args = { plan = "1. Read auth.lua\n2. Add login flow\n3. Test it", view = "inline" },
  })

  local calls = decode_calls()

  -- Plan envelope on the bus (for the chat surface).
  local sub = find_call(calls, function(c)
    return c.body.kind == "lead-workflow.plan.submitted"
  end)
  assert_true(sub ~= nil,
    "write-review must emit lead-workflow.plan.submitted; got "
    .. json.encode(_test.calls()))
  assert_eq(sub.body.plan, "1. Read auth.lua\n2. Add login flow\n3. Test it",
    "plan text in envelope")
  assert_eq(sub.body.plan_id, "plan-firing-plan-1",
    "plan_id is derived from the write-review firing id")

  -- BLOCKING: no tool.result yet for write-review.
  local pre = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-plan-1"
  end)
  assert_eq(pre, nil,
    "write-review is blocking — no tool.result until user verdict; got "
    .. json.encode(_test.calls()))

  -- Active plan state records the pending firing.
  local ap = lw._internals.state.active_plan
  assert_true(type(ap) == "table",               "active_plan recorded")
  assert_eq(ap.status, "pending",                "status starts pending")
  assert_eq(ap.pending_firing_id, "firing-plan-1",
    "pending_firing_id captures the write-review firing for later ack")
  assert_eq(ap.content, "1. Read auth.lua\n2. Add login flow\n3. Test it",
    "plan content stored verbatim")
end

-- ------------------------------------------------------------------
-- /approve resolves the deferred write-review ack with approval.
-- ------------------------------------------------------------------

do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-2",
    name = "write-review",
    args = { plan = "Plan A", view = "inline" },
  })
  _test.calls_clear()

  feed("nefor-tui", { kind = "chat.review.respond", text = "/approve" })

  local calls = decode_calls()

  local approved_env = find_call(calls, function(c)
    return c.body.kind == "lead-workflow.plan.approved"
  end)
  assert_true(approved_env ~= nil,
    "user /approve must emit lead-workflow.plan.approved; got "
    .. json.encode(_test.calls()))
  assert_eq(approved_env.body.approved, true, "approved=true on /approve")

  -- The deferred write-review ack resolves.
  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-plan-2"
  end)
  assert_true(reply ~= nil,
    "/approve resolves the deferred write-review tool.result")
  assert_eq(reply.body.output.status, "approved",
    "tool.result.output.status == 'approved'")
  assert_true(type(reply.body.output.notice) == "string"
              and #reply.body.output.notice > 0,
    "tool.result carries a notice directive for the model")

  -- State: approved, pending_firing_id cleared.
  local ap = lw._internals.state.active_plan
  assert_true(type(ap) == "table", "active_plan still present after verdict")
  assert_eq(ap.status, "approved", "status flipped to approved")
  assert_eq(ap.pending_firing_id, nil,
    "pending_firing_id cleared once the deferred ack fires")
end

-- ------------------------------------------------------------------
-- /reject resolves the deferred ack with rejection + reason.
-- ------------------------------------------------------------------

do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-3",
    name = "write-review",
    args = { plan = "Plan B", view = "inline" },
  })
  _test.calls_clear()

  feed("nefor-tui", { kind = "chat.review.respond",
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
    return c.body.kind == "tool.result" and c.body.id == "firing-plan-3"
  end)
  assert_true(reply ~= nil, "/reject resolves the deferred write-review ack")
  assert_eq(reply.body.output.status, "rejected",
    "tool.result.output.status == 'rejected'")
  assert_eq(reply.body.output.reason, "too aggressive timeline",
    "tool.result carries the rejection reason for the model")
end

-- ------------------------------------------------------------------
-- Non-verdict user message while plan pending — discards the plan and
-- resolves the deferred ack with status: "discarded".
-- ------------------------------------------------------------------

do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-discard",
    name = "write-review",
    args = { plan = "Plan C", view = "inline" },
  })
  _test.calls_clear()

  feed("nefor-tui", { kind = "chat.review.respond",
                      text = "actually can you also add step 4" })

  local calls = decode_calls()
  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-plan-discard"
  end)
  assert_true(reply ~= nil,
    "comment while plan pending must resolve the deferred ack")
  assert_eq(reply.body.output.status, "discarded",
    "comment resolves with status: 'discarded'")
  assert_eq(reply.body.output.comment, "actually can you also add step 4",
    "comment text rides along in the tool.result for the model")

  -- active_plan is flushed.
  assert_eq(lw._internals.state.active_plan, nil,
    "non-verdict comment discards the plan slot entirely")
end

-- ------------------------------------------------------------------
-- Single-use approval: a non-verdict user message AFTER /approve
-- flushes the approval so the next writer MAG execute is gated again.
-- ------------------------------------------------------------------

do
  fresh()
  write_mag_file("firing-writer-write-expired", "expired-build.mag", WRITER_MAG)
  _test.calls_clear()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-single-use",
    name = "write-review",
    args = { plan = "Plan D", view = "inline" },
  })
  feed("nefor-tui", { kind = "chat.review.respond", text = "/approve" })
  assert_eq(lw._internals.state.active_plan.status, "approved",
    "verdict applied")

  -- Next user message expires the approval.
  feed("nefor-tui", { kind = "chat.input.submit", text = "do this please" })
  assert_eq(lw._internals.state.active_plan, nil,
    "next user message after verdict flushes the approval")

  _test.calls_clear()
  execute_mag("firing-writer-expired", "expired-build.mag")
  local err = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result"
       and c.body.id == "firing-writer-expired"
       and type(c.body.error) == "string"
  end)
  assert_true(err ~= nil,
    "after approval expires, the writer MAG execute is gated again")
end

-- ------------------------------------------------------------------
-- Replay must not synthesize fresh chat.plan.append envelopes from
-- lead-workflow.plan.submitted. The session log already contains the
-- original chat.plan.append in chronological order; regenerating it
-- during replay appends historical plans at the tail on reattach.
-- ------------------------------------------------------------------

do
  fresh()
  local replay_window = require("core.history_replay")
  replay_window.set(true)
  feed("step", {
    kind         = "lead-workflow.plan.submitted",
    plan         = "Replayed plan body",
    submitted_at = "2026-05-08T00:00:00.000Z",
  })
  replay_window.set(false)

  local calls = decode_calls()
  local appended = find_call(calls, function(c)
    return c.body.kind == "chat.plan.append"
  end)
  assert_eq(appended, nil,
    "replayed plan.submitted must not synthesize chat.plan.append; got "
    .. json.encode(_test.calls()))
end

-- Replay does NOT rebuild state.active_plan. Approval/verdict state is
-- per-session — flushing on session boundary is the contract.
do
  fresh()
  local replay_window = require("core.history_replay")
  replay_window.set(true)
  feed("step", {
    kind         = "lead-workflow.plan.submitted",
    plan         = "Old session plan",
    submitted_at = "2026-05-08T00:00:00.000Z",
  })
  feed("step", {
    kind     = "lead-workflow.plan.approved",
    approved = true,
  })
  replay_window.set(false)
  assert_eq(lw._internals.state.active_plan, nil,
    "replay does NOT rebuild active_plan — each session starts with no carry-over approval")
end

-- Live path: the actor emits lead-workflow.plan.submitted from
-- write-review; the bus feeds that envelope back through receive_msg,
-- and the reducer re-emits chat.plan.append for the chat surface. The
-- test simulates the bus feedback explicitly because the test driver
-- doesn't wire actor.lua's bus subscription.
do
  fresh()
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-live",
    name = "write-review",
    args = { plan = "Live plan body", view = "inline" },
  })

  -- Simulate the bus feedback (in production, actor.lua's bus.on_event
  -- subscriber re-dispatches the actor's own emitted envelope through
  -- receive_msg).
  feed("step", {
    kind         = "lead-workflow.plan.submitted",
    plan         = "Live plan body",
    submitted_at = "2026-05-08T00:00:00.000Z",
  })

  local calls = decode_calls()
  local appended = find_call(calls, function(c)
    return c.body.kind == "chat.plan.append"
       and c.body.submitted_at == "2026-05-08T00:00:00.000Z"
  end)
  assert_true(appended ~= nil,
    "write-review on live path (with bus feedback) must emit chat.plan.append; got "
    .. json.encode(_test.calls()))
  assert_eq(appended.body.text, "Live plan body",
    "live chat.plan.append carries the plan text")
end

-- ------------------------------------------------------------------
-- Permission modes: auto declines human review prompts, while auto/yolo
-- bypass safe-mode writer gates for execution.
-- ------------------------------------------------------------------

do
  fresh()
  feed("tool-gate", { kind = "tool-gate.mode_changed", mode = "auto" })
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-auto",
    name = "write-review",
    args = { plan = "Auto mode plan", view = "inline" },
  })
  local calls = decode_calls()
  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-plan-auto"
  end)
  assert_true(reply ~= nil, "auto write-review returns immediately")
  assert_true(type(reply.body.error) == "string"
              and reply.body.error:find("permission_denied%[auto%]") ~= nil,
    "auto write-review returns a permission_denied[auto] error")
  assert_eq(lw._internals.state.active_plan, nil,
    "auto write-review must not open or approve a plan slot")
end

do
  fresh()
  feed("tool-gate", { kind = "tool-gate.mode_changed", mode = "auto" })
  write_mag_file("firing-writer-write-auto", "auto-build.mag", WRITER_MAG)
  _test.calls_clear()
  execute_mag("firing-writer-auto", "auto-build.mag")
  local calls = decode_calls()
  local invoke = find_call(calls, function(c)
    return c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"
  end)
  assert_true(invoke ~= nil, "auto bypasses the human plan gate for writer MAG execute")
end

do
  fresh()
  feed("tool-gate", { kind = "tool-gate.mode_changed", mode = "yolo" })
  write_mag_file("firing-writer-write-yolo", "yolo-build.mag", WRITER_MAG)
  _test.calls_clear()
  execute_mag("firing-writer-yolo", "yolo-build.mag")
  local calls = decode_calls()
  local invoke = find_call(calls, function(c)
    return c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"
  end)
  assert_true(invoke ~= nil, "yolo bypasses writer MAG execute approval gate")
end

-- ------------------------------------------------------------------
-- session_end terminates active graph AND flushes the plan slot
-- ------------------------------------------------------------------

do
  fresh()
  write_mag_file("firing-mag-write-end", "session-end.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-mag-execute-end", "session-end.mag")
  local run_id = next(lw._internals.state.active_runs)
  assert_true(type(run_id) == "string", "active_runs has an entry after MAG execute")

  -- Also submit a plan that's awaiting approval at session-end.
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-at-end",
    name = "write-review",
    args = { plan = "in-flight plan", view = "inline" },
  })
  assert_eq(lw._internals.state.active_plan.status, "pending",
    "plan slot is pending before session_end")
  _test.calls_clear()

  -- Direct invocation matches the bus.on_event subscriber the actor
  -- installs at module load.
  lw._internals.terminate_active_graph()

  local calls = decode_calls()
  local cancel = find_call(calls, function(c)
    return c.body.kind == "graph.cancel" and c.body.run_id == run_id
  end)
  assert_true(cancel ~= nil, "session_end emits graph.cancel for the active run")

  assert_eq(next(lw._internals.state.active_runs), nil,
    "active_runs cleared after termination")
  assert_eq(lw._internals.state.active_plan, nil,
    "active_plan flushed at session_end — no carry-over approval")
end
