-- tests/lua/mag-kernel/multi_run_test.lua — concurrent run contexts
-- (plugins/mag/lua/mag-kernel/init.lua, the full kernel entry). Driven from
-- engine/tests/starter_mag_kernel_test.rs, whose nefor stub queues
-- `nefor.emit` bodies into the global `__emitted` array.
--
-- The contract under test (docs/ir.md, Run contexts): each begin_run creates
-- an isolated context — inventory, routing, correlations — so
--   (a) two concurrent runs of the SAME program construct their actors
--       independently and a second run starting mid-first-run resets nothing;
--   (b) wire ids are run-scoped (`r<K>/cap-<n>` correlation ids,
--       `r<K>/<actor>@r<seq>` chat handles) and bus responses resolve to the
--       right run's context, in any order;
--   (c) every kernel→control-plane event carries run_id;
--   (d) kills scope to their run: killing one run's actor leaves the same id
--       alive in the other run, and drops only that run's correlations;
--   (e) run-context teardown after complete (end_run) reaps that run only;
--   (f) begin_run under a NEW session_id reaps stale contexts from the
--       previous session (kill handlers run — the llm's provider-cancel
--       reaches the bus with its run-scoped chat handle);
--   (g) a duplicate live run_id rejects.

local kernel = require("init")

-- ------------------------------------------------------------------
-- assert helpers
-- ------------------------------------------------------------------

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ", tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

local function starts_with(s, prefix)
  return type(s) == "string" and s:sub(1, #prefix) == prefix
end

-- ------------------------------------------------------------------
-- emitted-wire helpers (the harness stub queues nefor.emit into __emitted)
-- ------------------------------------------------------------------

local function drain_emitted()
  local out = {}
  for i, body in ipairs(__emitted) do
    out[i] = body
  end
  for i = #__emitted, 1, -1 do
    __emitted[i] = nil
  end
  return out
end

local function filter(bodies, pred)
  local out = {}
  for _, b in ipairs(bodies) do
    if pred(b) then out[#out + 1] = b end
  end
  return out
end

local function by_kind(bodies, kind)
  return filter(bodies, function(b) return b.kind == kind end)
end

-- ------------------------------------------------------------------
-- The program selects the llm's final-answer output structurally.
-- ------------------------------------------------------------------

local function program()
  return {
    actors = {
      {
        id = "agent",
        factory = "llm",
        params = { model = "m", provider = "prov", system = "answer" },
        evidence={version=2,identity="nefor.factory.llm",arguments={},input={kind="named",name="nefor.contracts.ProviderInput",arguments={}},output={kind="union",items={{kind="named",name="nefor.contracts.ToolCalls",arguments={}},{kind="named",name="nefor.contracts.FinalAnswer",arguments={}}}}},
        input={type={kind="named",name="nefor.contracts.ProviderInput",arguments={}},wire="generic-provider.ProviderOut"},outputs={{type={kind="named",name="nefor.contracts.ToolCalls",arguments={}},wire="generic-tool.ToolCalls"},{type={kind="named",name="nefor.contracts.FinalAnswer",arguments={}},wire="generic-provider.FinalAnswer"}},
        routes = {},
      },
    },
    messages = {
      { to = "agent", content = {
        kind = "generic-provider.ProviderOut",
        messages = { { role = "user", content = "hi" } },
      } },
    },
    kills = {},
    rules = {},
    result = { from = {
      actor = "agent",
      wire = "generic-provider.FinalAnswer",
    } },
  }
end

-- Begin + start one run; returns the emitted bodies of the start turn.
local function launch(run_id, session_id)
  local begun = kernel.begin_run({ run_id = run_id, run_name = run_id, session_id = session_id })
  assert_true(begun.ok, run_id .. " begins: " .. tostring(begun.error))
  local res = kernel.start(run_id, program())
  assert_true(res.ok, run_id .. " starts: " .. tostring(res.error))
  return drain_emitted()
end

-- The single provider-class tool.invoke in a batch of emitted bodies.
local function the_invoke(bodies, label)
  local invokes = by_kind(bodies, "tool.invoke")
  assert_eq(#invokes, 1, label .. ": exactly one provider invoke on the wire")
  return invokes[1]
end

-- ==================================================================
-- (a)+(b)+(c) two concurrent runs of the same program
-- ==================================================================

drain_emitted()

local a_wire = launch("run-A", "s1")
local a_invoke = the_invoke(a_wire, "run-A")
assert_true(starts_with(a_invoke.id, "r1/cap-"),
  "run-A correlation id is scope-prefixed; got " .. tostring(a_invoke.id))
assert_eq(a_invoke.args.chat_id, "r1/agent@r1",
  "run-A chat handle is scope-prefixed")
assert_eq(#by_kind(a_wire, "mag.run_started"), 1, "run-A run_started emitted")
assert_eq(by_kind(a_wire, "mag.run_started")[1].run_id, "run-A",
  "run_started carries run_id")
for _, e in ipairs(by_kind(a_wire, "mag.actor_spawned")) do
  assert_eq(e.run_id, "run-A", "actor_spawned carries run_id")
end
for _, e in ipairs(by_kind(a_wire, "mag.actor_ready")) do
  assert_eq(e.run_id, "run-A", "actor_ready carries run_id")
end
-- The activity pair rides the same run-stamped channel: the agent went busy
-- at its first activation (and stays busy while its provider invoke pends).
assert_true(#by_kind(a_wire, "mag.actor_busy") >= 1, "actor_busy emitted at activation")
for _, e in ipairs(by_kind(a_wire, "mag.actor_busy")) do
  assert_eq(e.run_id, "run-A", "actor_busy carries run_id")
end
for _, e in ipairs(by_kind(a_wire, "mag.actor_idle")) do
  assert_eq(e.run_id, "run-A", "actor_idle carries run_id")
end
assert_eq(#by_kind(a_wire, "mag.modification_applied"), 1,
  "run-A initial modification applied")
assert_eq(by_kind(a_wire, "mag.modification_applied")[1].run_id, "run-A",
  "modification_applied carries run_id")

-- Second run of the SAME program, mid-first-run.
local b_wire = launch("run-B", "s1")
local b_invoke = the_invoke(b_wire, "run-B")
assert_true(starts_with(b_invoke.id, "r2/cap-"),
  "run-B correlation id carries its own scope; got " .. tostring(b_invoke.id))
assert_eq(b_invoke.args.chat_id, "r2/agent@r1",
  "run-B chat handle carries its own scope (same actor id, same round)")

-- run-B starting did NOT reset run-A: its actors are still alive, fresh
-- spawn/ready events fired for run-B (no duplicate-alive degradation), and
-- run-A saw no kill.
assert_eq(kernel.state_of("run-A", "agent"), "alive", "run-A agent survives run-B start")
assert_eq(kernel.state_of("run-B", "agent"), "alive", "run-B agent constructs independently")
assert_eq(#by_kind(b_wire, "mag.actor_spawned"), 1, "run-B spawns its own constellation")
assert_eq(#by_kind(b_wire, "mag.actor_killed"), 0, "run-B start kills nothing")

-- ==================================================================
-- (b cont.) responses resolve to the right run, out of order: answer B first
-- ==================================================================

local owner = kernel.bus_response({ id = b_invoke.id, result = { text = "answer-B" } })
assert_eq(owner, "run-B", "run-B's correlation resolves to run-B's context")
local b_done = drain_emitted()
local b_complete = by_kind(b_done, "mag.run_complete")
assert_eq(#b_complete, 1, "run-B completes")
assert_eq(b_complete[1].run_id, "run-B", "run_complete carries run_id")

local rc_b = kernel.take_run_complete("run-B")
assert_true(rc_b ~= nil, "run-B terminal capture is set")
assert_eq(rc_b.result.text, "answer-B", "run-B's result is run-B's answer")
assert_true(kernel.take_run_complete("run-A") == nil,
  "run-A is still in flight — B's completion is not A's")

-- ==================================================================
-- (e) teardown after complete: end_run reaps run-B only
-- ==================================================================

assert_true(kernel.end_run("run-B", "run_complete"), "run-B context existed")
local b_teardown = drain_emitted()
local b_killed = {}
for _, e in ipairs(by_kind(b_teardown, "mag.actor_killed")) do
  assert_eq(e.run_id, "run-B", "teardown kills are stamped with run-B")
  assert_eq(e.reason, "run_complete",
    "post-complete teardown kills carry reason run_complete")
  b_killed[e.id] = true
end
assert_true(b_killed["agent"], "run-B's leftovers are reaped")
assert_true(kernel.context("run-B") == nil, "run-B context dropped")
assert_eq(kernel.state_of("run-A", "agent"), "alive", "run-A untouched by run-B teardown")

-- ==================================================================
-- (d) kills scope to their run
-- ==================================================================

local c_wire = launch("run-C", "s1")
local c_invoke = the_invoke(c_wire, "run-C")

-- Kill run-A's agent (mid-flight: it holds an open provider request).
local applied = kernel.apply("run-A", { kills = { "agent" } })
assert_true(applied.ok, "kill applies to run-A")
local a_kill_wire = drain_emitted()
local a_killed = by_kind(a_kill_wire, "mag.actor_killed")
assert_eq(#a_killed, 1, "one actor killed")
assert_eq(a_killed[1].run_id, "run-A", "the kill event is run-A's")
assert_eq(a_killed[1].reason, "modification",
  "a kill entry in an applied modification carries reason modification")
-- The dying llm's provider-cancel reached the bus with run-A's scoped handle.
local cancels = filter(a_kill_wire, function(b) return b.kind == "prov.chat.cancel" end)
assert_eq(#cancels, 1, "the dying llm aborts its provider request")
assert_eq(cancels[1].chat_id, "r1/agent@r1", "the cancel names run-A's scoped chat")

-- Same id in run-C: untouched.
assert_eq(kernel.state_of("run-C", "agent"), "alive", "run-C's agent survives run-A's kill")
assert_eq(kernel.state_of("run-A", "agent"), "dead", "run-A's agent is dead")

-- run-A's correlation dropped with the kill: its late reply resolves nowhere.
assert_true(kernel.bus_response({ id = a_invoke.id, result = { text = "too-late" } }) == nil,
  "a killed actor's late reply is voided")
-- run-C's correlation still resolves.
assert_eq(kernel.bus_response({ id = c_invoke.id, result = { text = "answer-C" } }), "run-C",
  "run-C's correlation still resolves after run-A's kill")
assert_true(kernel.take_run_complete("run-C") ~= nil, "run-C completes")
drain_emitted()
kernel.end_run("run-C", "run_complete")
-- An end_run with no reason is an outright kill (the mag.kill_run path).
-- run-A's only actor was already killed by the modification, so teardown must
-- not emit a duplicate actor_killed event for it.
kernel.end_run("run-A")
local kill_teardown = drain_emitted()
local reasons = {}
for _, e in ipairs(by_kind(kill_teardown, "mag.actor_killed")) do
  reasons[e.run_id] = e.reason
end
assert_eq(reasons["run-C"], "run_complete", "run-C teardown carries its reason")
assert_eq(reasons["run-A"], nil, "kill_run does not re-kill an actor already removed")

-- ==================================================================
-- (f) session-boundary reaping: a new session's begin_run reaps stale
-- contexts from the previous session, and only those
-- ==================================================================

launch("run-E", "s1") -- never terminates: its llm awaits a provider reply
local begun = kernel.begin_run({ run_id = "run-F", run_name = "run-F", session_id = "s2" })
assert_true(begun.ok, "run-F begins under the new session")
assert_eq(#begun.reaped, 1, "one stale run reaped at the session boundary")
assert_eq(begun.reaped[1], "run-E", "the previous session's run is the one reaped")
assert_true(kernel.context("run-E") == nil, "run-E context dropped")
assert_true(kernel.context("run-F") ~= nil, "run-F context live")
local reap_wire = drain_emitted()
-- The reap ran kill handlers: run-E's in-flight llm cancelled its provider
-- request, scoped to run-E's context.
local reap_cancels = filter(reap_wire, function(b) return b.kind == "prov.chat.cancel" end)
assert_eq(#reap_cancels, 1, "the reaped run's llm aborts its provider request")
assert_eq(reap_cancels[1].chat_id, "r4/agent@r1", "the cancel names run-E's scoped chat")
for _, e in ipairs(by_kind(reap_wire, "mag.actor_killed")) do
  assert_eq(e.run_id, "run-E", "reap kills are stamped with the stale run's id")
  assert_eq(e.reason, "reaped", "session-boundary reap kills carry reason reaped")
end

-- A same-session sibling is NOT reaped: run-G under s2 leaves run-F alone.
local begun_g = kernel.begin_run({ run_id = "run-G", run_name = "run-G", session_id = "s2" })
assert_true(begun_g.ok, "run-G begins")
assert_eq(#begun_g.reaped, 0, "a same-session begin reaps nothing")
assert_true(kernel.context("run-F") ~= nil, "run-F survives its sibling's begin")

-- ==================================================================
-- (g) duplicate live run_id rejects
-- ==================================================================

local dup = kernel.begin_run({ run_id = "run-F", run_name = "run-F", session_id = "s2" })
assert_true(not dup.ok, "a duplicate live run_id rejects")
assert_true(tostring(dup.error):find("already live", 1, true) ~= nil,
  "the rejection names the collision; got " .. tostring(dup.error))

kernel.end_run("run-F")
kernel.end_run("run-G")
drain_emitted()

print("mag-kernel multi_run_test: all cases passed")
