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

-- Current authoring dialect: agents are the compiler's `agent` template,
-- composed with a sink via (graph … :terminal out). The lead's validators
-- never parse this source — compilation happens in the mag plugin and the
-- validators run over the modification in the mag.loaded reply — so these
-- strings only document what the lead writes to disk.
local READ_ONLY_MAG = [[
(type mag.Task)
(type generic-provider.FinalAnswer)

(let [worker (agent {:id "worker"
                     :system "Answer the task."
                     :provider "chatgpt"
                     :profile "standard"
                     :tools ["read_file"]}
               : mag.Task -> generic-provider.FinalAnswer)
      out    (node "sink" {} : generic-provider.FinalAnswer -> generic-provider.FinalAnswer)]
  (graph worker -> out :terminal out))
]]

local WRITER_MAG = [[
(type mag.Task)
(type generic-provider.FinalAnswer)

(let [build (agent {:id "build"
                    :system "Implement feature X."
                    :provider "chatgpt"
                    :profile "fast"
                    :tools ["read_file" "write_file"]}
              : mag.Task -> generic-provider.FinalAnswer)
      out   (node "sink" {} : generic-provider.FinalAnswer -> generic-provider.FinalAnswer)]
  (graph build -> out :terminal out))
]]

-- Modification shapes the mag plugin replies with on mag.loaded — the
-- ModificationIr {actors, messages, kills, rules} the loader lowers to.
-- The agent template namespaces its internals under :id (worker.llm etc.);
-- these are trimmed to the actors the validators care about.
local KERNEL_FACTORIES = { "adapter", "llm", "loop-counter", "run-tool", "sink", "stub", "tool-result" }

local function read_only_modification()
  return {
    actors = {
      { id = "worker.entry", factory = "adapter",
        params = { seed = "provider-in" },
        routes = { ["generic-provider.ProviderOut"] = { "worker.llm" } } },
      { id = "worker.llm", factory = "llm",
        params = { system = "Answer the task.", provider = "chatgpt",
                   profile = "standard", tools = { "read_file" } },
        routes = { ["generic-provider.FinalAnswer"] = { "sink" } } },
      { id = "sink", factory = "sink", params = {}, routes = {} },
    },
    messages = { { to = "worker.entry", content = { kind = "task", prompt = "<initial task text>" } } },
    kills = {},
    rules = {},
  }
end

local function writer_modification()
  return {
    actors = {
      { id = "build.llm", factory = "llm",
        params = { system = "Implement feature X.", provider = "chatgpt",
                   profile = "fast", tools = { "read_file", "write_file" } },
        routes = { ["generic-provider.FinalAnswer"] = { "sink" } } },
      { id = "sink", factory = "sink", params = {}, routes = {} },
    },
    messages = { { to = "build.llm", content = { kind = "task", prompt = "<initial task text>" } } },
    kills = {},
    rules = {},
  }
end

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

-- Drive the load handshake: find the emitted mag.load, feed the mag.loaded
-- reply carrying `modification`, return the load call. `factories` defaults
-- to the full kernel registry.
local function feed_loaded(modification, factories)
  local load = find_call(decode_calls(), function(c)
    return c.body.kind == "mag.load" and c.target == "mag"
  end)
  assert_true(load ~= nil,
    "mag compile/execute must emit mag.load to the mag plugin; got "
    .. json.encode(_test.calls()))
  feed("mag", {
    kind        = "mag.loaded",
    in_reply_to = load.body.id,
    hash        = "sha256:test",
    factories   = factories or KERNEL_FACTORIES,
    modification = modification,
  })
  return load
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
-- mag execute: the load handshake — mag.load first, mag.execute only
-- after the mag.loaded reply validates the modification.
-- ------------------------------------------------------------------

do
  fresh()
  write_mag_file("firing-mag-write-1", "auth-login-map.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-mag-execute-1", "auth-login-map.mag")

  -- Synchronous handshake: mag.load is sent first; mag.execute is withheld
  -- until the mag.loaded reply carries the modification.
  local calls = decode_calls()
  local load = find_call(calls, function(c)
    return c.body.kind == "mag.load" and c.target == "mag"
  end)
  assert_true(load ~= nil,
    "mag execute emits mag.load to the mag plugin; got " .. json.encode(_test.calls()))
  assert_eq(load.body.entry, "auth-login-map.mag", "mag.load names the .mag entry file")
  assert_true(type(load.body.source_dir) == "string" and #load.body.source_dir > 0,
    "mag.load carries the workspace source_dir")

  local premature = find_call(calls, function(c) return c.body.kind == "mag.execute" end)
  assert_eq(premature, nil,
    "mag.execute must NOT be sent before mag.loaded (synchronous handshake)")
  local pre_reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-mag-execute-1"
  end)
  assert_eq(pre_reply, nil, "no executing reply until the load handshake resolves")

  _test.calls_clear()
  feed("mag", {
    kind        = "mag.loaded",
    in_reply_to = load.body.id,
    hash        = "sha256:read-only",
    factories   = KERNEL_FACTORIES,
    modification = read_only_modification(),
  })
  calls = decode_calls()

  local exec = find_call(calls, function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
  end)
  assert_true(exec ~= nil,
    "mag.loaded releases mag.execute; got " .. json.encode(_test.calls()))
  assert_true(type(exec.body.session_id) == "string" and #exec.body.session_id > 0,
    "lead injects session_id on mag.execute")

  -- Profiles land via the params overlay, keyed by the ACTOR id that
  -- authors params.profile — the agent template's namespaced llm actor.
  local overlay = exec.body.params_overlay
  assert_true(type(overlay) == "table" and type(overlay["worker.llm"]) == "table",
    "params_overlay keys on the profiled llm actor id; got " .. json.encode(_test.calls()))
  assert_eq(overlay["worker.llm"].reasoning_effort, "medium",
    "profile 'standard' resolves to reasoning_effort=medium")
  assert_true(type(overlay["worker.llm"].provider) == "string",
    "profile resolution threads the provider")

  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-mag-execute-1"
  end)
  assert_true(reply ~= nil and reply.body.output ~= nil, "execute replies executing")
  assert_eq(reply.body.output.status, "executing", "reply reports the program is executing")
  assert_eq(reply.body.output.engine, "mag-kernel", "reply reports the kernel engine")
  assert_eq(reply.body.output.hash, "sha256:read-only", "reply carries the program hash")
  assert_eq(exec.body.run_id, reply.body.output.run_id,
    "mag.execute run_id matches the reply run_id")

  -- Active run tracked from the modification's actors: factory under the
  -- summary's reasoner key (what the chat surface renders).
  local run = lw._internals.state.active_runs[reply.body.output.run_id]
  assert_true(type(run) == "table", "active_runs contains the dispatched run_id")
  assert_eq(run.terminal, "sink", "the canonical sink actor is the terminal")
  _test.calls_clear()
  invoke_tool("firing-graph-status-actors", "graph-status", { run_id = reply.body.output.run_id })
  local status = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-graph-status-actors"
  end)
  assert_true(status ~= nil, "graph-status returns the active run")
  local nodes = status.body.output.run.nodes
  assert_eq(nodes[1].id, "worker.entry", "actor ids preserved in run summaries")
  assert_eq(nodes[2].reasoner, "llm", "actor factory carried under the reasoner key")
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

-- ------------------------------------------------------------------
-- mag compile: mag.load through the plugin, preview rendered from the
-- mag.loaded modification. Compile never executes.
-- ------------------------------------------------------------------

do
  fresh()
  write_mag_file("firing-mag-write-compile", "deterministic-check.mag", READ_ONLY_MAG)
  _test.calls_clear()
  invoke_tool("firing-mag-compile", "mag", {
    action = "compile",
    file = "deterministic-check.mag",
  })
  feed_loaded(read_only_modification())

  local calls = decode_calls()
  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-mag-compile"
  end)
  assert_true(reply ~= nil, "mag compile returns a tool.result")
  assert_eq(reply.body.output.status, "compiled", "mag compile reports compiled status")
  assert_eq(reply.body.output.hash, "sha256:test", "mag compile reports the program hash")
  local preview = reply.body.output.preview
  assert_true(type(preview) == "string", "mag compile returns a preview string")
  for _, needle in ipairs({
    "worker.llm (llm)",                                    -- actor id + factory
    "provider: \"chatgpt\"",                               -- params summary
    "routes: generic-provider.FinalAnswer -> sink",        -- typed routes
    "-> worker.entry (task)",                              -- initial message
    "Hash: sha256:test",                                   -- hash
    "Registry factories: adapter, llm",                    -- kernel registry
  }) do
    assert_true(preview:find(needle, 1, true) ~= nil,
      "compile preview includes '" .. needle .. "'; got:\n" .. preview)
  end
  local leaked = find_call(calls, function(c)
    return c.body.kind == "mag.execute"
  end)
  assert_eq(leaked, nil, "mag compile previews only and does not send mag.execute")
end

-- A mag.error reply (compile failure) fails the firing with the compiler
-- message instead of leaving it hanging.
do
  fresh()
  write_mag_file("firing-mag-write-badsrc", "broken.mag", "(graph nope)")
  _test.calls_clear()
  invoke_tool("firing-mag-compile-fail", "mag", {
    action = "compile",
    file = "broken.mag",
  })
  local load = find_call(decode_calls(), function(c)
    return c.body.kind == "mag.load" and c.target == "mag"
  end)
  assert_true(load ~= nil, "compile emits mag.load")
  _test.calls_clear()
  feed("mag", {
    kind        = "mag.error",
    in_reply_to = load.body.id,
    message     = "graph requires a :terminal binding",
  })
  local err = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result"
       and c.body.id == "firing-mag-compile-fail"
       and type(c.body.error) == "string"
  end)
  assert_true(err ~= nil, "mag.error resolves the pending compile as a tool error")
  assert_true(err.body.error:find("compilation failed", 1, true) ~= nil
              and err.body.error:find("terminal binding", 1, true) ~= nil,
    "compile failure carries the compiler message; got " .. json.encode(_test.calls()))
end

-- ------------------------------------------------------------------
-- Run close: terminal mag.run_result closes the run, relays a fresh
-- model turn, and appends the visible run-result block.
-- ------------------------------------------------------------------

-- The kernel lead: agentic-loop relays a dispatched run's completion as
-- a fresh turn-program spawn. On first use the spawner loads its shipped
-- turn-program (a mag.load with a distinct entry from the lead tool's
-- workspace loads) — answer it with a minimal compiled lead shape — and
-- the relayed text rides the mag.execute's initial task payload.
local function lead_turn_modification()
  return {
    actors = {
      { id = "lead.entry", factory = "adapter", params = { seed = "provider-in" },
        routes = { ["generic-provider.ProviderOut"] = { "lead.llm" } } },
      { id = "lead.llm", factory = "llm", params = {},
        routes = { ["generic-provider.FinalAnswer"] = { "sink" } } },
      { id = "sink", factory = "sink", params = {}, routes = {} },
    },
    messages = { { to = "lead.entry", content = { kind = "task", prompt = "<initial task text>" } } },
    kills = {},
    rules = {},
  }
end

local function relayed_lead_prompt()
  local calls = decode_calls()
  local load = find_call(calls, function(c)
    return c.body.kind == "mag.load"
       and c.body.entry == "agentic-loop/lead-turn.mag"
  end)
  if load ~= nil then
    agentic_loop.receive_msg(make_entry("mag", {
      kind = "mag.loaded", in_reply_to = load.body.id,
      hash = "sha256:lead", modification = lead_turn_modification(),
    }))
    calls = decode_calls()
  end
  local exec = find_call(calls, function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
       and c.body.run_name == "lead"
  end)
  if exec == nil then return nil end
  local msg = exec.body.modification and exec.body.modification.messages
    and exec.body.modification.messages[1]
  return msg and msg.content and msg.content.prompt or nil
end

do
  fresh()
  write_mag_file("firing-kernel-write", "kernel-run.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-kernel-exec", "kernel-run.mag")
  feed_loaded(read_only_modification())
  local reply = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-kernel-exec"
  end)
  assert_true(reply ~= nil and reply.body.output ~= nil, "execute replies executing")

  -- Terminal mag.run_result closes the run AND relays a fresh model turn
  -- carrying the sink output content read from the path.
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

  -- The completion is relayed as a fresh lead turn (agentic-loop's kernel
  -- turn spawner — a lead turn-program mag.execute re-prompt).
  local prompt = relayed_lead_prompt()
  assert_true(type(prompt) == "string",
    "mag.run_result relays a fresh lead turn; got " .. json.encode(_test.calls()))
  assert_true(prompt:find("SINK OUTPUT CONTENT", 1, true) ~= nil,
    "the relayed turn carries the sink output content read from the path")
  assert_true(prompt:find(out_path, 1, true) ~= nil,
    "the relayed turn carries the sink output path")

  -- The visible run-result block is appended to the chat surface. It carries
  -- status + run id + the sink output PATH, but NOT the output content (that
  -- rides the relayed turn above; no double-render).
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
end

-- The kernel's lifecycle stream drives node statuses (every kernel event
-- carries its run_id — runs are concurrent, the tracker keys by it), and a
-- mag.run_result carrying the sink's result INLINE relays its text without
-- any file read.
do
  fresh()
  write_mag_file("firing-kernel-inline-write", "kernel-inline.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-kernel-inline", "kernel-inline.mag")
  feed_loaded(read_only_modification())
  local reply = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-kernel-inline"
  end)
  assert_true(reply ~= nil and reply.body.output ~= nil, "execute replies executing")
  local run_id = reply.body.output.run_id

  -- Kernel lifecycle: every event carries its run_id (concurrent runs track
  -- independently). worker.entry is killed mid-run; the rest complete.
  feed("mag", { kind = "mag.run_started", run_id = run_id, run_name = "kernel-inline" })
  for _, actor in ipairs({
    { id = "worker.entry", factory = "adapter" },
    { id = "worker.llm",   factory = "llm" },
    { id = "sink",         factory = "sink" },
  }) do
    feed("mag", { kind = "mag.actor_spawned", run_id = run_id, id = actor.id, factory = actor.factory })
    feed("mag", { kind = "mag.actor_ready",   run_id = run_id, id = actor.id })
  end
  feed("mag", { kind = "mag.actor_killed", run_id = run_id, id = "worker.entry" })
  -- An event for a DIFFERENT run must not leak into this one's node table.
  feed("mag", { kind = "mag.actor_killed", run_id = "some-other-run", id = "worker.llm" })
  feed("mag", {
    kind = "mag.run_complete", run_id = run_id, from = "sink",
    result = { text = "INLINE RESULT TEXT" }, persisted = false,
  })

  _test.calls_clear()
  feed("mag", {
    kind      = "mag.run_result",
    run_id    = run_id,
    status    = "completed",
    persisted = false,
    result    = { from = "worker.llm", kind = "generic-provider.FinalAnswer",
                  text = "INLINE RESULT TEXT" },
  })

  -- The relayed fresh turn carries the inline result text — no output file
  -- exists anywhere in this scenario.
  local prompt = relayed_lead_prompt()
  assert_true(type(prompt) == "string",
    "inline mag.run_result relays a fresh lead turn; got " .. json.encode(_test.calls()))
  assert_true(prompt:find("INLINE RESULT TEXT", 1, true) ~= nil,
    "the relayed turn carries the inline result text; got " .. tostring(prompt))

  -- The result block carries the actors' final statuses, not dispatch-time
  -- pending: killed stays killed, everything else is done.
  local block = find_call(decode_calls(), function(c)
    return c.body.kind == "chat.graph_result.append" and c.target == "nefor-tui"
  end)
  assert_true(block ~= nil, "run-result block appended")
  local statuses = {}
  for _, n in ipairs(block.body.nodes) do statuses[n.id] = n.status end
  assert_eq(statuses["worker.entry"], "killed", "killed actor keeps its terminal state")
  assert_eq(statuses["worker.llm"], "done", "completed actor is done, not pending")
  assert_eq(statuses["sink"], "done", "sink is done, not pending")
end

-- A failed run appends a failed run-result block carrying the error.
do
  fresh()
  write_mag_file("firing-kernel-fail-write", "kernel-fail.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-kernel-fail", "kernel-fail.mag")
  feed_loaded(read_only_modification())
  local reply = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-kernel-fail"
  end)
  assert_true(reply ~= nil and reply.body.output ~= nil, "execute replies executing")

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
end

-- ------------------------------------------------------------------
-- Execute validators over the modification's actors.
-- ------------------------------------------------------------------

-- Unknown factory against the load-reply registry blocks mag.execute.
do
  fresh()
  write_mag_file("firing-kernel-badfactory-write", "kernel-bad.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-kernel-badfactory", "kernel-bad.mag")
  -- Registry omits adapter/llm → validation must reject before execute.
  feed_loaded(read_only_modification(), { "sink", "stub" })

  local calls = decode_calls()
  local exec = find_call(calls, function(c) return c.body.kind == "mag.execute" end)
  assert_eq(exec, nil, "unknown factory blocks mag.execute")
  local err = find_call(calls, function(c)
    return c.body.kind == "tool.result"
       and c.body.id == "firing-kernel-badfactory"
       and type(c.body.error) == "string"
  end)
  assert_true(err ~= nil and err.body.error:find("unknown factory", 1, true) ~= nil,
    "validation rejects the unknown factory with a clear error; got " .. json.encode(_test.calls()))
  assert_true(err.body.error:find("worker.entry", 1, true) ~= nil
              and err.body.error:find("adapter", 1, true) ~= nil,
    "rejection names the offending actor and factory")
end

-- Sink validators: missing sink, and sink without inbound routes. The
-- missing-sink error teaches the current dialect (agent + :terminal sink).
do
  fresh()
  write_mag_file("firing-sink-missing-write", "no-sink.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-sink-missing", "no-sink.mag")
  local m = read_only_modification()
  table.remove(m.actors, 3) -- drop the sink actor
  m.actors[2].routes = {}
  feed_loaded(m)
  local calls = decode_calls()
  assert_eq(find_call(calls, function(c) return c.body.kind == "mag.execute" end), nil,
    "missing sink blocks mag.execute")
  local err = find_call(calls, function(c)
    return c.body.kind == "tool.result"
       and c.body.id == "firing-sink-missing"
       and type(c.body.error) == "string"
  end)
  assert_true(err ~= nil and err.body.error:find("no sink actor", 1, true) ~= nil,
    "missing sink is rejected; got " .. json.encode(_test.calls()))
  assert_true(err.body.error:find("(agent {", 1, true) ~= nil
              and err.body.error:find(":terminal out", 1, true) ~= nil,
    "the sink error teaches the current (agent …) + :terminal dialect; got "
    .. tostring(err.body.error))
end

do
  fresh()
  write_mag_file("firing-sink-orphan-write", "orphan-sink.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-sink-orphan", "orphan-sink.mag")
  local m = read_only_modification()
  m.actors[2].routes = {} -- nothing routes to the sink any more
  feed_loaded(m)
  local err = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result"
       and c.body.id == "firing-sink-orphan"
       and type(c.body.error) == "string"
  end)
  assert_true(err ~= nil and err.body.error:find("no inbound routes", 1, true) ~= nil,
    "a sink nothing routes to is rejected; got " .. json.encode(_test.calls()))
end

-- Profile validators: an llm actor must author :profile (or raw
-- reasoning_effort); both at once and unknown names are rejected.
do
  fresh()
  write_mag_file("firing-profile-missing-write", "no-profile.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-profile-missing", "no-profile.mag")
  local m = read_only_modification()
  m.actors[2].params.profile = nil
  feed_loaded(m)
  local err = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result"
       and c.body.id == "firing-profile-missing"
       and type(c.body.error) == "string"
  end)
  assert_true(err ~= nil and err.body.error:find("missing required :profile", 1, true) ~= nil,
    "an llm actor without profile/reasoning_effort is rejected; got "
    .. json.encode(_test.calls()))
  assert_true(err.body.error:find("worker.llm", 1, true) ~= nil,
    "the profile error names the llm actor")
end

do
  fresh()
  write_mag_file("firing-profile-both-write", "both-profile.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-profile-both", "both-profile.mag")
  local m = read_only_modification()
  m.actors[2].params.reasoning_effort = "high"
  feed_loaded(m)
  local err = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result"
       and c.body.id == "firing-profile-both"
       and type(c.body.error) == "string"
  end)
  assert_true(err ~= nil
              and err.body.error:find("both profile and reasoning_effort", 1, true) ~= nil,
    "profile + raw reasoning_effort together are rejected; got "
    .. json.encode(_test.calls()))
end

do
  fresh()
  write_mag_file("firing-profile-unknown-write", "bad-profile.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-profile-unknown", "bad-profile.mag")
  local m = read_only_modification()
  m.actors[2].params.profile = "turbo"
  feed_loaded(m)
  local err = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result"
       and c.body.id == "firing-profile-unknown"
       and type(c.body.error) == "string"
  end)
  assert_true(err ~= nil and err.body.error:find("unknown profile 'turbo'", 1, true) ~= nil,
    "unknown profile names are rejected; got " .. json.encode(_test.calls()))
end

-- mag.loaded snapshots the kernel factory registry for factory validation.
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
  feed_loaded(writer_modification())
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
  -- No mag.execute should leak through.
  local leaked = find_call(calls, function(c)
    return c.body.kind == "mag.execute"
  end)
  assert_true(leaked == nil,
    "gate rejection must NOT send mag.execute to the kernel")
end

-- After /approve, the same writer program is accepted; the profile overlay
-- keys on the writer's namespaced llm actor.
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
  feed_loaded(writer_modification())
  local calls = decode_calls()
  local exec = find_call(calls, function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
  end)
  assert_true(exec ~= nil,
    "after plan approval, write-capable MAG execute must send mag.execute; got "
    .. json.encode(_test.calls()))
  local overlay = exec.body.params_overlay
  assert_true(type(overlay) == "table" and type(overlay["build.llm"]) == "table",
    "writer profile overlay keys on the namespaced llm actor")
  assert_eq(overlay["build.llm"].reasoning_effort, "low",
    "profile 'fast' resolves to reasoning_effort=low")
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
  feed_loaded(writer_modification())
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
  feed_loaded(writer_modification())
  local calls = decode_calls()
  local exec = find_call(calls, function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
  end)
  assert_true(exec ~= nil, "auto bypasses the human plan gate for writer MAG execute")
end

do
  fresh()
  feed("tool-gate", { kind = "tool-gate.mode_changed", mode = "yolo" })
  write_mag_file("firing-writer-write-yolo", "yolo-build.mag", WRITER_MAG)
  _test.calls_clear()
  execute_mag("firing-writer-yolo", "yolo-build.mag")
  feed_loaded(writer_modification())
  local calls = decode_calls()
  local exec = find_call(calls, function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
  end)
  assert_true(exec ~= nil, "yolo bypasses writer MAG execute approval gate")
end

-- ------------------------------------------------------------------
-- session_end terminates active graph AND flushes the plan slot
-- ------------------------------------------------------------------

do
  fresh()
  write_mag_file("firing-mag-write-end", "session-end.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-mag-execute-end", "session-end.mag")
  feed_loaded(read_only_modification())
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
  local kill = find_call(calls, function(c)
    return c.body.kind == "mag.kill_run" and c.body.run_id == run_id
       and c.target == "mag"
  end)
  assert_true(kill ~= nil,
    "session_end emits mag.kill_run for the active kernel run")

  assert_eq(next(lw._internals.state.active_runs), nil,
    "active_runs cleared after termination")
  assert_eq(lw._internals.state.active_plan, nil,
    "active_plan flushed at session_end — no carry-over approval")
end
