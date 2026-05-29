-- starter/reasoners/run_test.lua — unit tests for `run.lua` and
-- `run-wrappers.lua`.
--
-- Driven from `crates/nefor/tests/starter_run_reasoner_test.rs`,
-- which installs a stub `nefor.*` surface (json + engine.* + log.* +
-- bus.on_event) so `require("reasoners.run")` etc. succeed under a
-- bare mlua VM.

local json = nefor.json

local run          = require("reasoners.run")
local run_wrappers = require("reasoners.run-wrappers")

-- ------------------------------------------------------------------
-- helpers
-- ------------------------------------------------------------------

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

-- Build the body shape reasoners/init.lua's unwrap_invoke_body would
-- hand to handlers (firing_id at root + args/inputs/prev_state).
local function dispatch(handler, firing_id, args, prev_state)
  return handler({
    firing_id  = firing_id,
    args       = args,
    inputs     = {},
    prev_state = prev_state,
  })
end

-- Build a wire-shaped log entry that run.receive_msg accepts.
local function make_entry(origin, body)
  return {
    ts      = "2026-05-08T00:00:00.000Z",
    origin  = origin,
    payload = json.encode({ type = "event", from = origin, body = body }),
  }
end

local function find_call_with_kind(target_kind)
  for _, c in ipairs(_test.calls()) do
    local d = json.decode(c.payload)
    if d and d.body and d.body.kind == target_kind then
      return d, c.target
    end
  end
  return nil
end

-- Find a tool.result envelope with a specific id.
local function find_tool_result_for(id)
  for _, c in ipairs(_test.calls()) do
    local d = json.decode(c.payload)
    if d and d.body and d.body.kind == "tool.result" and d.body.id == id then
      return d.body
    end
  end
  return nil
end

-- Simulate tool-gate's tool.result fan-out for a specific tool_id.
local function deliver_tool_result(tool_id, output, error_)
  local body = { kind = "tool.result", id = tool_id }
  if output ~= nil then body.output = output end
  if error_ ~= nil then body.error = error_ end
  run.receive_msg(make_entry("tool-gate", body))
end

-- ------------------------------------------------------------------
-- parser unit tests — `parse_bash_output`
-- ------------------------------------------------------------------

do
  -- Stdout-only, no stderr marker, exit 0.
  local p = run._internals.parse_bash_output("hello\n[exit 0]")
  assert_eq(p.stdout, "hello\n", "stdout-only: stdout body preserved verbatim")
  assert_eq(p.stderr, "", "stdout-only: stderr is empty when no marker")
  assert_eq(p.exit_code, 0, "stdout-only: exit_code parsed")
end

do
  -- Stdout + stderr.
  local p = run._internals.parse_bash_output("out\n[stderr]\nerr\n[exit 0]")
  assert_eq(p.stdout, "out\n", "stdout+stderr: stdout extracted")
  assert_eq(p.stderr, "err\n", "stdout+stderr: stderr extracted")
  assert_eq(p.exit_code, 0, "stdout+stderr: exit_code parsed")
end

do
  -- Failing exit.
  local p = run._internals.parse_bash_output("[exit 1]")
  assert_eq(p.stdout, "", "failing-exit: empty stdout")
  assert_eq(p.stderr, "", "failing-exit: empty stderr")
  assert_eq(p.exit_code, 1, "failing-exit: exit_code parsed")
end

do
  -- Malformed: no exit footer.
  local p = run._internals.parse_bash_output("just text")
  assert_eq(p.stdout, "just text", "malformed: raw text surfaced as stdout")
  assert_eq(p.exit_code, nil, "malformed: nil exit_code")
end

-- ------------------------------------------------------------------
-- run reasoner — happy path: echo hello → exit 0
-- ------------------------------------------------------------------

do
  run._internals.reset()
  _test.calls_clear()

  local ret = dispatch(run.handle, "f-echo", { command = "echo hello" })
  assert_true(ret == nil, "run handle returns nil on accept")

  local invoke = find_call_with_kind("tool-gate.tool.invoke")
  assert_true(invoke ~= nil, "run dispatched tool-gate.tool.invoke")
  assert_eq(invoke.body.name, "bash", "tool-gate invoke targets the bash tool")
  assert_eq(invoke.body.args.command, "echo hello", "command forwarded verbatim")
  local tool_id = invoke.body.id
  assert_true(type(tool_id) == "string" and #tool_id > 0,
    "run minted a tool_id for correlation")

  _test.calls_clear()
  deliver_tool_result(tool_id, "hello\n[exit 0]")

  local result_env = find_tool_result_for("f-echo")
  assert_true(result_env ~= nil,
    "run emitted a terminal tool.result keyed off firing_id")
  assert_eq(result_env.result.stdout, "hello\n", "stdout surfaced")
  assert_eq(result_env.result.stderr, "", "stderr empty")
  assert_eq(result_env.result.exit_code, 0, "exit_code = 0")
end

-- ------------------------------------------------------------------
-- run reasoner — failing command: false → exit 1
-- ------------------------------------------------------------------

do
  run._internals.reset()
  _test.calls_clear()

  dispatch(run.handle, "f-false", { command = "false" })
  local invoke = find_call_with_kind("tool-gate.tool.invoke")
  local tool_id = invoke.body.id

  _test.calls_clear()
  -- bash plugin output for `false` is just the exit footer (no stdout,
  -- no stderr-marker).
  deliver_tool_result(tool_id, "[exit 1]")

  local result_env = find_tool_result_for("f-false")
  assert_true(result_env ~= nil, "run emitted terminal tool.result")
  assert_eq(result_env.result.exit_code, 1, "exit_code = 1 for failing command")
  assert_eq(result_env.result.stdout, "", "stdout empty")
  assert_eq(result_env.result.stderr, "", "stderr empty")
end

-- ------------------------------------------------------------------
-- run reasoner — bad args
-- ------------------------------------------------------------------

do
  run._internals.reset()
  _test.calls_clear()

  local ret = dispatch(run.handle, "f-bad", { command = "" })
  assert_true(type(ret) == "string" and #ret > 0,
    "run returns a string error for empty command")
  assert_eq(#_test.calls(), 0, "no envelopes emitted for invalid command")
end

do
  run._internals.reset()
  _test.calls_clear()

  local ret = dispatch(run.handle, "f-noargs", nil)
  assert_true(type(ret) == "string" and #ret > 0,
    "run returns a string error for missing args")
end

-- ------------------------------------------------------------------
-- run reasoner — infrastructure error from tool-gate
-- ------------------------------------------------------------------

do
  run._internals.reset()
  _test.calls_clear()

  dispatch(run.handle, "f-deny", { command = "rm -rf /" })
  local invoke = find_call_with_kind("tool-gate.tool.invoke")
  local tool_id = invoke.body.id

  _test.calls_clear()
  deliver_tool_result(tool_id, nil, "denied by gate policy")

  local result_env = find_tool_result_for("f-deny")
  assert_true(result_env ~= nil, "tool-gate denial surfaces a tool.result")
  assert_eq(result_env.error, "denied by gate policy",
    "infrastructure error forwarded verbatim")
  assert_eq(result_env.result, nil, "no result struct on infra error")
end

-- ------------------------------------------------------------------
-- run reasoner — unmatched tool_id is silently skipped
-- ------------------------------------------------------------------

do
  run._internals.reset()
  _test.calls_clear()

  -- A tool.result for some other firing's tool — no entry in
  -- tool_to_firing, so the run reasoner must not emit anything.
  deliver_tool_result("tool-not-ours", "[exit 0]")
  assert_eq(#_test.calls(), 0,
    "tool.result for unknown tool_id produces no emission")
end

-- ------------------------------------------------------------------
-- runCommand — registry resolution
-- ------------------------------------------------------------------

do
  run._internals.reset()
  _test.calls_clear()

  -- Default-registry hit: name="list" → "ls -la"
  dispatch(run_wrappers.handle, "f-list", { name = "list" })
  local rc_invoke = find_call_with_kind("tool.invoke")
  assert_true(rc_invoke ~= nil,
    "runCommand dispatched a tool.invoke for the inner `run` reasoner")
  assert_eq(rc_invoke.body.name, "run",
    "runCommand targets the `run` primitive")
  assert_eq(rc_invoke.body.args.args.command, "ls -la",
    "runCommand resolved 'list' against the default registry")
  assert_eq(rc_invoke.body.id, "f-list",
    "runCommand reuses the firing_id so the inner run's tool.result " ..
    "closes the same firing")
end

do
  run._internals.reset()
  _test.calls_clear()

  -- Caller-supplied registry overrides the default.
  dispatch(run_wrappers.handle, "f-custom", {
    name     = "compileProject",
    registry = { compileProject = "sbt --client compile" },
  })
  local rc_invoke = find_call_with_kind("tool.invoke")
  assert_eq(rc_invoke.body.args.args.command, "sbt --client compile",
    "runCommand prefers args.registry over the starter default")
end

do
  run._internals.reset()
  _test.calls_clear()

  -- Unknown name → string error, no emission.
  local ret = dispatch(run_wrappers.handle, "f-unknown", { name = "no-such" })
  assert_true(type(ret) == "string" and #ret > 0,
    "runCommand returns a string error for unknown name")
  assert_eq(#_test.calls(), 0, "no envelopes emitted for unknown name")
end

-- ------------------------------------------------------------------
-- runCommand → run end-to-end via the reasoners/init.lua dispatcher
-- ------------------------------------------------------------------
--
-- Wires up the full chain:
--   runCommand.handle → emits tool.invoke{name=run}
--   reasoners/init.lua's receive_msg → dispatches run.handle
--   run.handle → emits tool-gate.tool.invoke{name=bash}
--   tool-gate (simulated) → tool.result{id=tool_id}
--   run.receive_msg → emits tool.result{id=firing_id}
--
-- Confirms the wrapper layer's `name → command` mapping does end up at
-- the bash tool with the resolved command, and the inner run's
-- terminal envelope closes the wrapper's firing.

do
  local reasoners = require("reasoners")
  reasoners._internals.reset()
  reasoners._internals.seed()  -- no-op for the test path; safe to call
  run._internals.reset()
  _test.calls_clear()

  -- Step 1: dispatch the runCommand tool.invoke through reasoners.
  local invoke_body = {
    kind = "tool.invoke",
    id   = "f-e2e",
    name = "runCommand",
    args = { args = { name = "pwd" } },
  }
  reasoners.receive_msg({
    ts      = "2026-05-08T00:00:00.000Z",
    origin  = "reasoner-graph",
    payload = json.encode({ type = "event", from = "reasoner-graph", body = invoke_body }),
  })

  -- runCommand should have re-emitted as tool.invoke{name=run},
  -- which reasoners' receive_msg (called recursively here would NOT
  -- happen — the bus dispatch is async). Instead we manually deliver
  -- the next envelope back through reasoners.receive_msg so the run
  -- handler runs.
  local rc_invoke = find_call_with_kind("tool.invoke")
  assert_true(rc_invoke ~= nil, "runCommand emitted tool.invoke")
  assert_eq(rc_invoke.body.name, "run", "inner invoke targets run")
  assert_eq(rc_invoke.body.args.args.command, "pwd",
    "runCommand resolved 'pwd' against the default registry")

  reasoners.receive_msg({
    ts      = "2026-05-08T00:00:00.000Z",
    origin  = "runCommand",
    payload = json.encode({ type = "event", from = "runCommand", body = rc_invoke.body }),
  })

  local bash_invoke = find_call_with_kind("tool-gate.tool.invoke")
  assert_true(bash_invoke ~= nil,
    "run reasoner emitted tool-gate.tool.invoke for bash")
  assert_eq(bash_invoke.body.name, "bash", "targets bash")
  assert_eq(bash_invoke.body.args.command, "pwd", "command threaded all the way through")
  local tool_id = bash_invoke.body.id

  _test.calls_clear()
  -- Step 4: simulate tool-gate's tool.result for the bash invocation.
  reasoners.receive_msg(make_entry("tool-gate", {
    kind   = "tool.result",
    id     = tool_id,
    output = "/tmp/x\n[exit 0]",
  }))

  -- The terminal tool.result must be keyed off the wrapper's firing_id.
  local terminal = find_tool_result_for("f-e2e")
  assert_true(terminal ~= nil,
    "run reasoner emitted terminal tool.result keyed off the wrapper's firing_id")
  assert_eq(terminal.result.stdout, "/tmp/x\n", "stdout threaded through wrapper layer")
  assert_eq(terminal.result.exit_code, 0, "exit_code threaded through wrapper layer")
end

-- ------------------------------------------------------------------
-- Malformed tool.invoke payloads return owed errors instead of raising
-- ------------------------------------------------------------------

do
  local reasoners = require("reasoners")
  reasoners._internals.reset()
  reasoners._internals.seed()
  run._internals.reset()
  _test.calls_clear()

  reasoners.receive_msg({
    ts      = "2026-05-08T00:00:00.000Z",
    origin  = "reasoner-graph",
    payload = json.encode({
      type = "event",
      from = "reasoner-graph",
      body = {
        kind = "tool.invoke",
        id   = "f-malformed",
        name = "run",
        args = "not a table",
      },
    }),
  })

  local terminal = find_tool_result_for("f-malformed")
  assert_true(terminal ~= nil,
    "malformed invoke must still close the firing with tool.result")
  assert_true(type(terminal.error) == "string"
      and terminal.error:match("missing args") ~= nil,
    "malformed invoke reports a structured handler error")
end

do
  local reasoners = require("reasoners")
  reasoners._internals.reset()
  _test.calls_clear()

  reasoners.receive_msg({
    ts      = "2026-05-08T00:00:00.000Z",
    origin  = "reasoner-graph",
    payload = json.encode({ type = "event", from = "reasoner-graph", body = "bad body" }),
  })

  assert_eq(#_test.calls(), 0, "malformed event body is ignored without raising")
end
