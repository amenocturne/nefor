-- starter/lead_workflow_test.lua — unit tests for the lead-workflow
-- actor. Driven from
-- `crates/nefor/tests/starter_lead_workflow_test.rs`. Mirrors the
-- harness pattern in `starter_agentic_workflow_test.rs`.

local lw   = require("libs.lead-workflow")
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
  _test.set_plugins({ "mag", "tool-gate", "nefor-tui" })
  _test.calls_clear()
end

-- The advertised MAG tool points at the injected canonical contract, not at
-- private library source files that the lead cannot read from its workspace.
do
  fresh()
  feed("tool-gate", { kind = "tool-gate.hello" })
  local advertised = find_call(decode_calls(), function(call)
    return call.body.kind == "tool-gate.tools.advertise"
  end)
  assert_true(advertised ~= nil, "lead workflow advertises its tool schemas")
  local mag_schema, mag_eval_schema, await_schema
  for _, schema in ipairs(advertised.body.tools or {}) do
    assert_true(type(schema.display) == "table", schema.name .. " has display metadata")
    if schema.name == "mag" then mag_schema = schema end
    if schema.name == "mag-eval" then mag_eval_schema = schema end
    if schema.name == "await-run" then await_schema = schema end
  end
  assert_true(mag_schema ~= nil, "the MAG tool schema is advertised")
  assert_true(mag_eval_schema ~= nil, "the mag-eval tool schema is advertised")
  assert_true(await_schema ~= nil, "the await-run schema is advertised")
  assert_eq(await_schema.display.label, "Await run", "await-run has semantic display metadata")
  assert_eq(await_schema.display.primary.arg, "run_id", "await-run displays its stable handle")
  assert_eq(mag_eval_schema.display.label, "mag-eval", "mag-eval display keeps stable tool identity")
  assert_eq(mag_eval_schema.display.primary.arg, "intent", "mag-eval display uses exact intent")
  assert_true(string.find(mag_schema.description, "lib/patterns.md", 1, true) ~= nil,
    "the MAG schema points to the injected canonical patterns")
  assert_true(string.find(mag_schema.description, "lib/nefor/*.mag", 1, true) == nil,
    "the MAG schema does not point at unreadable library implementation files")
  assert_true(string.find(mag_schema.description, "(require \"...\")", 1, true) ~= nil,
    "the MAG schema reinforces literal require syntax")
  assert_true(string.find(mag_schema.description, "When to dispatch a graph", 1, true) == nil,
    "the MAG schema leaves allocation policy to the system prompt")
  assert_true(string.find(mag_schema.description, "Anything multi-file", 1, true) == nil,
    "the MAG schema does not encode task-shape routing heuristics")
  assert_true(string.find(mag_schema.description, "redo delegated work", 1, true) == nil,
    "the MAG schema does not encode agent ownership policy")
end

local starter_profiles = require("config").active.orchestration_profiles

local function with_profiles(profiles, fn)
  local config = require("config")
  config.active.orchestration_profiles = profiles
  local ok, err = pcall(fn)
  config.active.orchestration_profiles = starter_profiles
  if not ok then error(err, 0) end
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
local KERNEL_FACTORIES = { "adapter", "llm", "run-tool", "sink", "stub", "tool-result" }

local function foreign_contracts(factories)
  local contracts = {}
  for _, name in ipairs(factories or KERNEL_FACTORIES) do
    contracts[#contracts + 1] = { identity = "nefor.factory." .. name }
  end
  return contracts
end

-- Test fixtures stay compact by describing the former sink-shaped graph,
-- then this helper expresses the same program through the current artifact
-- boundary: qualified foreign actors plus a structural result selector.
local function artifact_from_modification(modification)
  local actors, sink_ids = {}, {}
  for _, actor in ipairs(modification.actors or {}) do
    if actor.factory == "sink" then
      sink_ids[actor.id] = true
    else
      local routes = {}
      for wire, destinations in pairs(actor.routes or {}) do
        local kept = {}
        for _, destination in ipairs(destinations) do
          local destination_id = destination.actor
          if not sink_ids[destination_id] and destination_id ~= "sink" then
            kept[#kept + 1] = destination
          end
        end
        if #kept > 0 then routes[wire] = kept end
      end
      actors[#actors + 1] = {
        id = actor.id,
        foreign = "nefor.factory." .. tostring(actor.factory),
        params = actor.params or {},
        routes = routes,
      }
    end
  end
  local result
  for _, actor in ipairs(modification.actors or {}) do
    if actor.factory ~= "sink" then
      for wire, destinations in pairs(actor.routes or {}) do
        for _, destination in ipairs(destinations) do
          local destination_id = destination.actor
          if destination_id == "sink" or sink_ids[destination_id] then
            result = { from = { actor = actor.id, type = wire, wire = wire } }
          end
        end
      end
    end
  end
  return {
    format = "nefor.graph-modification/v1",
    data = {
      actors = actors,
      messages = modification.messages or {},
      kills = modification.kills or {},
      rules = modification.rules or {},
      result = result,
    },
  }
end

local function read_only_modification()
  return {
    actors = {
      { id = "worker.entry", factory = "adapter",
        params = { seed = "provider-in" },
        routes = { ["generic-provider.ProviderOut"] = { { actor = "worker.llm", wire = "generic-provider.ProviderOut" } } } },
      { id = "worker.llm", factory = "llm",
        params = { system = "Answer the task.", provider = "chatgpt",
                   profile = "standard", tools = { "read_file" } },
        routes = { ["generic-provider.FinalAnswer"] = { { actor = "sink", wire = "generic-provider.FinalAnswer" } } } },
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
        routes = { ["generic-provider.FinalAnswer"] = { { actor = "sink", wire = "generic-provider.FinalAnswer" } } } },
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

local function invoke_tool_with_metadata(id, name, args, metadata)
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id = id,
    caller_id = metadata and metadata.caller_id,
    invocation = metadata and metadata.invocation,
    name = name,
    args = args or {},
  })
end

local function invocation(session_id, principal, capability_id)
  capability_id = capability_id or "r-provenance/cap-1"
  return {
    session_id = session_id,
    run_id = "run-provenance",
    run_scope = capability_id:match("^([^/]+)/") or "r-provenance",
    actor_id = principal == "lead" and "lead.run-tool" or "worker.run-tool",
    capability_id = capability_id,
    principal = principal,
  }
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

-- Drive the load handshake and return the load call.
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
    foreign_contracts = foreign_contracts(factories),
    artifact = artifact_from_modification(modification),
  })
  return load
end

-- The composition supplies the shared base agent behind the ready MAG
-- constructor. MAG programs add a positional system overlay and send the full
-- task as a user message; they do not select models.
do
  fresh()
  lw.configure({ agent_defaults = {
    provider = "chatgpt",
    model = "general-model",
    reasoning_effort = "medium",
    system = "universal composed prompt",
  } })
  write_mag_file("system-overlay-write", "system-overlay.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("system-overlay-execute", "system-overlay.mag")
  local modification = read_only_modification()
  modification.actors[2].foreign = "nefor.factory.structured-output"
  modification.actors[2].params.profile = nil
  feed_loaded(modification)
  local exec = find_call(decode_calls(), function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
  end)
  assert_true(exec ~= nil, "configured universal system permits execution")
  local patch = exec.body.params_overlay["worker.llm"]
  assert_eq(patch.provider, "chatgpt", "ready agent receives default provider")
  assert_eq(patch.model, "general-model",
    "structured-output ready agent receives default model")
  assert_eq(patch.reasoning_effort, "medium", "ready agent receives default effort")
  assert_eq(patch.system,
    "universal composed prompt\n\n---\n\nAnswer the task.",
    "the runtime composes the shared base with the delegated position")
end

-- ------------------------------------------------------------------
-- dependency module roots — shared by mag and mag-eval
-- ------------------------------------------------------------------

local function latest_mag_load()
  local loads = find_calls(decode_calls(), function(c)
    return c.body.kind == "mag.load" and c.target == "mag"
  end)
  return loads[#loads]
end

local function assert_config_rejected(value, label)
  local ok = pcall(function()
    lw.configure({ dependency_module_roots = value })
  end)
  assert_eq(ok, false, label .. " must be rejected")
end

do
  fresh()
  write_mag_file("roots-default-write", "roots-default.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("roots-default-execute", "roots-default.mag")
  local default_load = latest_mag_load()
  assert_true(default_load ~= nil, "default mag execution emits mag.load")
  assert_eq(#default_load.body.module_roots, 1,
    "omitted dependency roots preserve the single workspace root")
  assert_true(default_load.body.module_roots[1]:match("/lib$") ~= nil,
    "the default root is ws/lib")

  fresh()
  local configured = { "/deps/standard", "/deps/extra" }
  lw.configure({ dependency_module_roots = configured })
  configured[1] = "/mutated/caller"
  write_mag_file("roots-custom-write", "roots-custom.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("roots-custom-execute", "roots-custom.mag")
  local normal_load = latest_mag_load()
  assert_eq(normal_load.body.module_roots[1], "/deps/standard",
    "normal mag defensively copies configured dependency roots")
  assert_eq(normal_load.body.module_roots[2], "/deps/extra",
    "normal mag preserves dependency order")
  assert_true(normal_load.body.module_roots[3]:match("/lib$") ~= nil,
    "normal mag places the workspace-local root last")

  -- Mutating an emitted envelope cannot corrupt the roots held for the next
  -- eval: init.lua and mag-eval each own defensive copies.
  normal_load.body.module_roots[1] = "/mutated/envelope"
  _test.calls_clear()
  invoke_tool("roots-eval", "mag-eval", { intent = "Evaluate expression",
    expr = '(nefor.shell.command "roots" "true")',
  })
  local eval_load = latest_mag_load()
  assert_true(eval_load ~= nil, "mag-eval emits mag.load")
  assert_eq(eval_load.body.module_roots[1], "/deps/standard",
    "mag-eval receives its own defensive root copy")
  assert_eq(eval_load.body.module_roots[2], "/deps/extra",
    "mag-eval preserves dependency order")
  assert_true(eval_load.body.module_roots[3]:match("/lib$") ~= nil,
    "mag-eval places the workspace-local root last")

  fresh()
  invoke_tool("roots-eval-reset", "mag-eval", { intent = "Evaluate expression",
    expr = '(nefor.shell.command "roots-reset" "true")',
  })
  local reset_load = latest_mag_load()
  assert_eq(#reset_load.body.module_roots, 1,
    "reset restores mag-eval's exact default root set")

  assert_config_rejected("/not/a/list", "a scalar root configuration")
  assert_config_rejected(false, "a false root configuration")
  assert_config_rejected({ "" }, "an empty root")
  assert_config_rejected({ [1] = "/a", [3] = "/c" }, "a sparse root list")
  assert_config_rejected({ [1] = "/a", [4] = "/d", [5] = "/e" },
    "a root list with multiple holes and trailing numeric keys")
  assert_config_rejected({ [2] = "/b", [3] = "/c" },
    "a root list missing its first index")
  assert_config_rejected({ [1] = "/a", named = "/b" }, "a keyed root list")
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
    foreign_contracts = foreign_contracts(KERNEL_FACTORIES),
    artifact = artifact_from_modification(read_only_modification()),
  })
  calls = decode_calls()

  local exec = find_call(calls, function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
  end)
  assert_true(exec ~= nil,
    "mag.loaded releases mag.execute; got " .. json.encode(_test.calls()))
  assert_true(type(exec.body.session_id) == "string" and #exec.body.session_id > 0,
    "lead injects session_id on mag.execute")
  assert_eq(exec.body.principal, "subagent",
    "dispatched mag execute declares the subagent domain principal")

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

  -- Active run tracks the structural result actor from artifact metadata.
  local run = lw._internals.state.active_runs[reply.body.output.run_id]
  assert_true(type(run) == "table", "active_runs contains the dispatched run_id")
  assert_eq(run.terminal, "worker.llm", "the result-producing actor is terminal")
  _test.calls_clear()
  invoke_tool("firing-graph-status-actors", "graph-status", { run_id = reply.body.output.run_id })
  local status = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-graph-status-actors"
  end)
  assert_true(status ~= nil, "graph-status returns the active run")
  local nodes = status.body.output.run.nodes
  assert_eq(nodes[1].id, "worker.entry", "actor ids preserved in run summaries")
  assert_eq(nodes[2].reasoner, "nefor.factory.llm",
    "qualified foreign capability carried under the reasoner key")
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
    "worker.llm (nefor.factory.llm)",                      -- actor + capability
    "provider: \"chatgpt\"",                               -- params summary
    "Result: worker.llm (generic-provider.FinalAnswer)",   -- structural result
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
      { id = "lead.source", factory = "source",
        params = { value = { prompt = "<initial task text>" } },
        routes = { ["nefor.graph.Value"] = {
          { actor = "lead.entry", wire = "task" },
        } } },
      { id = "lead.entry", factory = "adapter", params = { seed = "provider-in" },
        routes = { ["generic-provider.ProviderOut"] = { { actor = "lead.llm", wire = "generic-provider.ProviderOut" } } } },
      { id = "lead.llm", factory = "llm", params = {},
        routes = { ["generic-provider.FinalAnswer"] = { { actor = "sink", wire = "generic-provider.FinalAnswer" } } } },
      { id = "sink", factory = "sink", params = {}, routes = {} },
    },
    messages = { { to = "lead.source", content = { kind = "mag.Unit" } } },
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
      hash = "sha256:lead",
      artifact = artifact_from_modification(lead_turn_modification()),
    }))
    calls = decode_calls()
  end
  local exec = find_call(calls, function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
       and c.body.run_name == "lead"
  end)
  if exec == nil then return nil end
  local modification = exec.body.artifact and exec.body.artifact.data
  for _, actor in ipairs(modification and modification.actors or {}) do
    if actor.id == "lead.source" then return actor.params.value.prompt end
  end
  return nil
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
  assert_true(prompt:find(out_path, 1, true) == nil,
    "the relayed turn does not duplicate the sink output path")

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

-- A successful run can still arrive without usable result content when the
-- sink did not inline a result and its persisted output is unreadable. Relay
-- that condition explicitly: the lead must not infer findings from success,
-- while the visible graph result continues to expose the artifact location.
do
  fresh()
  write_mag_file("firing-kernel-missing-write", "kernel-missing.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-kernel-missing", "kernel-missing.mag")
  feed_loaded(read_only_modification())
  local reply = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-kernel-missing"
  end)
  assert_true(reply ~= nil and reply.body.output ~= nil, "execute replies executing")

  local missing_path = os.tmpname()
  os.remove(missing_path)
  _test.calls_clear()
  feed("mag", {
    kind        = "mag.run_result",
    run_id      = reply.body.output.run_id,
    status      = "completed",
    output_path = missing_path,
  })

  local prompt = relayed_lead_prompt()
  assert_true(type(prompt) == "string",
    "missing output still relays a fresh lead turn; got " .. json.encode(_test.calls()))
  assert_true(prompt:find("result content is unavailable", 1, true) ~= nil,
    "the relayed turn names the missing-content condition")
  assert_true(prompt:find("do not infer or fabricate findings", 1, true) ~= nil,
    "the relayed turn forbids fabricating a result from successful status")
  assert_true(prompt:find(missing_path, 1, true) == nil,
    "the missing artifact path is not duplicated in model input")

  local block = find_call(decode_calls(), function(c)
    return c.body.kind == "chat.graph_result.append" and c.target == "nefor-tui"
  end)
  assert_true(block ~= nil and type(block.body.output) == "string"
              and block.body.output:find(missing_path, 1, true) ~= nil,
    "the visible graph result retains the missing artifact path")
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
  feed("mag", { kind = "mag.actor_busy", run_id = run_id, id = "worker.entry" })
  feed("mag", { kind = "mag.actor_idle", run_id = run_id, id = "worker.entry" })
  assert_eq(lw._internals.state.active_runs[run_id].nodes["worker.entry"].status,
    "done", "a settled firing is done while its actor remains resident")
  feed("mag", { kind = "mag.actor_busy", run_id = run_id, id = "worker.entry" })
  assert_eq(lw._internals.state.active_runs[run_id].nodes["worker.entry"].status,
    "running", "a later firing moves the resident actor back to running")
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
  assert_true(err ~= nil and err.body.error:find("unknown foreign", 1, true) ~= nil,
    "validation rejects the unknown capability with a clear error; got " .. json.encode(_test.calls()))
  assert_true(err.body.error:find("worker.entry", 1, true) ~= nil
              and err.body.error:find("adapter", 1, true) ~= nil,
    "rejection names the offending actor and capability")
end

-- Structural result metadata is required even though result collection is not
-- represented as an actor.
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
    "missing result boundary blocks mag.execute")
  local err = find_call(calls, function(c)
    return c.body.kind == "tool.result"
       and c.body.id == "firing-sink-missing"
       and type(c.body.error) == "string"
  end)
  assert_true(err ~= nil
              and err.body.error:find("no structural result boundary", 1, true) ~= nil,
    "missing result boundary is rejected; got " .. json.encode(_test.calls()))
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
  assert_true(err ~= nil
              and err.body.error:find("no structural result boundary", 1, true) ~= nil,
    "an orphaned result fixture is rejected; got " .. json.encode(_test.calls()))
end

-- Profile validators: config.active.orchestration_profiles is the open
-- registry. An llm actor must author :profile (or raw reasoning_effort);
-- both at once, unknown names, and malformed configured entries are rejected.
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

-- Structured-output actors carry the compiler-checked OptionalIdentifier
-- record rather than the plain string used by the untyped llm factory.
do
  fresh()
  write_mag_file("firing-typed-profile-write", "typed-profile.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-typed-profile", "typed-profile.mag")
  local m = read_only_modification()
  m.actors[2].foreign = "nefor.factory.structured-output"
  m.actors[2].params.profile = { present = true, value = "standard" }
  feed_loaded(m)
  local exec = find_call(decode_calls(), function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
  end)
  assert_true(exec ~= nil, "a typed OptionalIdentifier profile executes")
  local patch = exec.body.params_overlay["worker.llm"]
  assert_eq(patch.provider, starter_profiles.standard.provider,
    "typed profile resolves provider")
  assert_eq(patch.model, starter_profiles.standard.model,
    "typed profile resolves model")
  assert_eq(patch.reasoning_effort, starter_profiles.standard.reasoning_effort,
    "typed profile resolves effort")
end

do
  with_profiles({ zeta = starter_profiles.standard, alpha = starter_profiles.fast }, function()
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
    assert_true(err.body.error:find("Configured profiles: alpha, zeta.", 1, true) ~= nil,
      "unknown profile error lists configured names in sorted order")
  end)
end

do
  with_profiles({
    strong = { provider = "chatgpt", model = "gpt-5.6-sol", reasoning_effort = "medium" },
    bulk = { provider = "chatgpt", model = "gpt-5.6-luna", reasoning_effort = "low" },
    audit = { provider = "openai", model = "review-model", reasoning_effort = "high" },
    another = { provider = "openai", model = "other-model", reasoning_effort = "medium" },
    fifth = { provider = "openai", model = "fifth-model", reasoning_effort = "low" },
  }, function()
    fresh()
    write_mag_file("firing-profile-strong-write", "strong-profile.mag", READ_ONLY_MAG)
    _test.calls_clear()
    execute_mag("firing-profile-strong", "strong-profile.mag")
    local m = read_only_modification()
    m.actors[2].params.profile = "strong"
    feed_loaded(m)
    local exec = find_call(decode_calls(), function(c)
      return c.body.kind == "mag.execute" and c.target == "mag"
    end)
    assert_true(exec ~= nil, "an arbitrary configured profile executes")
    local patch = exec.body.params_overlay["worker.llm"]
    assert_eq(patch.provider, "chatgpt", "custom profile resolves provider")
    assert_eq(patch.model, "gpt-5.6-sol", "custom profile resolves model")
    assert_eq(patch.reasoning_effort, "medium", "custom profile resolves effort")
  end)
end

local function assert_profile_config_error(profiles, expected)
  with_profiles(profiles, function()
    fresh()
    write_mag_file("firing-profile-malformed-write", "malformed-profile.mag", READ_ONLY_MAG)
    _test.calls_clear()
    execute_mag("firing-profile-malformed", "malformed-profile.mag")
    feed_loaded(read_only_modification())
    local err = find_call(decode_calls(), function(c)
      return c.body.kind == "tool.result"
         and c.body.id == "firing-profile-malformed"
         and type(c.body.error) == "string"
    end)
    assert_true(err ~= nil and err.body.error:find(expected, 1, true) ~= nil,
      "malformed profile registry is rejected with '" .. expected .. "'; got " ..
      json.encode(_test.calls()))
  end)
end

assert_profile_config_error(nil, "config.active.orchestration_profiles must be a table")
assert_profile_config_error({ [1] = starter_profiles.standard },
  "config.active.orchestration_profiles keys must be non-empty strings")
assert_profile_config_error({ [""] = starter_profiles.standard },
  "config.active.orchestration_profiles keys must be non-empty strings")
assert_profile_config_error({}, "unknown profile 'standard'. Configured profiles: none.")
assert_profile_config_error({ standard = "bad" }, "configured profile 'standard' must be a table")
assert_profile_config_error({
  standard = { provider = "", model = "model", reasoning_effort = "medium" },
}, "configured profile 'standard' requires a non-empty string provider")
assert_profile_config_error({
  standard = { provider = "chatgpt", model = nil, reasoning_effort = "medium" },
}, "configured profile 'standard' requires a non-empty string model")
assert_profile_config_error({
  standard = { provider = "chatgpt", model = "model", reasoning_effort = 3 },
}, "configured profile 'standard' requires a non-empty string reasoning_effort")
assert_profile_config_error({
  standard = { provider = "chatgpt", model = "model", reasoning_effort = "medium" },
  broken = { provider = "chatgpt", model = "", reasoning_effort = "low" },
}, "configured profile 'broken' requires a non-empty string model")

-- mag.loaded snapshots qualified foreign capabilities for validation.
do
  fresh()
  feed("mag", {
    kind      = "mag.loaded",
    foreign_contracts = foreign_contracts({ "sink", "llm", "stub", "run-tool" }),
  })
  local set = lw._internals.state.kernel_factories
  assert_true(type(set) == "table"
              and set["nefor.factory.sink"] == true
              and set["nefor.factory.llm"] == true,
    "mag.loaded populates the foreign capability registry snapshot")
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
-- Permission modes: auto/yolo bypass safe-mode writer gates. Auto
-- write-review immediately approves so the writer can execute.
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
  local sub = find_call(calls, function(c)
    return c.body.kind == "lead-workflow.plan.submitted"
  end)
  assert_true(sub ~= nil, "auto write-review emits plan.submitted")
  assert_eq(sub.body.plan, "Auto mode plan", "submitted event carries plan text")

  local approved_env = find_call(calls, function(c)
    return c.body.kind == "lead-workflow.plan.approved"
  end)
  assert_true(approved_env ~= nil, "auto write-review emits plan.approved")
  assert_eq(approved_env.body.approved, true, "auto write-review approves")

  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-plan-auto"
  end)
  assert_true(reply ~= nil, "auto write-review returns immediately")
  assert_eq(reply.body.error, nil, "auto write-review does not return an error")
  assert_eq(reply.body.output.status, "approved", "auto write-review returns approved")

  local ap = lw._internals.state.active_plan
  assert_true(type(ap) == "table", "auto write-review records active_plan")
  assert_eq(ap.status, "approved", "auto active_plan is approved")
  assert_eq(ap.pending_firing_id, nil,
    "auto active_plan has no pending_firing_id")

  write_mag_file("firing-writer-write-auto-reviewed", "auto-reviewed-build.mag", WRITER_MAG)
  _test.calls_clear()
  execute_mag("firing-writer-auto-reviewed", "auto-reviewed-build.mag")
  feed_loaded(writer_modification())
  local exec = find_call(decode_calls(), function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
  end)
  assert_true(exec ~= nil, "auto write-review approval lets writer execute")
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

-- (mag-eval caller routing and ownership) The gate-facing firing id is an
-- inner correlation; caller_id retains the outer scoped capability identity.
do
  local mag_eval = require("libs.lead-workflow.mag-eval")
  local artifact = artifact_from_modification(read_only_modification())

  -- Lead caller: classify by caller_id, validate, submit, acknowledge with the
  -- standard stable handle, and register in the shared active-run table.
  fresh()
  agentic_loop._internals.state.current_turn = { scope = "r7" }
  feed("tool-gate", { kind = "lead-workflow.tool.invoke", id = "gate-77",
    caller_id = "r7/cap-1", from = "lead.llm", name = "mag-eval",
    args = { intent = "Inspect files", expr = "(nefor.shell.command \"x\" \"pwd\")" } })
  local load = latest_mag_load()
  assert_true(load ~= nil, "lead eval starts a compile handshake")
  assert_eq(find_call(decode_calls(), function(c) return c.body.kind == "tool.result" end), nil,
    "no acknowledgment exists before compilation and validation")
  _test.calls_clear()
  feed("mag", { kind = "mag.loaded", in_reply_to = load.body.id,
    hash = "sha256:eval", foreign_contracts = foreign_contracts(), artifact = artifact })
  local calls = decode_calls()
  local exec = find_call(calls, function(c) return c.body.kind == "mag.execute" end)
  local ack = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "gate-77"
  end)
  assert_true(exec ~= nil and ack ~= nil, "lead eval executes and promptly acknowledges")
  assert_eq(exec.body.principal, "subagent", "trusted local subagent principal is preserved")
  assert_eq(ack.body.output.status, "executing", "lead eval uses structured executing ack")
  assert_eq(ack.body.output.engine, "mag-kernel", "lead eval names the standard engine")
  assert_eq(ack.body.output.hash, "sha256:eval", "lead eval ack carries compile hash")
  assert_eq(ack.body.output.run_id, exec.body.run_id, "ack handle equals execute run id")
  local run_id = exec.body.run_id
  assert_true(lw._internals.state.active_runs[run_id] ~= nil,
    "lead eval is registered in standard active_runs")
  assert_eq(next(mag_eval._internals.state.attached_runs), nil,
    "lead eval has no duplicate detached owner")

  -- Terminal delivery is later and goes through the standard result renderer,
  -- archive, and completion relay exactly once.
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id,
    status = "completed", result = { text = "the eval output" } })
  calls = decode_calls()
  assert_eq(find_call(calls, function(c) return c.body.kind == "tool.result" end), nil,
    "terminal result does not emit a second tool result")
  assert_true(find_call(calls, function(c)
    return c.body.kind == "chat.graph_result.append" and c.body.run_id == run_id
  end) ~= nil, "lead eval renders through the standard graph result channel")
  assert_eq(lw._internals.state.active_runs[run_id], nil, "terminal result closes active run")
  assert_eq(lw._internals.state.completed_runs[#lw._internals.state.completed_runs].run_id,
    run_id, "terminal result archives the stable run handle")
  local queued = agentic_loop._internals.state.pending_user_inputs[1]
  assert_true(type(queued) == "string" and queued:find("the eval output", 1, true) ~= nil,
    "terminal output reaches the deferred graph completion channel")

  -- Graph-agent caller: a foreign caller_id remains attached and receives no
  -- acknowledgment until its terminal result.
  fresh()
  agentic_loop._internals.state.current_turn = { scope = "r7" }
  feed("tool-gate", { kind = "lead-workflow.tool.invoke", id = "gate-88",
    caller_id = "r9/cap-4", from = "worker.run-tool", name = "mag-eval",
    args = { intent = "Inspect files", expr = "(nefor.shell.command \"x\" \"pwd\")" } })
  load = latest_mag_load()
  _test.calls_clear()
  feed("mag", { kind = "mag.loaded", in_reply_to = load.body.id,
    hash = "sha256:agent", foreign_contracts = foreign_contracts(), artifact = artifact })
  calls = decode_calls()
  exec = find_call(calls, function(c) return c.body.kind == "mag.execute" end)
  assert_true(exec ~= nil, "graph-agent eval executes after validation")
  assert_eq(find_call(calls, function(c) return c.body.kind == "tool.result" end), nil,
    "graph-agent eval stays attached")
  assert_true(mag_eval._internals.state.attached_runs[exec.body.run_id] ~= nil,
    "attached run is owned only until terminal capability settlement")
  assert_eq(lw._internals.state.active_runs[exec.body.run_id], nil,
    "attached graph-agent eval is not a lead active run")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = exec.body.run_id,
    status = "completed", result = { text = "blocking output" } })
  local reply = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "gate-88"
  end)
  assert_true(reply ~= nil, "graph-agent eval settles on terminal result")
  assert_eq(reply.body.output, "blocking output", "attached result returns terminal output")

  -- Cancel is terminal for an attached eval: ownership is removed before the
  -- kernel interrupt, and duplicate cancel / late terminal replies stay silent.
  fresh()
  agentic_loop._internals.state.current_turn = { scope = "r7" }
  feed("tool-gate", { kind = "lead-workflow.tool.invoke", id = "gate-attached-cancel",
    caller_id = "r9/cap-5", from = "worker.run-tool", name = "mag-eval",
    args = { intent = "Wait forever", expr = "(nefor.shell.command \"x\" \"sleep 10\")" } })
  load = latest_mag_load()
  _test.calls_clear()
  feed("mag", { kind = "mag.loaded", in_reply_to = load.body.id,
    hash = "sha256:agent-cancel", foreign_contracts = foreign_contracts(), artifact = artifact })
  exec = find_call(decode_calls(), function(c) return c.body.kind == "mag.execute" end)
  assert_true(exec ~= nil and mag_eval._internals.state.attached_runs[exec.body.run_id] ~= nil,
    "cancel test starts an attached eval")
  _test.calls_clear()
  feed("tool-gate", { kind = "lead-workflow.tool.cancel", id = "gate-attached-cancel" })
  calls = decode_calls()
  assert_true(find_call(calls, function(c)
    return c.body.kind == "mag.interrupt_run" and c.body.run_id == exec.body.run_id
       and c.body.terminate == true
  end) ~= nil, "attached cancel terminates the eval")
  assert_eq(mag_eval._internals.state.attached_runs[exec.body.run_id], nil,
    "attached cancel releases source ownership")
  _test.calls_clear()
  feed("tool-gate", { kind = "lead-workflow.tool.cancel", id = "gate-attached-cancel" })
  feed("mag", { kind = "mag.run_result", run_id = exec.body.run_id,
    status = "failed", error = "late cancellation result" })
  assert_eq(#decode_calls(), 0,
    "duplicate cancel and late attached result cannot emit a second source settlement")

  local function dispatch_lead_eval(inner, outer)
    fresh()
    agentic_loop._internals.state.current_turn = { scope = "r7" }
    feed("tool-gate", { kind = "lead-workflow.tool.invoke", id = inner,
      caller_id = outer, name = "mag-eval",
      args = { intent = "Inspect lifecycle", expr = "(nefor.shell.command \"x\" \"pwd\")" } })
    local pending_load = latest_mag_load()
    _test.calls_clear()
    feed("mag", { kind = "mag.loaded", in_reply_to = pending_load.body.id,
      hash = "sha256:lifecycle", foreign_contracts = foreign_contracts(), artifact = artifact })
    local submitted = find_call(decode_calls(), function(c)
      return c.body.kind == "mag.execute"
    end)
    assert_true(submitted ~= nil, "lifecycle eval submits")
    return submitted.body.run_id
  end

  -- Failed/killed outcomes use the same standard close and relay path.
  run_id = dispatch_lead_eval("gate-failed", "r7/cap-20")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id,
    status = "failed", error = "eval failed" })
  calls = decode_calls()
  assert_true(find_call(calls, function(c)
    return c.body.kind == "chat.graph_result.append" and c.body.status == "failed"
  end) ~= nil, "failed eval renders a standard failed graph result")
  assert_eq(lw._internals.state.completed_runs[#lw._internals.state.completed_runs].run_id,
    run_id, "failed eval is archived")

  run_id = dispatch_lead_eval("gate-killed", "r7/cap-21")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "killed" })
  assert_true(find_call(decode_calls(), function(c)
    return c.body.kind == "chat.graph_result.append" and c.body.status == "failed"
  end) ~= nil, "killed eval closes through the standard failure channel")
  assert_eq(lw._internals.state.active_runs[run_id], nil, "killed eval leaves no active owner")

  -- Standard control surfaces address the exact acknowledged handle.
  run_id = dispatch_lead_eval("gate-terminate", "r7/cap-22")
  _test.calls_clear()
  invoke_tool("terminate-eval", "terminate-graph", { run_id = run_id })
  calls = decode_calls()
  assert_true(find_call(calls, function(c)
    return c.body.kind == "mag.kill_run" and c.body.run_id == run_id
  end) ~= nil, "terminate-graph kills the eval by its stable handle")
  assert_true(lw._internals.state.active_runs[run_id] ~= nil,
    "terminate-graph retains the eval until canonical confirmation")
  assert_eq(lw._internals.state.active_runs[run_id].phase, "terminating",
    "terminate-graph marks the eval terminating")
  feed("mag", { kind = "mag.run_result", run_id = run_id,
    status = "killed", error = "terminated" })
  assert_eq(lw._internals.state.active_runs[run_id], nil,
    "canonical killed result closes the terminating eval")

  run_id = dispatch_lead_eval("gate-cancel", "r7/cap-23")
  _test.calls_clear()
  feed("tool-gate", { kind = "lead-workflow.tool.cancel", id = "gate-cancel" })
  assert_true(find_call(decode_calls(), function(c)
    return c.body.kind == "mag.interrupt_run" and c.body.run_id == run_id
       and c.body.terminate == true
  end) ~= nil, "dispatch-firing cancellation terminates the standard eval run")

  run_id = dispatch_lead_eval("gate-session", "r7/cap-24")
  _test.calls_clear()
  lw._internals.terminate_active_graph()
  assert_true(find_call(decode_calls(), function(c)
    return c.body.kind == "mag.kill_run" and c.body.run_id == run_id
  end) ~= nil, "session cleanup kills the standard eval run")
  assert_eq(lw._internals.state.active_runs[run_id], nil,
    "session cleanup removes eval active ownership")
end

-- TUI-requested workflow termination settles and renders every run without
-- turning the user's stop decision into replacement lead work.
do
  fresh()
  local registry = lw._internals.run_registry
  local run_ids = {}
  for i = 1, 2 do
    local run_id = registry:mint_run_id()
    lw._internals.register_active_run(run_id,
      { { id = "worker-" .. i, foreign = "llm" } }, "worker-" .. i,
      "dispatch-" .. i, "terminated-" .. i, sessions.current_id())
    invoke_tool("wait-terminated-" .. i, "await-run", { run_id = run_id })
    run_ids[i] = run_id
  end
  _test.calls_clear()
  feed("nefor-tui", { kind = "chat.workflows.terminate_requested", scope = "all" })
  assert_eq(lw._internals.state.active_runs[run_ids[1]].terminate_reason,
    "user-tui-termination", "TUI provenance classifies the first live run")
  assert_eq(lw._internals.state.active_runs[run_ids[2]].terminate_reason,
    "user-tui-termination", "TUI provenance classifies every live run")

  for i, run_id in ipairs(run_ids) do
    feed("mag", { kind = "mag.run_result", run_id = run_id,
      status = "killed", error = "run killed" })
    local calls = decode_calls()
    assert_true(find_call(calls, function(c)
      return c.body.kind == "chat.graph_result.append"
        and c.body.run_id == run_id
        and c.body.status == "failed"
        and c.body.error == "run killed"
    end) ~= nil, "user-terminated run " .. i .. " remains visibly failed")
    assert_true(find_call(calls, function(c)
      return c.body.kind == "tool.result"
        and c.body.id == "wait-terminated-" .. i
        and c.body.error_code == "await_run_killed"
    end) ~= nil, "user-terminated run " .. i .. " still settles its waiter")
  end
  assert_eq(#agentic_loop._internals.state.deferred_queue, 0,
    "user termination creates no deferred lead task")
  assert_eq(#agentic_loop._internals.state.pending_user_inputs, 0,
    "user termination creates no queued lead task")
  assert_eq(find_call(decode_calls(), function(c) return c.body.kind == "mag.execute" end), nil,
    "multiple killed workflows create no replacement lead turn")

  _test.calls_clear()
  for _, run_id in ipairs(run_ids) do
    feed("mag", { kind = "mag.run_result", run_id = run_id,
      status = "killed", error = "duplicate" })
  end
  assert_eq(#decode_calls(), 0, "duplicate user-termination terminals remain idempotent")
end

-- Cancellation during pending load removes correlation, so a late compiler
-- response cannot submit orphaned work.
do
  local mag_eval = require("libs.lead-workflow.mag-eval")
  fresh()
  agentic_loop._internals.state.current_turn = { scope = "r7" }
  feed("tool-gate", { kind = "lead-workflow.tool.invoke", id = "gate-pending",
    caller_id = "r7/cap-9", name = "mag-eval",
    args = { intent = "Inspect files", expr = "(nefor.shell.command \"x\" \"sleep 1\")" } })
  local load = latest_mag_load()
  assert_true(mag_eval._internals.state.pending_loads[load.body.id] ~= nil,
    "compile is pending before cancellation")
  feed("tool-gate", { kind = "lead-workflow.tool.cancel", id = "gate-pending" })
  assert_eq(mag_eval._internals.state.pending_loads[load.body.id], nil,
    "cancel removes pending compile correlation")
  _test.calls_clear()
  feed("mag", { kind = "mag.loaded", in_reply_to = load.body.id,
    hash = "sha256:late", foreign_contracts = foreign_contracts(),
    artifact = artifact_from_modification(read_only_modification()) })
  assert_eq(find_call(decode_calls(), function(c) return c.body.kind == "mag.execute" end), nil,
    "late compile response cannot execute orphaned work")
  assert_eq(next(lw._internals.state.active_runs), nil, "late response registers no active run")
end

-- Compile and pre-execute validation failures remain direct and never create a
-- handle or active run.
do
  fresh()
  agentic_loop._internals.state.current_turn = { scope = "r7" }
  feed("tool-gate", { kind = "lead-workflow.tool.invoke", id = "gate-compile-error",
    caller_id = "r7/cap-10", name = "mag-eval",
    args = { intent = "Compile expression", expr = "(broken" } })
  local load = latest_mag_load()
  _test.calls_clear()
  feed("mag", { kind = "mag.error", in_reply_to = load.body.id, message = "unexpected EOF" })
  local calls = decode_calls()
  local err = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "gate-compile-error"
  end)
  assert_true(err ~= nil and err.body.error:find("unexpected EOF", 1, true) ~= nil,
    "eval compile error returns directly to its firing")
  assert_eq(find_call(calls, function(c) return c.body.kind == "mag.execute" end), nil,
    "compile failure never executes")
  assert_eq(next(lw._internals.state.active_runs), nil, "compile failure has no run handle")

  fresh()
  agentic_loop._internals.state.current_turn = { scope = "r7" }
  feed("tool-gate", { kind = "lead-workflow.tool.invoke", id = "gate-validation-error",
    caller_id = "r7/cap-11", name = "mag-eval",
    args = { intent = "Validate expression", expr = "(nefor.shell.command \"x\" \"pwd\")" } })
  load = latest_mag_load()
  local invalid = artifact_from_modification(read_only_modification())
  invalid.data.result = nil
  _test.calls_clear()
  feed("mag", { kind = "mag.loaded", in_reply_to = load.body.id,
    hash = "sha256:invalid", foreign_contracts = foreign_contracts(), artifact = invalid })
  calls = decode_calls()
  err = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "gate-validation-error"
  end)
  assert_true(err ~= nil and err.body.error:find("structural result boundary", 1, true) ~= nil,
    "eval validation error returns directly to its firing")
  assert_eq(find_call(calls, function(c) return c.body.kind == "mag.execute" end), nil,
    "validation failure never executes")
  assert_eq(next(lw._internals.state.active_runs), nil, "validation failure has no run handle")
end

-- Concurrent eval loads may resolve in reverse order without crossing firing
-- correlation or stable run handles.
do
  fresh()
  agentic_loop._internals.state.current_turn = { scope = "r7" }
  for _, call in ipairs({
    { inner = "gate-a", outer = "r7/cap-12", intent = "Inspect alpha" },
    { inner = "gate-b", outer = "r7/cap-13", intent = "Inspect beta" },
  }) do
    feed("tool-gate", { kind = "lead-workflow.tool.invoke", id = call.inner,
      caller_id = call.outer, name = "mag-eval",
      args = { intent = call.intent, expr = "(nefor.shell.command \"x\" \"pwd\")" } })
  end
  local loads = find_calls(decode_calls(), function(c)
    return c.body.kind == "mag.load" and c.target == "mag"
  end)
  assert_eq(#loads, 2, "two concurrent eval compiles are pending")
  _test.calls_clear()
  for i = #loads, 1, -1 do
    feed("mag", { kind = "mag.loaded", in_reply_to = loads[i].body.id,
      hash = "sha256:" .. tostring(i), foreign_contracts = foreign_contracts(),
      artifact = artifact_from_modification(read_only_modification()) })
  end
  local calls = decode_calls()
  for _, inner in ipairs({ "gate-a", "gate-b" }) do
    local ack = find_call(calls, function(c)
      return c.body.kind == "tool.result" and c.body.id == inner
    end)
    assert_true(ack ~= nil and lw._internals.state.active_runs[ack.body.output.run_id] ~= nil,
      "reverse load response preserves " .. inner .. " run correlation")
  end
  local handles = {}
  for run_id in pairs(lw._internals.state.active_runs) do handles[#handles + 1] = run_id end
  table.sort(handles)
  _test.calls_clear()
  invoke_tool("reverse-wait-1", "await-run", { run_id = handles[1] })
  invoke_tool("reverse-wait-2", "await-run", { run_id = handles[2] })
  feed("mag", { kind = "mag.run_result", run_id = handles[2], status = "completed",
    result = { text = "second first" } })
  feed("mag", { kind = "mag.run_result", run_id = handles[1], status = "completed",
    result = { text = "first second" } })
  calls = decode_calls()
  local by_id = {}
  for _, call in ipairs(calls) do
    if call.body.kind == "tool.result" then by_id[call.body.id] = call.body end
  end
  assert_eq(by_id["reverse-wait-1"].output.result.text, "first second",
    "reverse terminal order preserves first handle correlation")
  assert_eq(by_id["reverse-wait-2"].output.result.text, "second first",
    "reverse terminal order preserves second handle correlation")
end

-- File-based execute has the same pending-load cancellation guarantees as
-- mag-eval: cancel invalidates by dispatch firing, is idempotent, and makes
-- either kind of late compiler response a silent no-op.
do
  fresh()
  write_mag_file("file-pending-write", "pending-cancel.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("file-pending-execute", "pending-cancel.mag")
  local load = latest_mag_load()
  assert_true(load ~= nil and lw._internals.state.pending_mag_load[load.body.id] ~= nil,
    "file execute compile is pending before cancellation")

  _test.calls_clear()
  feed("tool-gate", { kind = "lead-workflow.tool.cancel", id = "file-pending-execute" })
  assert_eq(lw._internals.state.pending_mag_load[load.body.id], nil,
    "file execute cancel invalidates pending load correlation")
  assert_eq(#decode_calls(), 0,
    "canceling an unsubmitted file load emits no source settlement or kernel control")

  feed("tool-gate", { kind = "lead-workflow.tool.cancel", id = "file-pending-execute" })
  feed("mag", { kind = "mag.loaded", in_reply_to = load.body.id,
    hash = "sha256:file-late", foreign_contracts = foreign_contracts(),
    artifact = artifact_from_modification(read_only_modification()) })
  feed("mag", { kind = "mag.error", in_reply_to = load.body.id,
    message = "late compiler failure" })
  local calls = decode_calls()
  assert_eq(find_call(calls, function(c) return c.body.kind == "mag.execute" end), nil,
    "duplicate cancel and late loaded response cannot execute file work")
  assert_eq(find_call(calls, function(c) return c.body.kind == "tool.result" end), nil,
    "late loaded/error responses cannot settle canceled file execute again")
  assert_eq(next(lw._internals.state.active_runs), nil,
    "late file responses register no active run")
end

-- Session teardown clears every pending file load. Compiler responses arriving
-- after the boundary cannot launch work into the ended or following session.
do
  fresh()
  write_mag_file("file-session-write", "pending-session.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("file-session-execute", "pending-session.mag")
  local load = latest_mag_load()
  assert_true(load ~= nil and lw._internals.state.pending_mag_load[load.body.id] ~= nil,
    "file execute compile is pending before session end")

  lw._internals.terminate_active_graph()
  assert_eq(next(lw._internals.state.pending_mag_load), nil,
    "session end clears all pending file loads")
  _test.calls_clear()
  feed("mag", { kind = "mag.loaded", in_reply_to = load.body.id,
    hash = "sha256:session-late", foreign_contracts = foreign_contracts(),
    artifact = artifact_from_modification(read_only_modification()) })
  feed("mag", { kind = "mag.error", in_reply_to = load.body.id,
    message = "late session compiler failure" })
  local calls = decode_calls()
  assert_eq(find_call(calls, function(c) return c.body.kind == "mag.execute" end), nil,
    "post-session loaded response cannot execute file work")
  assert_eq(find_call(calls, function(c) return c.body.kind == "tool.result" end), nil,
    "post-session loaded/error responses emit no source settlement")
  assert_eq(next(lw._internals.state.active_runs), nil,
    "post-session file responses register no active run")
end

-- ------------------------------------------------------------------
-- double-Esc interrupts DETACHED dispatched runs (the tag-blocking incident)
-- ------------------------------------------------------------------
--
-- The `mag` execute tool is FIRE-AND-FORGET: it acks "executing" at dispatch
-- and the lead's turn completes and goes idle while the sub-run churns. When
-- the user double-Escs, the agentic-loop's own interrupt sees NOTHING (the lead
-- is not blocked on a current_run_id), so the detached runs would sail on — the
-- verified incident. lead-workflow owns state.active_runs, so its own
-- `chat.interrupt_all` subscription interrupts EACH detached run. This is the
-- end-to-end test whose absence let the bug ship.
do
  fresh()
  -- Dispatch two detached runs through the real load/execute handshake. Clear
  -- the call log between them so feed_loaded matches each run's own mag.load
  -- (both are on the bus otherwise, and the matcher takes the first).
  write_mag_file("firing-a", "run-a.mag", READ_ONLY_MAG)
  execute_mag("firing-exec-a", "run-a.mag")
  feed_loaded(read_only_modification())
  _test.calls_clear()
  write_mag_file("firing-b", "run-b.mag", READ_ONLY_MAG)
  execute_mag("firing-exec-b", "run-b.mag")
  feed_loaded(read_only_modification())

  local run_ids = {}
  for id, _ in pairs(lw._internals.state.active_runs) do run_ids[#run_ids + 1] = id end
  assert_eq(#run_ids, 2, "two detached dispatched runs are tracked in active_runs")
  _test.calls_clear()

  -- Double-Esc while the lead is idle. nefor-tui broadcasts chat.interrupt_all.
  feed("nefor-tui", { kind = "chat.interrupt_all" })

  local interrupts = find_calls(decode_calls(), function(c)
    return c.body.kind == "mag.interrupt_run" and c.target == "mag"
  end)
  assert_eq(#interrupts, 2,
    "interrupt_all interrupts EVERY detached run; got " .. json.encode(_test.calls()))
  local hit = {}
  for _, c in ipairs(interrupts) do
    hit[c.body.run_id] = true
    -- A dispatched run is ephemeral: it must be TERMINATED (ended failed), not
    -- gracefully interrupted — otherwise its agent llm re-fires and answers
    -- "Completed", relaying a phantom success (the incident this fixes).
    assert_true(c.body.terminate == true,
      "detached run " .. tostring(c.body.run_id) .. " is TERMINATED, not gracefully interrupted")
  end
  for _, rid in ipairs(run_ids) do
    assert_true(hit[rid], "detached run " .. rid .. " was interrupted by double-Esc")
  end
end

-- (relay of interruption) An interrupted dispatched run settles failed
-- "interrupted by user"; that failure must reach the lead's next turn through
-- the relay — never a silent disappearance (the no-amnesia principle). Spies
-- the relay to prove the failure crosses the layer boundary intact.
do
  fresh()
  write_mag_file("firing-relay", "relay.mag", READ_ONLY_MAG)
  execute_mag("firing-exec-relay", "relay.mag")
  feed_loaded(read_only_modification())
  local run_id = next(lw._internals.state.active_runs)
  assert_true(type(run_id) == "string", "a run is tracked after dispatch")

  local captured
  local orig = agentic_loop.relay_run_completion
  agentic_loop.relay_run_completion = function(c) captured = c end
  _test.calls_clear()

  feed("mag", {
    kind = "mag.run_result", run_id = run_id,
    status = "failed", error = "interrupted by user",
  })
  agentic_loop.relay_run_completion = orig

  assert_true(captured ~= nil,
    "the interrupted run's failure reaches agentic-loop's relay")
  assert_eq(captured.status, "failed", "relay carries the failed status")
  assert_true(type(captured.error) == "string"
    and captured.error:find("interrupted by user") ~= nil,
    "relay carries the interruption reason — not a silent drop")
  assert_eq(next(lw._internals.state.active_runs), nil,
    "the interrupted run is closed out of active_runs")
end

-- (mag execute dispatch cancel propagation) A `tool.cancel` addressed to a
-- `mag` execute DISPATCH firing propagates into that detached run —
-- completeness for the general cancel route, mirroring mag-eval.cancel for
-- blocking firings.
do
  fresh()
  write_mag_file("firing-c", "run-c.mag", READ_ONLY_MAG)
  execute_mag("firing-exec-c", "run-c.mag")
  feed_loaded(read_only_modification())
  local run_id = next(lw._internals.state.active_runs)
  local run = lw._internals.state.active_runs[run_id]
  assert_eq(run.dispatch_firing_id, "firing-exec-c",
    "the run records its dispatch firing id")
  _test.calls_clear()

  feed("tool-gate", { kind = "lead-workflow.tool.cancel", id = "firing-exec-c" })
  local interrupt = find_call(decode_calls(), function(c)
    return c.body.kind == "mag.interrupt_run" and c.target == "mag"
       and c.body.run_id == run_id
  end)
  assert_true(interrupt ~= nil,
    "a cancel for the dispatch firing interrupts the detached run")
  assert_true(interrupt.body.terminate == true,
    "the dispatch-firing cancel TERMINATES the detached run (ends it failed)")

  _test.calls_clear()
  feed("tool-gate", { kind = "lead-workflow.tool.cancel", id = "firing-nope" })
  assert_eq(find_call(decode_calls(), function(c)
    return c.body.kind == "mag.interrupt_run"
  end), nil, "a cancel for an unknown firing emits no interrupt")
end

-- (interrupt_all with nothing dispatched is a clean no-op)
do
  fresh()
  _test.calls_clear()
  feed("nefor-tui", { kind = "chat.interrupt_all" })
  assert_eq(find_call(decode_calls(), function(c)
    return c.body.kind == "mag.interrupt_run"
  end), nil, "no active runs → interrupt_all emits nothing")
end

-- mag-eval display intent is mandatory and bounded to 1-5 words.
do
  fresh()
  _test.calls_clear()
  local mag_eval = require("libs.lead-workflow.mag-eval")
  mag_eval.handle("intent-missing", { expr = "(nefor.shell.command \"x\" \"pwd\")" })
  local missing = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "intent-missing"
  end)
  assert_true(missing ~= nil and missing.body.error:find("1%-5 words") ~= nil,
    "mag-eval rejects missing intent")
  _test.calls_clear()
  mag_eval.handle("intent-long", { intent = "one two three four five six", expr = "(nefor.shell.command \"x\" \"pwd\")" })
  local long = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "intent-long"
  end)
  assert_true(long ~= nil and long.body.error:find("1%-5 words") ~= nil,
    "mag-eval rejects overlong intent")
end

-- Delayed gate approvals consume their preserved invocation provenance. Lead
-- provenance still receives the stable detached acknowledgement; if approval
-- lands after a session switch, both mag and mag-eval fail before touching a
-- workspace or starting a compiler load.
do
  fresh()
  local owning_session = sessions.current_id()
  write_mag_file("provenance-write", "provenance.mag", READ_ONLY_MAG)
  _test.calls_clear()
  invoke_tool_with_metadata("provenance-mag-lead", "mag", {
    action = "execute", file = "provenance.mag",
  }, { caller_id = "opaque-gate-inner", invocation = invocation(owning_session, "lead") })
  feed_loaded(read_only_modification())
  local lead_ack = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "provenance-mag-lead"
  end)
  assert_true(lead_ack ~= nil and lead_ack.body.output.status == "executing",
    "lead provenance yields the immediate stable mag acknowledgement")
  assert_true(lw._internals.state.active_runs[lead_ack.body.output.run_id] ~= nil,
    "lead provenance selects detached awaitable routing independently of caller_id")

  fresh()
  owning_session = sessions.current_id()
  write_mag_file("provenance-attached-write", "provenance-attached.mag", READ_ONLY_MAG)
  _test.calls_clear()
  invoke_tool_with_metadata("provenance-mag-agent", "mag", {
    action = "execute", file = "provenance-attached.mag",
  }, { caller_id = "r-agent/cap-1", invocation = invocation(owning_session, "subagent", "r-agent/cap-1") })
  feed_loaded(read_only_modification())
  local attached_exec = find_call(decode_calls(), function(c) return c.body.kind == "mag.execute" end)
  assert_true(attached_exec ~= nil and lw._internals.state.active_runs[attached_exec.body.run_id] == nil,
    "subagent provenance keeps file mag attached and out of the awaitable registry")
  assert_eq(find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "provenance-mag-agent"
  end), nil, "attached file mag does not acknowledge before terminal completion")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = attached_exec.body.run_id,
    status = "completed", result = { text = "attached result" } })
  assert_true(find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "provenance-mag-agent"
  end) ~= nil, "attached file mag settles its graph-agent capability")

  fresh()
  owning_session = sessions.current_id()
  local stale_mag = invocation(owning_session, "lead", "r-stale/cap-1")
  sessions.new()
  _test.calls_clear()
  invoke_tool_with_metadata("stale-mag", "mag", {
    action = "execute", file = "never-loaded.mag",
  }, { caller_id = "r-current/cap-1", invocation = stale_mag })
  local calls = decode_calls()
  local stale_error = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "stale-mag"
  end)
  assert_true(stale_error ~= nil and stale_error.body.error:find("no longer active", 1, true),
    "delayed mag approval fails against its ended invocation session")
  assert_eq(find_call(calls, function(c) return c.body.kind == "mag.load" end), nil,
    "stale mag provenance performs no workspace load or execute")

  fresh()
  owning_session = sessions.current_id()
  local lead_eval = invocation(owning_session, "lead", "r-eval/cap-1")
  _test.calls_clear()
  invoke_tool_with_metadata("provenance-eval-lead", "mag-eval", {
    intent = "Inspect provenance", expr = "(nefor.shell.command \"x\" \"pwd\")",
  }, { caller_id = "opaque-gate-inner", invocation = lead_eval })
  local eval_load = latest_mag_load()
  feed("mag", { kind = "mag.loaded", in_reply_to = eval_load.body.id,
    hash = "sha256:provenance", foreign_contracts = foreign_contracts(),
    artifact = artifact_from_modification(read_only_modification()) })
  lead_ack = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "provenance-eval-lead"
  end)
  assert_true(lead_ack ~= nil and lead_ack.body.output.status == "executing",
    "lead provenance yields the immediate stable mag-eval acknowledgement")
  assert_true(lw._internals.state.active_runs[lead_ack.body.output.run_id] ~= nil,
    "lead eval provenance selects detached awaitable routing")

  fresh()
  owning_session = sessions.current_id()
  local stale_eval = invocation(owning_session, "lead", "r-stale-eval/cap-1")
  sessions.new()
  _test.calls_clear()
  invoke_tool_with_metadata("stale-eval", "mag-eval", {
    intent = "Never execute", expr = "(nefor.shell.command \"x\" \"pwd\")",
  }, { caller_id = "r-current/cap-2", invocation = stale_eval })
  calls = decode_calls()
  stale_error = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "stale-eval"
  end)
  assert_true(stale_error ~= nil and stale_error.body.error:find("no longer active", 1, true),
    "delayed mag-eval approval fails against its ended invocation session")
  assert_eq(find_call(calls, function(c) return c.body.kind == "mag.load" end), nil,
    "stale mag-eval provenance performs no workspace write, load, or execute")
end

-- await-run blocks on the canonical terminal event without polling. Multiple
-- waiters receive one canonical result each and the normal relay remains once.
local function dispatch_awaitable(tag)
  fresh()
  write_mag_file(tag .. "-write", tag .. ".mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag(tag .. "-execute", tag .. ".mag")
  feed_loaded(read_only_modification())
  local ack = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == tag .. "-execute"
  end)
  assert_true(ack ~= nil and ack.body.output.run_id:match("^mag%-run%-%w[%w_-]*$") ~= nil,
    "dispatch returns an opaque awaitable run handle")
  return ack.body.output.run_id
end

do
  local run_id = dispatch_awaitable("await-slow")
  _test.calls_clear()
  invoke_tool("waiter-b", "await-run", { run_id = run_id })
  invoke_tool("waiter-a", "await-run", { run_id = run_id })
  assert_eq(#decode_calls(), 0, "active await-run retains firing with no immediate result")
  assert_eq(lw._internals.run_registry.waiter_runs["waiter-a"], run_id,
    "waiter correlation is retained without polling")
  local relays = 0
  local original = agentic_loop.relay_run_completion
  agentic_loop.relay_run_completion = function(_) relays = relays + 1 end
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "completed",
    result = { text = "slow output" }, gate_metadata = { source = "gate" } })
  local calls = decode_calls()
  for _, id in ipairs({ "waiter-a", "waiter-b" }) do
    local replies = find_calls(calls, function(c)
      return c.body.kind == "tool.result" and c.body.id == id
    end)
    assert_eq(#replies, 1, id .. " receives exactly one result")
    assert_eq(replies[1].body.output.result.text, "slow output",
      id .. " receives canonical output")
    assert_eq(replies[1].body.output.gate_metadata.source, "gate",
      id .. " receives terminal metadata pass-through")
  end
  assert_eq(relays, 1, "normal asynchronous relay remains independent")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "completed" })
  agentic_loop.relay_run_completion = original
  assert_eq(#decode_calls(), 0, "duplicate terminal event is a total no-op")
  assert_eq(relays, 1, "duplicate terminal event cannot relay twice")

  invoke_tool("already-done", "await-run", { run_id = run_id })
  local immediate = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "already-done"
  end)
  assert_true(immediate ~= nil and immediate.body.output.result.text == "slow output",
    "retained completed run returns immediately")
end

-- Canonical await retention is independent of the legacy graph-status
-- projection. An output_path-only success keeps the old structural terminal
-- result summary, while await returns the complete canonical terminal body.
do
  local run_id = dispatch_awaitable("status-compat")
  local terminal = lw._internals.state.active_runs[run_id].terminal
  _test.calls_clear()
  invoke_tool("status-compat-wait", "await-run", { run_id = run_id })
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "completed",
    output_path = "/tmp/status-compat.txt", metadata = { canonical = true } })
  local waiter = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "status-compat-wait"
  end)
  assert_eq(waiter.body.output.output_path, "/tmp/status-compat.txt",
    "await receives the canonical output_path payload")
  assert_eq(waiter.body.output.metadata.canonical, true,
    "await retains canonical terminal metadata")
  local summary = lw._internals.summarize_run(lw._internals.state.completed_runs[#lw._internals.state.completed_runs])
  assert_eq(summary.result[terminal].output.output_path, "/tmp/status-compat.txt",
    "graph-status keeps the prior structural terminal result projection")
  assert_eq(summary.output_path, nil,
    "graph-status does not leak canonical-only top-level output_path")

  run_id = dispatch_awaitable("status-failure-fallback")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "failed" })
  summary = lw._internals.summarize_run(lw._internals.state.completed_runs[#lw._internals.state.completed_runs])
  assert_eq(summary.error, "mag run failed",
    "failed graph-status summary exposes actionable fallback error")
end

-- Failed and killed terminals preserve typed status/error semantics.
do
  for _, case in ipairs({
    { name = "failed", code = "await_run_failed", error = "worker failed" },
    { name = "killed", code = "await_run_killed", error = "stopped" },
  }) do
    local run_id = dispatch_awaitable("await-" .. case.name)
    invoke_tool("wait-" .. case.name, "await-run", { run_id = run_id })
    _test.calls_clear()
    feed("mag", { kind = "mag.run_result", run_id = run_id, status = case.name,
      error = case.error, metadata = { passthrough = true } })
    local reply = find_call(decode_calls(), function(c)
      return c.body.kind == "tool.result" and c.body.id == "wait-" .. case.name
    end)
    assert_true(reply ~= nil and reply.body.error_code == case.code,
      case.name .. " waiter receives stable typed error")
    assert_eq(reply.body.status, case.name, case.name .. " status is preserved")
    assert_eq(reply.body.terminal.metadata.passthrough, true,
      case.name .. " canonical metadata is preserved")
  end
end

-- Canceling one waiter detaches only it; no kernel control or source result is
-- emitted, the run and other waiters continue.
do
  local run_id = dispatch_awaitable("await-cancel")
  _test.calls_clear()
  invoke_tool("cancel-me", "await-run", { run_id = run_id })
  invoke_tool("keep-me", "await-run", { run_id = run_id })
  feed("tool-gate", { kind = "lead-workflow.tool.cancel", id = "cancel-me" })
  assert_eq(find_call(decode_calls(), function(c)
    return c.body.kind == "mag.kill_run" or c.body.kind == "mag.interrupt_run"
  end), nil, "waiter cancellation never cancels the run")
  assert_true(lw._internals.state.active_runs[run_id] ~= nil,
    "run remains active after waiter cancellation")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "completed",
    result = { text = "after cancel" } })
  local calls = decode_calls()
  assert_eq(find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "cancel-me"
  end), nil, "canceled waiter receives no late source result")
  assert_true(find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "keep-me"
  end) ~= nil, "other waiter receives the terminal result")
end

-- terminate-graph retains a terminating run and its waiter until canonical
-- killed confirmation. Session end instead settles waiters and clears state.
do
  local run_id = dispatch_awaitable("await-terminate")
  _test.calls_clear()
  invoke_tool("termination-waiter", "await-run", { run_id = run_id })
  invoke_tool("termination-request", "terminate-graph", { run_id = run_id })
  assert_eq(lw._internals.state.active_runs[run_id].phase, "terminating",
    "termination marks rather than archives the run")
  assert_eq(find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "termination-waiter"
  end), nil, "waiter stays blocked while terminating")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "killed" })
  local reply = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "termination-waiter"
  end)
  assert_true(reply ~= nil and reply.body.error_code == "await_run_killed",
    "canonical killed result settles terminating waiter")

  run_id = dispatch_awaitable("await-session")
  invoke_tool("session-waiter", "await-run", { run_id = run_id })
  _test.calls_clear()
  lw._internals.terminate_active_graph(sessions.current_id())
  reply = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "session-waiter"
  end)
  assert_true(reply ~= nil and reply.body.error_code == "await_run_session_ended",
    "session end settles waiter with a typed error")
  assert_eq(lw._internals.run_registry.waiter_runs["session-waiter"], nil,
    "session end leaks no waiter correlation")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "killed" })
  assert_eq(#decode_calls(), 0, "late session terminal is ignored")
end

-- Malformed, unknown, wrong-session, and expired handles fail directly.
do
  fresh()
  for _, case in ipairs({
    { id = "malformed", run_id = "bad handle", code = "await_run_malformed" },
    { id = "unknown", run_id = "mag-run-rg-1-2-3", code = "await_run_unknown" },
  }) do
    invoke_tool(case.id, "await-run", { run_id = case.run_id })
    local reply = find_call(decode_calls(), function(c)
      return c.body.kind == "tool.result" and c.body.id == case.id
    end)
    assert_true(reply ~= nil and reply.body.error_code == case.code,
      case.code .. " is returned directly")
    _test.calls_clear()
  end
  local registry = lw._internals.run_registry
  local foreign = registry:register({ run_id = registry:mint_run_id(), run_name = "foreign",
    session_id = "other-session", terminal = "worker" })
  invoke_tool("wrong", "await-run", { run_id = foreign.run_id })
  local wrong = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "wrong"
  end)
  assert_true(wrong ~= nil and wrong.body.error_code == "await_run_wrong_session",
    "wrong-session handle is distinguishable from unknown")

  fresh()
  registry = lw._internals.run_registry
  local retained = {}
  for i = 1, 65 do
    local run = registry:register({ run_id = registry:mint_run_id(), run_name = "boundary-" .. i,
      session_id = sessions.current_id(), terminal = "worker" })
    retained[i] = run.run_id
    registry:settle(run.run_id, { status = "completed", result = { text = tostring(i) } })
  end
  assert_eq(#registry.completed_runs, 64, "production retention keeps exactly 64 terminal outcomes")
  local _, boundary_error = registry:lookup(retained[1], sessions.current_id())
  assert_eq(boundary_error.error_code, "await_run_expired", "the 65th terminal expires exactly the oldest outcome")
  assert_true(registry:get(retained[2]) ~= nil and registry:get(retained[65]) ~= nil,
    "retention boundary preserves outcomes 2 through 65")

  registry.tombstone_limit = 2
  registry:add_tombstone("mag-run-prune-a", sessions.current_id())
  registry:add_tombstone("mag-run-prune-b", sessions.current_id())
  registry:add_tombstone("mag-run-prune-c", sessions.current_id())
  assert_eq(registry.tombstones["mag-run-prune-a"], nil,
    "tombstone retention prunes the oldest ownership marker")
  assert_true(registry.tombstones["mag-run-prune-b"] ~= nil
      and registry.tombstones["mag-run-prune-c"] ~= nil,
    "tombstone pruning retains the newest bounded markers")
  registry.tombstone_limit = 256

  fresh()
  registry = lw._internals.run_registry
  registry.terminal_limit = 1
  local first = registry:register({ run_id = registry:mint_run_id(), run_name = "first",
    session_id = sessions.current_id(), terminal = "worker" })
  registry:settle(first.run_id, { status = "completed", result = { text = "first" } })
  local second = registry:register({ run_id = registry:mint_run_id(), run_name = "second",
    session_id = sessions.current_id(), terminal = "worker" })
  registry:settle(second.run_id, { status = "completed", result = { text = "second" } })
  invoke_tool("expired", "await-run", { run_id = first.run_id })
  local expired = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "expired"
  end)
  assert_true(expired ~= nil and expired.body.error_code == "await_run_expired",
    "displaced terminal outcome leaves an expired tombstone")
  registry.terminal_limit = 64
end
