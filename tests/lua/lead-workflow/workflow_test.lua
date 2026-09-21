-- examples/nefor-agent/lead_workflow_test.lua — unit tests for the lead-workflow
-- actor. Driven from
-- `crates/nefor/tests/starter_lead_workflow_test.rs`. Mirrors the
-- harness pattern in `starter_agentic_workflow_test.rs`.

local lw   = require("libs.lead-workflow")
local json = nefor.json
local agentic_loop = require("libs.agentic-loop")
local sessions = require("libs.sessions")
local sessions_root = nefor.fs.data_root() .. "/sessions"
sessions.configure { root = sessions_root }
require("libs.mag-workspace").configure {
  sessions_root = sessions_root,
}

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

local selected_model_snapshot
local model_snapshot_resolutions

local function fresh()
  lw._internals.reset()
  selected_model_snapshot = { provider = "snapshot-provider", model = "snapshot-model" }
  model_snapshot_resolutions = 0
  lw.configure({
    resolve_model_snapshot = function()
      model_snapshot_resolutions = model_snapshot_resolutions + 1
      return selected_model_snapshot
    end,
  })
  lw._internals.set_grace_scheduler(function(_, callback)
    callback()
    return function() end
  end)
  lw._internals.set_termination_scheduler(function(_, _)
    return function() end
  end)
  agentic_loop._internals.reset()
  sessions._internals.reset_state()
  sessions.init()
  agentic_loop.receive_msg(make_entry("conversation-manager", {
    kind = "conversation.projection.delta",
    conversation_id = "lead-workflow-test-conversation",
    sequence = 1,
    change = {
      kind = "conversation_created",
      conversation = { provenance = { surface = "lead" } },
    },
  }))
  local system_completed = find_call(decode_calls(), function(call)
    return call.body.kind == "conversation.fact.append"
       and type(call.body.fact) == "table"
       and call.body.fact.kind == "message_completed"
       and call.body.fact.conversation_id == "lead-workflow-test-conversation"
  end)
  if system_completed ~= nil then
    agentic_loop.receive_msg(make_entry("conversation-manager", {
      kind = "conversation.projection.delta",
      conversation_id = "lead-workflow-test-conversation",
      sequence = 2,
      change = {
        kind = "message_completed",
        message = {
          id = system_completed.body.fact.message_id,
          role = "system",
          content = {},
        },
      },
    }))
  end
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
  local write_schema, preview_schema, apply_schema, await_schema, graph_status_schema
  for _, schema in ipairs(advertised.body.tools or {}) do
    assert_true(type(schema.display) == "table", schema.name .. " has display metadata")
    if schema.name == "mag-write-file" then write_schema = schema end
    if schema.name == "mag-preview" then preview_schema = schema end
    if schema.name == "mag-apply" then apply_schema = schema end
    if schema.name == "mag-await" then await_schema = schema end
    if schema.name == "mag-status" then graph_status_schema = schema end
  end
  assert_true(write_schema ~= nil, "mag-write-file is advertised")
  assert_true(preview_schema ~= nil, "mag-preview is advertised")
  assert_true(apply_schema ~= nil, "mag-apply is advertised")
  assert_eq(write_schema.parameters.required[1], "file", "write requires file")
  assert_eq(write_schema.parameters.required[2], "new_string", "write requires new_string")
  assert_true(type(write_schema.parameters.properties.old_string) == "table", "write has optional old_string")
  assert_eq(preview_schema.parameters.required[1], "file", "preview requires only file")
  assert_eq(preview_schema.parameters.properties.content, nil, "preview cannot write source")
  assert_true(type(apply_schema.parameters.properties.content) == "table", "apply may create source")
  assert_eq(apply_schema.parameters.properties.run_id, nil, "apply exposes only fresh workflow dispatch")
  assert_true(await_schema ~= nil, "the mag-await schema is advertised")
  assert_true(graph_status_schema ~= nil, "the mag-status schema is advertised")
  assert_true(graph_status_schema.description:find("One-shot snapshot", 1, true) ~= nil
      and graph_status_schema.description:find("not an await or wait mechanism", 1, true) ~= nil
      and graph_status_schema.description:find("Never call it in a polling loop", 1, true) ~= nil,
    "mag-status is described only as a current-state snapshot, not an await")
  assert_true(graph_status_schema.description:find("immediately after dispatch merely to wait", 1, true) ~= nil,
    "mag-status explicitly rejects post-dispatch waiting")
  assert_true(graph_status_schema.description:find("when your next step depends", 1, true) == nil
      and graph_status_schema.description:find("Block until", 1, true) == nil,
    "mag-status carries no dependency-wait affordance")
  assert_eq(await_schema.display.compact.label, "await run", "mag-await has semantic display metadata")
  assert_eq(await_schema.display.compact.primary.select.path, "invocation_label",
    "mag-await prefers the registry-owned presentation label")
  assert_eq(await_schema.display.compact.primary.select.fallback.source, "args",
    "mag-await falls back to its stable raw handle")
  assert_eq(await_schema.display.compact.primary.select.fallback.path, "run_id",
    "mag-await raw fallback is the exact addressed handle")
  assert_true(await_schema.description:find("waits indefinitely", 1, true) ~= nil,
    "mag-await canonically warns about persistent foreground processes")
  for _, schema in ipairs(advertised.body.tools or {}) do
    assert_true(schema.name ~= "mag" and schema.name ~= "mag-eval",
      "legacy overloaded MAG tools are not advertised")
  end
end

-- Current authoring dialect. The lead's validators never parse this source —
-- compilation happens in the mag plugin and the validators run over the
-- modification in the mag.loaded reply — so these strings only document what
-- the lead writes to disk.
local READ_ONLY_MAG = [=[
import core.types.{}
import agents.{}
import nefor.actors.{}
import nefor.artifact.{}
import nefor.contracts.{}
import nefor.graph.{}

type WorkerRequest {prompt: String}
let start = nefor.graph.source("worker-task", WorkerRequest {prompt: "Answer the task."})
let worker = agents.agent<WorkerRequest, nefor.contracts.TextAnswer>("worker", agents.AgentConfig {model: agents.standard, system: "Answer the task.", tools: ["read_file"], tool_approval_policy: named(nefor.contracts.ToolApprovalPolicy, Default, nil)})
let out = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("worker-output")
nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, worker), nefor.graph.edge(worker, out)])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
]=]

local WRITER_MAG = [=[
import core.types.{}
import agents.{}
import nefor.actors.{}
import nefor.artifact.{}
import nefor.contracts.{}
import nefor.graph.{}

type WorkerRequest {prompt: String}
let start = nefor.graph.source("build-task", WorkerRequest {prompt: "Implement feature X."})
let build = agents.agent<WorkerRequest, nefor.contracts.TextAnswer>("build", agents.AgentConfig {model: agents.standard, system: "Implement feature X.", tools: ["read_file", "write_file"], tool_approval_policy: named(nefor.contracts.ToolApprovalPolicy, Default, nil)})
let out = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("build-output")
nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, build), nefor.graph.edge(build, out)])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
]=]

-- Compact concrete-modification fixtures used inside canonical program envelopes.
-- The agent template namespaces its internals under :id (worker.llm etc.);
-- these are trimmed to the actors the validators care about.
local KERNEL_FACTORIES = { "adapter", "llm", "run-tool", "sink", "stub", "tool-result" }

local function factory_contracts(factories)
  local contracts = {}
  for _, name in ipairs(factories or KERNEL_FACTORIES) do
    contracts[#contracts + 1] = { identity = "nefor.factory." .. name }
  end
  return contracts
end

-- Test fixtures stay compact while still crossing the current artifact
-- boundary: qualified capability factories, top-level routes, endpoint-addressed
-- messages, and a structural result selector.
local function actor_endpoint(id)
  return { constructor = "ActorEndpoint", value = { id = id } }
end

local function actor_port(id, semantic_type, wire)
  return { endpoint = actor_endpoint(id), type = semantic_type or {},
    type_id = type(semantic_type) == "string" and semantic_type or "test-type", wire = wire }
end

local function artifact_from_modification(modification)
  local input_ports, output_ports = {}, {}
  local function endpoint_id(port)
    return port and port.endpoint and port.endpoint.value and port.endpoint.value.id
  end
  for _, route in ipairs(modification.routes or {}) do
    input_ports[endpoint_id(route.to)] = route.to
    local id = endpoint_id(route.from)
    output_ports[id] = output_ports[id] or {}
    output_ports[id][#output_ports[id] + 1] = route.from
  end
  local result_port = modification.result and modification.result.from
  if result_port then
    local id = endpoint_id(result_port)
    output_ports[id] = output_ports[id] or {}
    output_ports[id][#output_ports[id] + 1] = result_port
  end
  local actors = {}
  for _, actor in ipairs(modification.actors or {}) do
    actors[#actors + 1] = {
      id = actor.id, factory = "nefor.factory." .. tostring(actor.factory),
      type_arguments = actor.type_arguments or {},
      params = { ["$mag"] = "packed-value", value = actor.params or {} },
      input = input_ports[actor.id] or actor_port(actor.id, {}, "input"),
      outputs = output_ports[actor.id] or {},
    }
  end
  local messages = {}
  for _, message in ipairs(modification.messages or {}) do
    local to = type(message.to) == "table" and message.to or actor_port(message.to,
      message.semantic_type, type(message.content) == "table" and message.content.kind or "input")
    messages[#messages + 1] = {
      to = to, transforms = message.transforms or {},
      semantic_type = message.semantic_type or to.type,
      semantic_type_id = message.semantic_type_id or to.type_id,
      content = { ["$mag"] = "packed-value", value = message.content },
    }
  end
  local result = modification.result and modification.result.from
  return { types = modification.types or {}, actors = actors,
    routes = modification.routes or {},
    messages = messages, nodes = modification.nodes or {}, kills = modification.kills or {},
    result = result and { from = {
      type = result.type, type_id = result.type_id,
      leaves = { { port = result, steps = {} } }, through = {},
    } } or nil }
end

local function envelope_from_modification(modification)
  return { format = "nefor.mag", version = 4, kind = "program",
    program = { initial = artifact_from_modification(modification), operations = {} } }
end

local function read_only_modification()
  return {
    actors = {
      { id = "worker.entry", factory = "adapter", params = { seed = "provider-in" } },
      { id = "worker.llm", factory = "llm", params = {
        system = "Answer the task.", provider = "chatgpt", model = "gpt-5.6-sol",
        reasoning_effort = "medium", tools = { "read_file" },
      } },
    },
    routes = { { id = "entry/llm",
      from = actor_port("worker.entry", "generic-provider.ProviderOut", "generic-provider.ProviderOut"),
      to = actor_port("worker.llm", "generic-provider.ProviderOut", "generic-provider.ProviderOut"),
      transforms = {},
    } },
    messages = { { to = "worker.entry", content = {
      kind = "nefor.agent.Input", value = { prompt = "<initial task text>" },
    } } },
    kills = {},
    result = { from = actor_port("worker.llm", "generic-provider.TextAnswer", "generic-provider.TextAnswer") },
  }
end

local function writer_modification()
  return {
    actors = { { id = "build.llm", factory = "llm", params = {
      system = "Implement feature X.", provider = "chatgpt", model = "gpt-5.6-luna",
      reasoning_effort = "low", tools = { "read_file", "write_file" },
    } } },
    routes = {},
    messages = { { to = "build.llm", content = { kind = "task", prompt = "<initial task text>" } } },
    kills = {},
    result = { from = actor_port("build.llm", "generic-provider.TextAnswer", "generic-provider.TextAnswer") },
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

local function tool_result(id)
  return find_call(decode_calls(), function(call)
    return call.body.kind == "tool.result" and call.body.id == id
  end)
end

local function table_size(values)
  local size = 0
  for _ in pairs(values) do size = size + 1 end
  return size
end

-- mag-status throttles repeated snapshots of the same target without
-- serializing inspection of distinct concurrent runs or the all-runs view.
do
  fresh()
  lw._internals.set_graph_status_now(function() return 100 end)

  invoke_tool("status-a-first", "mag-status", { run_id = "A" })
  invoke_tool("status-b", "mag-status", { run_id = "B" })
  invoke_tool("status-a-repeat", "mag-status", { run_id = "A" })

  assert_true(tool_result("status-a-first").body.output ~= nil,
    "first status query for A is allowed")
  assert_true(tool_result("status-b").body.output ~= nil,
    "status query for B is independent of A")
  local repeated = tool_result("status-a-repeat")
  assert_true(repeated.body.error ~= nil,
    "repeated status query for A inside the cooldown is blocked")
  assert_true(string.find(repeated.body.error, "run_id A", 1, true) ~= nil,
    "anti-polling diagnostic identifies the blocked run")
end

do
  fresh()
  lw._internals.set_graph_status_now(function() return 100 end)

  invoke_tool("status-all-first", "mag-status", {})
  invoke_tool("status-a-after-all", "mag-status", { run_id = "A" })
  assert_true(tool_result("status-all-first").body.output ~= nil,
    "all-runs status query is allowed")
  assert_true(tool_result("status-a-after-all").body.output ~= nil,
    "per-run query is independent of all-runs query")

  fresh()
  lw._internals.set_graph_status_now(function() return 100 end)
  invoke_tool("status-a-before-all", "mag-status", { run_id = "A" })
  invoke_tool("status-all-after-a", "mag-status", {})
  assert_true(tool_result("status-a-before-all").body.output ~= nil,
    "per-run status query is allowed")
  assert_true(tool_result("status-all-after-a").body.output ~= nil,
    "all-runs query is independent of per-run query")
end

do
  fresh()
  local now = 100
  lw._internals.set_graph_status_now(function() return now end)
  invoke_tool("status-expiry-first", "mag-status", { run_id = "expired-run" })
  now = 159
  invoke_tool("status-expiry-blocked", "mag-status", { run_id = "expired-run" })
  now = 160
  invoke_tool("status-expiry-allowed", "mag-status", { run_id = "expired-run" })
  assert_true(tool_result("status-expiry-blocked").body.error ~= nil,
    "same target remains blocked before 60 seconds")
  assert_true(tool_result("status-expiry-allowed").body.output ~= nil,
    "same target is allowed at cooldown expiry")
end

do
  fresh()
  lw._internals.set_graph_status_now(function() return 100 end)
  local registry = lw._internals.run_registry
  local alpha = registry:register({
    run_id = "mag-run-status-alpha",
    run_name = "alpha",
    session_id = sessions.current_id(),
    terminal = "worker",
  })
  alpha.invocation_label = "Inspect alpha"
  local beta = registry:register({
    run_id = "mag-run-status-beta",
    run_name = "beta",
    session_id = sessions.current_id(),
    terminal = "worker",
  })
  beta.invocation_label = "Inspect beta"

  invoke_tool("gate-status-alpha", "mag-status", { run_id = alpha.run_id })
  invoke_tool("gate-status-beta", "mag-status", { run_id = beta.run_id })
  assert_eq(tool_result("gate-status-alpha").body.output.invocation_label, "Inspect alpha",
    "known mag-status returns the registry-owned invocation label")
  assert_eq(tool_result("gate-status-alpha").body.output.run_id, alpha.run_id,
    "first mag-status result retains exact run identity")
  assert_eq(tool_result("gate-status-beta").body.output.invocation_label, "Inspect beta",
    "concurrent mag-status results do not cross-label runs")
  assert_eq(tool_result("gate-status-beta").body.output.run_id, beta.run_id,
    "second mag-status result retains exact run identity")

  _test.calls_clear()
  invoke_tool("gate-status-raw", "mag-status", { run_id = "mag-run-not-known" })
  local unknown = tool_result("gate-status-raw").body.output
  assert_eq(unknown.run_id, "mag-run-not-known",
    "unknown mag-status run ids retain the raw run identity")
  assert_eq(unknown.invocation_label, nil,
    "unknown mag-status run ids cannot acquire a trusted label")
  assert_eq(find_call(decode_calls(), function(call)
    return call.body.kind == "chat.tool.display_primary"
  end), nil, "run-aware labels require no invocation-id display side channel")
end

do
  fresh()
  lw._internals.set_graph_status_now(function() return 100 end)
  local completed = lw._internals.run_registry:register({
    run_id = "mag-run-completed-A",
    run_name = "completed A",
    session_id = sessions.current_id(),
    terminal = "worker",
  })
  lw._internals.run_registry:settle(completed.run_id,
    { status = "completed", result = { text = "done" } })

  invoke_tool("status-completed-first", "mag-status", { run_id = completed.run_id })
  invoke_tool("status-completed-repeat", "mag-status", { run_id = completed.run_id })
  assert_eq(tool_result("status-completed-first").body.output.run.status, "completed",
    "completed runs retain normal mag-status semantics")
  assert_true(tool_result("status-completed-repeat").body.error ~= nil,
    "completed run ids use the same target-key cooldown")
end

do
  fresh()
  local now = 100
  lw._internals.set_graph_status_now(function() return now end)
  local limit = lw._internals.graph_status_cooldown_limit
  invoke_tool("status-unknown-first", "mag-status", { run_id = "mag-run-missing" })
  invoke_tool("status-unknown-repeat", "mag-status", { run_id = "mag-run-missing" })
  assert_eq(tool_result("status-unknown-first").body.output.status, "await_run_unknown",
    "unknown run ids retain authority-aware mag-status semantics")
  assert_true(tool_result("status-unknown-repeat").body.error ~= nil,
    "unknown run ids use the same target-key cooldown")

  for index = 1, limit + 1 do
    invoke_tool("status-unknown-" .. index, "mag-status",
      { run_id = string.format("mag-run-unknown-%03d", index) })
    local result = tool_result("status-unknown-" .. index)
    assert_eq(result.body.output.status, "await_run_unknown",
      "unknown run ids retain authority-aware mag-status semantics")
  end
  assert_eq(table_size(lw._internals.state.graph_status_cooldowns), limit,
    "arbitrary unknown ids cannot grow cooldown state beyond its bound")
  assert_eq(lw._internals.state.graph_status_cooldowns["run:mag-run-unknown-001"], nil,
    "bounded cooldown state evicts the oldest target")
  assert_true(lw._internals.state.graph_status_cooldowns[
    "run:" .. string.format("mag-run-unknown-%03d", limit + 1)] ~= nil,
    "bounded cooldown state retains the newest target")

  now = 160
  lw._internals.set_graph_status_now(function() return now end)
  invoke_tool("status-after-prune", "mag-status", { run_id = "mag-run-after-prune" })
  assert_eq(table_size(lw._internals.state.graph_status_cooldowns), 1,
    "expired cooldown entries are pruned before a new target is recorded")
end

do
  fresh()
  lw._internals.set_graph_status_now(function() return 100 end)
  invoke_tool("status-before-reset", "mag-status", { run_id = "reset-run" })
  lw._internals.reset()
  lw._internals.set_graph_status_now(function() return 100 end)
  _test.calls_clear()
  invoke_tool("status-after-reset", "mag-status", { run_id = "reset-run" })
  assert_true(tool_result("status-after-reset").body.output ~= nil,
    "lead-workflow reset clears mag-status cooldown state")
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

local function invocation(session_id, principal, capability_id, actor_id, run_id)
  capability_id = capability_id or "r-provenance/cap-1"
  return {
    session_id = session_id,
    run_id = run_id or "run-provenance",
    run_scope = capability_id:match("^([^/]+)/") or "r-provenance",
    actor_id = actor_id or (principal == "lead" and "lead.run-tool" or "worker.run-tool"),
    capability_id = capability_id,
    principal = principal,
    conversation_id = (actor_id or (principal == "lead" and "lead.run-tool" or "worker.run-tool"))
      .. ":conversation",
  }
end

local function write_mag_file(id, file, content)
  invoke_tool(id, "mag-write-file", {
    file = file,
    new_string = content,
  })
  local reply = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == id
  end)
  assert_true(reply ~= nil and reply.body.output and reply.body.output.operation == "created",
    "mag write must create " .. file .. "; got " .. json.encode(_test.calls()))
end

local function execute_mag(id, file)
  invoke_tool(id, "mag-apply", {
    file = file,
  })
end

-- Drive the load handshake and return the load call.
local function feed_loaded(modification, factories)
  local load = find_call(decode_calls(), function(c)
    return c.body.kind == "mag.load" and c.target == "mag"
  end)
  assert_true(load ~= nil,
    "mag compile/apply must emit mag.load to the mag plugin; got "
    .. json.encode(_test.calls()))
  local reply = {
    kind        = "mag.loaded",
    in_reply_to = load.body.id,
    hash        = "sha256:test",
    factories   = factories or KERNEL_FACTORIES,
    factory_contracts = factory_contracts(factories),
    artifact = envelope_from_modification(modification),
  }
  feed("mag", reply)
  return load
end

-- Integration regression for the production mag.loaded boundary: the same
-- factory-shaped reply drives registry capture, preview rendering, default
-- overlay resolution, execute forwarding, and active-run metadata.
do
  fresh()
  local mag_root = _repo_root .. "/mag"
  local context = require("libs.mag-context").new {
    guides = {
      { title = "MAG in Five Minutes", path = mag_root .. "/book/01. core/00. MAG in Five Minutes.md" },
      { title = "Nefor MAG in Five Minutes", path = mag_root .. "/book/02. nefor/00. Nefor MAG in Five Minutes.md" },
    },
    book_path = mag_root .. "/book/README.md",
    module_roots = {
      { name = "nefor-mag", path = mag_root .. "/lib" },
      { name = "config", path = _repo_root .. "/examples/nefor-agent/mag/lib" },
    },
    trailing_sections = { "NESTED TRAILING CONTEXT" },
  }
  lw.configure({
    ambient_context = context,
    agent_system = "universal composed prompt",
  })
  write_mag_file("system-overlay-write", "system-overlay.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("system-overlay-execute", "system-overlay.mag")
  local modification = read_only_modification()
  modification.actors[2].factory = "structured-output"
  local loaded_artifact = artifact_from_modification(modification)
  feed_loaded(modification,
    { "adapter", "structured-output", "run-tool", "sink", "stub", "tool-result" })
  local exec = find_call(decode_calls(), function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
  end)
  assert_true(exec ~= nil, "configured universal system permits execution")
  assert_eq(loaded_artifact.actors[2].factory,
    "nefor.factory.structured-output", "execute preserves the plugin artifact factory")
  assert_true(type(loaded_artifact.actors[2].type_arguments) == "table",
    "execute preserves the plugin artifact type arguments")
  assert_true(lw._internals.state.kernel_factories["nefor.factory.structured-output"] == true,
    "factory contracts from the plugin reply feed control-plane validation")
  local patch = exec.body.params_overlay["actor:10:worker.llm"]
  assert_eq(patch.provider, nil, "runtime does not override the authored provider")
  assert_eq(patch.model, nil, "runtime does not override the authored model")
  assert_eq(patch.reasoning_effort, nil,
    "runtime does not override the authored reasoning effort")
  local system = patch.system
  local base_at = assert(system:find("universal composed prompt", 1, true))
  local position_at = assert(system:find("Answer the task.", 1, true))
  local reasoner_at = assert(system:find("# Reasoner mental model", 1, true))
  local core_at = assert(system:find("# MAG in Five Minutes", 1, true))
  local nefor_at = assert(system:find("# Nefor MAG in Five Minutes", 1, true))
  local book_at = assert(system:find("Full MAG Book:", 1, true))
  local inventory_at = assert(system:find("Available MAG modules:", 1, true))
  local trailing_at = assert(system:find("NESTED TRAILING CONTEXT", 1, true))
  assert_true(base_at < position_at and position_at < reasoner_at
      and reasoner_at < core_at and core_at < nefor_at
      and nefor_at < book_at and book_at < inventory_at and inventory_at < trailing_at,
    "nested agents receive authored system first, then reasoner model, both guides, references, inventory, and trailing context")
  local _, reasoner_titles = system:gsub("# Reasoner mental model", "")
  assert_eq(reasoner_titles, 1, "nested reasoner mental model is injected exactly once")
  assert_true(system:find(mag_root .. "/book/README.md", 1, true) ~= nil,
    "nested agents receive the resolved full-book path")
  assert_true(system:find("nefor.graph", 1, true) ~= nil,
    "nested agents receive the canonical module inventory")
  local run = lw._internals.state.active_runs[exec.body.run_id]
  assert_eq(run.nodes["worker.llm"].reasoner, "nefor.factory.structured-output",
    "run metadata uses the artifact factory identity")
  local preview = require("libs.mag-workspace").preview(
    envelope_from_modification(modification), "sha256:test", KERNEL_FACTORIES)
  assert_true(preview:find("worker.llm (nefor.factory.structured-output)", 1, true) ~= nil,
    "preview renders the same artifact factory identity")
  assert_true(preview:find("route[1]", 1, true) ~= nil
      and preview:find("transforms: (identity)", 1, true) ~= nil,
    "preview renders ordered route transforms")
  assert_true(preview:find("Result boundary: generic-provider.TextAnswer", 1, true) ~= nil,
    "preview renders the StoredBoundary result meaningfully")
end

-- Preview and control decoding remove exactly the explicit compiler wrapper;
-- an authored record that resembles a semantic sum stays ordinary user data.
do
  local authored = { type = "sha256:user-authored", value = { nested = true } }
  local artifact = {
    format = "nefor.mag", version = 4, kind = "program", program = {
      initial = {
        types = {},
        actors = { {
          id = "record", factory = "nefor.factory.stub", type_arguments = {},
          params = { ["$mag"] = "packed-value", value = authored },
        } },
        routes = {},
        messages = { {
          to = actor_port("record", {}, "input"), transforms = {},
          semantic_type = {}, semantic_type_id = "test-type",
          content = { ["$mag"] = "packed-value", value = authored },
        } },
        kills = {}, nodes = {}, result = { from = {
          type = {}, type_id = "test-type",
          leaves = { { port = actor_port("record", {}, "result"), steps = {} } }, through = {},
        } },
      },
      operations = {},
    },
  }
  local workspace = require("libs.mag-workspace")
  local decoded = assert(workspace.decode_artifact(artifact))
  assert_eq(decoded.modification.actors[1].params.type, "sha256:user-authored",
    "Lua decode preserves a user record shaped like a semantic sum")
  assert_eq(decoded.modification.actors[1].params.value.nested, true,
    "Lua decode never recursively collapses the authored record")
  assert_eq(decoded.modification.messages[1].content.type, "sha256:user-authored",
    "message decoding follows the same one-boundary rule")
  local inventory = workspace.actor_inventory(decoded)
  assert_eq(inventory[1].actor.params.type, "sha256:user-authored",
    "control-plane inventory scans the preserved record without collapsing it")
  local preview = workspace.preview(artifact, "sha256:packed-preview", KERNEL_FACTORIES)
  assert_true(preview:find('type: "sha256:user-authored"', 1, true) ~= nil,
    "preview renders the preserved authored record rather than its nested value")
end

do
  local workspace = require("libs.mag-workspace")
  local initial = { types = {}, actors = {}, routes = {}, messages = {}, nodes = {}, kills = {},
    result = { from = { type = {}, type_id = "test-type", leaves = {}, through = {} } } }
  local delta = { types = {}, actors = {}, routes = {}, messages = {}, nodes = {}, kills = {} }
  local _, mixed_program_error = workspace.decode_artifact {
    format = "nefor.mag", version = 4, kind = "program",
    program = { initial = initial, operations = {} }, delta = delta,
  }
  assert_true(mixed_program_error:find("unknown field delta", 1, true) ~= nil,
    "program envelope rejects a delta sibling")
  local _, mixed_delta_error = workspace.decode_artifact {
    format = "nefor.mag", version = 4, kind = "delta", delta = delta,
    program = { initial = initial, operations = {} },
  }
  assert_true(mixed_delta_error:find("unknown field program", 1, true) ~= nil,
    "delta envelope rejects a program sibling")
  local _, delta_operations_error = workspace.decode_artifact {
    format = "nefor.mag", version = 4, kind = "delta",
    delta = { types = {}, actors = {}, routes = {}, messages = {}, nodes = {}, kills = {}, operations = {} },
  }
  assert_true(delta_operations_error:find("unknown field operations", 1, true) ~= nil,
    "delta payload rejects operation residue")
  local _, operation_error = workspace.decode_artifact {
    format = "nefor.mag", version = 4, kind = "program",
    program = { initial = initial, operations = { { extra = true } } },
  }
  assert_true(operation_error:find("unknown field extra", 1, true) ~= nil,
    "program operation rejects unknown fields")
end

-- Template payloads are a sum outside the packed-value boundary. Only Static
-- crosses that boundary; Expression remains an unevaluated reference.
do
  local workspace = require("libs.mag-workspace")
  local authored = { constructor = "Expression", value = { ["$mag"] = "packed-value", value = 42 } }
  local function artifact_for(payload)
    return {
      format = "nefor.mag", version = 4, kind = "program", program = {
        initial = { actors = {}, routes = {}, messages = {}, nodes = {}, kills = {}, types = {},
          result = { from = { type = {}, type_id = "test-type", leaves = {}, through = {} } } },
        operations = { {
          id = "expand", on = actor_port("source", {}, "result"),
          captures = {}, expressions = {},
          template = { actors = {}, messages = { {
            to = { endpoint = { constructor = "LocalActorRef", value = { slot = "worker" } }, wire = "input" },
            transforms = {}, content = payload,
          } } },
        } },
      },
    }
  end
  for _, payload in ipairs({
    { constructor = "Static", value = { ["$mag"] = "packed-value", value = authored } },
    { constructor = "Static", value = { ["$mag"] = "packed-value", value = false } },
    { constructor = "Expression", value = "trigger.expression" },
  }) do
    local artifact = artifact_for(payload)
    local before = nefor.json.encode(artifact)
    local decoded = assert(workspace.decode_artifact(artifact))
    local content = decoded.operations[1].template.messages[1].content
    assert_eq(content.constructor, payload.constructor, "template constructor survives decoding")
    if payload.constructor == "Expression" then
      assert_eq(content.value, "trigger.expression", "expression is not unpacked or evaluated")
    elseif payload.value.value == false then
      assert_eq(content.value, false, "false is a valid packed semantic value")
    else
      assert_eq(content.value.constructor, "Expression", "Static user data is not decoded recursively")
      assert_eq(content.value.value["$mag"], "packed-value", "nested user envelope remains user data")
    end
    assert_true(workspace.preview(artifact, "sha256:template", KERNEL_FACTORIES)
      :find("message[1] -> slot:worker/input", 1, true) ~= nil, "preview accepts both template payload variants")
    assert_eq(nefor.json.encode(artifact), before, "decode and preview preserve the immutable artifact")
  end
  for _, payload in ipairs({
    false, "not-a-payload", {}, { constructor = "Unknown", value = "x" },
    { constructor = "Static" }, { constructor = "Expression" },
    { constructor = "Expression", value = 42 },
    { constructor = "Expression", value = "x", extra = true },
    { constructor = "Static", value = authored },
  }) do
    local decoded, err = workspace.decode_artifact(artifact_for(payload))
    assert_eq(decoded, nil, "malformed template payload is rejected")
    assert_true(err:find("program.operations[1].template.messages[1].content", 1, true) ~= nil,
      "template failure includes indexed occurrence context")
  end
  local delta = { format = "nefor.mag", version = 4, kind = "delta", delta = {
    actors = {}, routes = {}, messages = { {
      to = actor_port("worker", {}, "input"), transforms = {},
      semantic_type = {}, semantic_type_id = "test-type",
      content = { ["$mag"] = "packed-value", value = authored },
    } }, nodes = {}, kills = {}, types = {},
  } }
  local decoded = assert(workspace.decode_artifact(delta))
  assert_eq(decoded.modification.messages[1].content.constructor, "Expression",
    "ordinary delta payload stays data, not a template expression")
end

-- Template definitions participate in validation and overlays, but mag-status
-- contains only concrete runtime actors. Each materialization enters through
-- its own lifecycle identity.
do
  fresh()
  lw.configure({ agent_system = "ambient template system" })
  write_mag_file("template-status-write", "template-status.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("template-status-execute", "template-status.mag")
  local load = find_call(decode_calls(), function(c)
    return c.body.kind == "mag.load" and c.target == "mag"
  end)
  local artifact = {
    format = "nefor.mag", version = 4, kind = "program", program = {
      initial = {
        types = {},
        actors = { {
          id = "source", factory = "nefor.factory.stub", type_arguments = {},
          params = { ["$mag"] = "packed-value", value = {} },
        } },
        routes = {}, messages = {}, kills = {}, nodes = {},
        result = { from = {
          type = {}, type_id = "test-type",
          leaves = { { port = actor_port("source", {}, "result"), steps = {} } },
          through = {},
        } },
      },
      operations = { {
        id = "expand", on = actor_port("source", {}, "result"),
        captures = {}, expressions = {}, template = {
          actors = { {
            slot = "worker", factory = "nefor.factory.llm", type_arguments = {},
            params = { ["$mag"] = "packed-value", value = {
              system = "template authored system", tools = { "read_file" },
            } },
          } },
          messages = {
            { to = { endpoint = { constructor = "LocalActorRef", value = { slot = "worker" } }, wire = "input" },
              content = { constructor = "Expression", value = "worker-input" } },
            { to = { endpoint = { constructor = "LocalActorRef", value = { slot = "worker" } }, wire = "input" },
              content = { constructor = "Static", value = { ["$mag"] = "packed-value", value = {} } } },
          }, kills = {}, nodes = {},
        },
      } },
    },
  }
  feed("mag", {
    kind = "mag.loaded", in_reply_to = load.body.id, hash = "sha256:template-status",
    factories = KERNEL_FACTORIES, factory_contracts = factory_contracts(), artifact = artifact,
  })
  local exec = find_call(decode_calls(), function(c) return c.body.kind == "mag.execute" end)
  assert_true(exec ~= nil, "template program passes definition-time validation")
  assert_true(exec.body.params_overlay["operation:6:expand:template:6:worker"].system
      :find("ambient template system", 1, true) ~= nil,
    "template definitions receive the same agent-system overlay as initial actors")
  local run = lw._internals.state.active_runs[exec.body.run_id]
  assert_eq(#run.nodes_order, 1, "template definitions are not registered as runtime nodes")
  assert_eq(run.nodes_order[1], "source", "initial concrete actor is registered by runtime id")

  feed("mag", { kind = "mag.actor_spawned", run_id = exec.body.run_id,
    id = "expand.worker.0", factory = "nefor.factory.llm" })
  feed("mag", { kind = "mag.actor_spawned", run_id = exec.body.run_id,
    id = "expand.worker.1", factory = "nefor.factory.llm" })
  assert_true(run.nodes["expand.worker.0"] ~= nil and run.nodes["expand.worker.1"] ~= nil,
    "separate materializations become distinct runtime nodes")
  assert_eq(#run.nodes_order, 3,
    "one definition produces only its concrete materialized instances in bookkeeping")

  lw._internals.set_graph_status_now(function() return 100 end)
  _test.calls_clear()
  invoke_tool("template-status", "mag-status", { run_id = exec.body.run_id })
  local nodes = tool_result("template-status").body.output.run.nodes
  assert_eq(#nodes, 3, "mag-status excludes the template definition")
  assert_eq(nodes[2].id, "expand.worker.0", "first materialization keeps its runtime identity")
  assert_eq(nodes[3].id, "expand.worker.1", "second materialization keeps its runtime identity")
end

-- ------------------------------------------------------------------
-- dependency module roots
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
  local workspace = require("libs.mag-workspace").workspace_dir(sessions.current_id())
  assert_true(nefor.fs.exists(workspace),
    "session workspace keeps a writable source directory")
  assert_true(not nefor.fs.exists(workspace .. "/lib"),
    "session workspace does not create a redundant local library directory")
  assert_true(not nefor.fs.exists(workspace .. "/book/README.md"),
    "the MAG Book is not copied into the session")
  _test.calls_clear()
  execute_mag("roots-default-execute", "roots-default.mag")
  local default_load = latest_mag_load()
  assert_true(default_load ~= nil, "default mag execution emits mag.load")
  assert_eq(#default_load.body.module_roots, 1,
    "omitted dependency roots preserve the single workspace root")
  assert_eq(default_load.body.module_roots[1], workspace,
    "the default root is the writable source workspace")

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
  assert_true(normal_load.body.module_roots[3]:match("/mag$") ~= nil,
    "normal mag places the workspace source root last")

  -- Mutating an emitted envelope cannot corrupt the roots held for the next
  -- file compilation.
  normal_load.body.module_roots[1] = "/mutated/envelope"
  _test.calls_clear()
  execute_mag("roots-custom-second", "roots-custom.mag")
  local second_load = latest_mag_load()
  assert_eq(second_load.body.module_roots[1], "/deps/standard",
    "later file compiles receive a defensive root copy")

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

do
  local invalid_snapshots = {
    { label = "scalar", value = "bad" },
    { label = "empty-provider", value = { provider = "", model = "m" } },
    { label = "empty-model", value = { provider = "p", model = "" } },
    { label = "empty-effort", value = { provider = "p", model = "m", reasoning_effort = "" } },
    { label = "scalar-provider-options", value = {
      provider = "p", model = "m", provider_options = "fast",
    } },
    { label = "array-provider-options", value = {
      provider = "p", model = "m", provider_options = { "fast" },
    } },
    { label = "scalar-profiles", value = { provider = "p", model = "m", profiles = "bad" } },
    { label = "empty-profile-name", value = {
      provider = "p", model = "m", profiles = { [""] = { provider = "p", model = "m" } },
    } },
    { label = "invalid-profile", value = {
      provider = "p", model = "m", profiles = { fast = { provider = "", model = "m" } },
    } },
    { label = "unknown-profile-field", value = {
      provider = "p", model = "m",
      profiles = { fast = { provider = "p", model = "m", extra = true } },
    } },
    { label = "unknown-field", value = { provider = "p", model = "m", extra = true } },
  }
  for _, case in ipairs(invalid_snapshots) do
    fresh()
    lw.configure({ resolve_model_snapshot = function() return case.value end })
    write_mag_file("invalid-snapshot-write-" .. case.label, "invalid-snapshot.mag", READ_ONLY_MAG)
    _test.calls_clear()
    execute_mag("invalid-snapshot-exec-" .. case.label, "invalid-snapshot.mag")
    feed_loaded(read_only_modification())
    assert_eq(find_call(decode_calls(), function(call)
      return call.body.kind == "mag.execute"
    end), nil, case.label .. " model snapshot fails before mag.execute")
    local rejected = tool_result("invalid-snapshot-exec-" .. case.label)
    assert_true(rejected ~= nil and type(rejected.body.error) == "string"
        and rejected.body.error:find("invalid model snapshot", 1, true) ~= nil,
      case.label .. " model snapshot reports a validation error")
  end
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
-- Fresh mag apply: the load handshake — mag.load first, internal mag.execute only
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
    "fresh mag apply emits mag.load to the plugin; got " .. json.encode(_test.calls()))
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

  selected_model_snapshot = {
    provider = "snapshot-provider-b", model = "snapshot-model-b", reasoning_effort = "high",
    profiles = {
      fast = { provider = "fast-provider", model = "fast-model" },
    },
  }
  _test.calls_clear()
  feed("mag", {
    kind        = "mag.loaded",
    in_reply_to = load.body.id,
    hash        = "sha256:read-only",
    factories   = KERNEL_FACTORIES,
    factory_contracts = factory_contracts(KERNEL_FACTORIES),
    artifact = envelope_from_modification(read_only_modification()),
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
    "fresh mag apply declares the subagent domain principal")
  assert_eq(exec.body.model_snapshot.provider, "snapshot-provider-b",
    "fresh execution samples the acknowledged provider after loading finishes")
  assert_eq(exec.body.model_snapshot.model, "snapshot-model-b",
    "fresh execution samples the acknowledged model after loading finishes")
  assert_eq(exec.body.model_snapshot.reasoning_effort, "high",
    "fresh execution forwards explicit acknowledged effort")
  assert_eq(exec.body.model_snapshot.profiles.fast.provider, "fast-provider",
    "fresh execution forwards config-resolved model profiles")
  assert_eq(exec.body.model_snapshot.profiles.fast.model, "fast-model",
    "fresh execution preserves the resolved profile model")
  selected_model_snapshot.profiles.fast.provider = "mutated-provider"
  assert_eq(exec.body.model_snapshot.profiles.fast.provider, "fast-provider",
    "fresh execution owns a deep copy of the profile snapshot")
  assert_eq(model_snapshot_resolutions, 1,
    "fresh execution resolves its model snapshot exactly once after validation")

  assert_eq(exec.body.params_overlay, nil,
    "without ambient system context the runtime emits no parameter overlay")
  assert_eq(exec.body.artifact.kind, "program",
    "mag.execute carries the compiled immutable program inline")
  local loaded_artifact = artifact_from_modification(read_only_modification())
  assert_eq(loaded_artifact.actors[2].params.value.provider, "chatgpt",
    "the compiled artifact carries the concrete provider")
  assert_eq(loaded_artifact.actors[2].params.value.model, "gpt-5.6-sol",
    "the compiled artifact carries the concrete model")
  assert_eq(loaded_artifact.actors[2].params.value.reasoning_effort, "medium",
    "the compiled artifact carries the concrete reasoning effort")

  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-mag-execute-1"
  end)
  assert_true(reply ~= nil and reply.body.output ~= nil, "execute replies executing")
  assert_eq(reply.body.output.status, "executing", "reply reports the program is executing")
  assert_eq(reply.body.output.engine, "mag-kernel", "reply reports the kernel engine")
  assert_eq(reply.body.output.hash, "sha256:read-only", "reply carries the program hash")
  assert_eq(exec.body.run_id, reply.body.output.run_id,
    "mag.execute run_id matches the reply run_id")
  assert_eq(reply.body.output.run_name, "auth-login-map",
    "execute acknowledgment prefers the readable program name")

  -- Active run tracks the structural result actor from artifact metadata.
  local run = lw._internals.state.active_runs[reply.body.output.run_id]
  assert_true(type(run) == "table", "active_runs contains the dispatched run_id")
  assert_eq(run.terminal, "result", "the intentional result boundary is terminal")
  _test.calls_clear()
  invoke_tool("firing-mag-status-actors", "mag-status", { run_id = reply.body.output.run_id })
  local status = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-mag-status-actors"
  end)
  assert_true(status ~= nil, "mag-status returns the active run")
  assert_eq(status.body.output.run.run_name, "auth-login-map",
    "mag-status includes the same readable run name")
  assert_eq(status.body.output.run.run_id, reply.body.output.run_id,
    "mag-status retains the opaque handle for disambiguation")
  local nodes = status.body.output.run.nodes
  assert_eq(nodes[1].id, "worker.entry", "runtime actor ids are preserved in run summaries")
  assert_eq(nodes[2].reasoner, "nefor.factory.llm",
    "qualified factory identity carried under the reasoner key")
end

do
  fresh()
  write_mag_file("snapshot-runs-write", "snapshot-runs.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("snapshot-run-a", "snapshot-runs.mag")
  feed_loaded(read_only_modification())
  local first_exec = find_call(decode_calls(), function(call)
    return call.body.kind == "mag.execute"
  end)
  assert_eq(first_exec.body.model_snapshot.provider, "snapshot-provider")
  assert_eq(first_exec.body.model_snapshot.model, "snapshot-model")
  assert_eq(first_exec.body.model_snapshot.reasoning_effort, nil,
    "absent effort is omitted from the execution snapshot")

  selected_model_snapshot = { provider = "snapshot-provider-c", model = "snapshot-model-c" }
  _test.calls_clear()
  execute_mag("snapshot-run-c", "snapshot-runs.mag")
  feed_loaded(read_only_modification())
  local second_exec = find_call(decode_calls(), function(call)
    return call.body.kind == "mag.execute"
  end)
  assert_eq(second_exec.body.model_snapshot.provider, "snapshot-provider-c",
    "a later run captures the later acknowledged provider")
  assert_eq(second_exec.body.model_snapshot.model, "snapshot-model-c",
    "a later run captures the later acknowledged model")
  assert_eq(first_exec.body.model_snapshot.provider, "snapshot-provider",
    "later selection cannot retarget an already emitted run snapshot")
end

do
  fresh()
  invoke_tool("firing-bad-path", "mag-write-file", {
    file = "../bad.mag",
    new_string = READ_ONLY_MAG,
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

-- Workspace-relative paths cannot escape through a symlink component.
do
  fresh()
  local workspace = require("libs.mag-workspace").workspace_dir(sessions.current_id())
  assert(require("libs.mag-workspace").init_workspace(sessions.current_id()))
  local link = workspace .. "/outside"
  local linked = nefor.fs.symlink("/tmp", link)
  assert_true(linked.ok, "test symlink is created")
  invoke_tool("source-symlink", "mag-write-file", {
    file = "outside/escape.mag", new_string = READ_ONLY_MAG,
  })
  local result = tool_result("source-symlink")
  assert_true(result and result.body.error:find("symlink paths", 1, true) ~= nil,
    "MAG source writes reject symlink escape paths")
end

-- MAG source writes share one strict create/overwrite/exact-edit contract.
do
  fresh()
  invoke_tool("source-create", "mag-write-file", {
    file = "nested/source.mag", new_string = "alpha beta",
  })
  local created = tool_result("source-create").body.output
  assert_eq(created.operation, "created", "first whole-file write creates source")
  invoke_tool("source-overwrite", "mag-write-file", {
    file = "nested/source.mag", new_string = "gamma beta",
  })
  local overwritten = tool_result("source-overwrite").body.output
  assert_eq(overwritten.operation, "overwritten", "later whole-file write overwrites source")
  assert_eq(overwritten.message, "Overwrote the entire file with the provided content.",
    "whole-file overwrite receipt explains the completed action")
  invoke_tool("source-edit", "mag-write-file", {
    file = "nested/source.mag", old_string = " beta", new_string = "",
  })
  local edited = tool_result("source-edit").body.output
  assert_eq(edited.operation, "edited", "old_string selects exact replacement")
  local handle = assert(io.open(edited.source_path, "r"))
  assert_eq(handle:read("*a"), "gamma", "empty new_string deletes the exact match")
  handle:close()
end

-- Inline apply source is create-only: a failed compile remains editable, but
-- a later call cannot silently overwrite the same file.
do
  fresh()
  invoke_tool("inline-create", "mag-apply", {
    file = "inline.mag", content = READ_ONLY_MAG,
  })
  local load = find_call(decode_calls(), function(c) return c.body.kind == "mag.load" end)
  assert_true(load ~= nil, "inline content creates source and starts compilation")
  feed("mag", { kind = "mag.error", in_reply_to = load.body.id,
    message = "invalid source" })
  local failed = tool_result("inline-create")
  assert_true(failed and failed.body.error:find("invalid source", 1, true) ~= nil,
    "inline apply returns the compiler failure")
  assert_eq(find_call(decode_calls(), function(c)
    return c.body.kind == "mag.execute" or c.body.kind == "mag.apply"
  end), nil, "inline compile failure dispatches no work")
  _test.calls_clear()
  invoke_tool("inline-conflict", "mag-apply", {
    file = "inline.mag", content = READ_ONLY_MAG,
  })
  local conflict = tool_result("inline-conflict")
  assert_true(conflict and conflict.body.error:find("already exists", 1, true) ~= nil,
    "inline content refuses to overwrite an existing source")
  assert_true(conflict.body.error:find("mag-write-file", 1, true) ~= nil,
    "inline conflict explains the repair flow")
  assert_eq(find_call(decode_calls(), function(c)
    return c.body.kind == "mag.load" or c.body.kind == "mag.execute" or c.body.kind == "mag.apply"
  end), nil, "source creation failure stops before compilation and dispatch")
end

-- ------------------------------------------------------------------
-- mag compile: mag.load through the plugin, preview rendered from the
-- mag.loaded modification. Compile never executes.
-- ------------------------------------------------------------------

do
  fresh()
  write_mag_file("firing-mag-write-compile", "deterministic-check.mag", READ_ONLY_MAG)
  _test.calls_clear()
  invoke_tool("firing-mag-compile", "mag-preview", {
    file = "deterministic-check.mag",
  })
  feed_loaded(read_only_modification())

  local calls = decode_calls()
  local reply = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "firing-mag-compile"
  end)
  assert_true(reply ~= nil, "mag compile returns a tool.result")
  local preview = reply.body.output.workflow_tree
  assert_true(type(preview) == "string", "mag preview returns a workflow tree")
  assert_eq(reply.body.output.status, nil, "preview omits redundant status")
  assert_eq(reply.body.output.source_path, nil, "preview omits the known source path")
  local leaked = find_call(calls, function(c)
    return c.body.kind == "mag.execute"
  end)
  assert_eq(leaked, nil, "mag compile previews only and does not send mag.execute")
end

-- A mag.error reply (compile failure) fails the firing with the compiler
-- message instead of leaving it hanging.
do
  fresh()
  write_mag_file("firing-mag-write-badsrc", "broken.mag", "graph nope")
  _test.calls_clear()
  invoke_tool("firing-mag-compile-fail", "mag-preview", {
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

-- A run_id is rejected even when it names a valid live run. A rejected call
-- neither creates source nor dispatches work; removing the field can recover
-- through the normal one-call source-and-execution path.
for _, run_id in ipairs({ "", "orient", "mag-run-live-apply" }) do
  fresh()
  lw._internals.register_active_run("mag-run-live-apply", {}, "terminal", "dispatch-live",
    "live", sessions.current_id())
  _test.calls_clear()
  invoke_tool("apply-with-run-id", "mag-apply", {
    file = "fresh-only.mag", content = READ_ONLY_MAG, run_id = run_id,
  })
  local rejected = tool_result("apply-with-run-id")
  assert_true(rejected and rejected.body.error:find("run_id is not supported", 1, true) ~= nil,
    "apply rejects every supplied run_id")
  assert_true(rejected.body.error:find("Omit run_id", 1, true) ~= nil,
    "rejection explains how to start the workflow")
  assert_eq(find_call(decode_calls(), function(c)
    return c.body.kind == "mag.load" or c.body.kind == "mag.build"
      or c.body.kind == "mag.execute" or c.body.kind == "mag.apply"
  end), nil, "unsupported live modification cannot compile or dispatch work")

  invoke_tool("apply-without-run-id", "mag-apply", {
    file = "fresh-only.mag", content = READ_ONLY_MAG,
  })
  feed_loaded(read_only_modification())
  local exec = find_call(decode_calls(), function(c) return c.body.kind == "mag.execute" end)
  assert_true(exec ~= nil, "removing run_id creates source and dispatches in one call")
  assert_true(exec.body.run_id ~= "mag-run-live-apply", "dispatch mints a new run handle")
end

local function has_relayed_lead_turn()
  return find_call(decode_calls(), function(c)
    return c.body.kind == "mag.load" and c.body.entry == "agentic-loop/lead-turn.mag"
  end) ~= nil
end

-- ------------------------------------------------------------------
-- File-based MAG application completion grace. Tests drive the deadline
-- callback explicitly (no wall clock).
-- ------------------------------------------------------------------

local function controlled_grace()
  local timers = {}
  lw._internals.set_grace_scheduler(function(delay_ms, callback)
    assert_eq(delay_ms, lw._internals.sync_completion_grace_ms,
      "the named completion grace default is used")
    local timer = { callback = callback, canceled = false }
    timers[#timers + 1] = timer
    return function() timer.canceled = true end
  end)
  return timers
end

local function start_file_run(firing_id, file)
  write_mag_file(firing_id .. "-write", file, READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag(firing_id, file)
  feed_loaded(read_only_modification())
  local exec = find_call(decode_calls(), function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
  end)
  assert_true(exec ~= nil, "file execution reaches the shared run owner")
  return exec.body.run_id
end

do
  fresh()
  local timers = controlled_grace()
  local run_id = start_file_run("grace-fast-success", "grace-fast-success.mag")
  assert_eq(tool_result("grace-fast-success"), nil,
    "execute remains open during the grace window")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "completed",
    result = { value = "fast result" } })
  local result = tool_result("grace-fast-success")
  assert_eq(result.body.output.status, "completed",
    "completion before grace returns the canonical terminal result")
  assert_eq(result.body.output.result.value, "fast result",
    "sync result preserves canonical payload")
  assert_true(timers[1].canceled, "terminal settlement cancels its deadline")
  assert_eq(has_relayed_lead_turn(), false,
    "sync-delivered completion is not relayed as a second lead turn")
end

do
  fresh()
  local timers = controlled_grace()
  local run_id = start_file_run("grace-fast-failure", "grace-fast-failure.mag")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "failed",
    error = "fast boom" })
  local result = tool_result("grace-fast-failure")
  assert_true(result.body.error:find("fast boom", 1, true) ~= nil,
    "failure before grace returns the canonical terminal failure")
  assert_eq(result.body.status, "failed", "terminal failure keeps failed status")
  assert_true(timers[1].canceled, "failed settlement cancels its deadline")
  assert_eq(find_call(decode_calls(), function(c)
    return c.body.kind == "chat.graph_result.append" and c.body.run_id == run_id
  end), nil, "sync mag-apply has no duplicate terminal result projection")
  assert_eq(has_relayed_lead_turn(), false,
    "sync-delivered failure is not relayed a second time")
end

do
  fresh()
  local timers = controlled_grace()
  local run_id = start_file_run("grace-timeout", "grace-timeout.mag")
  _test.calls_clear()
  timers[1].callback()
  local ack = tool_result("grace-timeout")
  assert_eq(ack.body.output.status, "executing",
    "deadline returns the existing async acknowledgment")
  assert_eq(ack.body.output.run_id, run_id, "acknowledgment preserves the exact run id")
  assert_eq(ack.body.output.engine, "mag-kernel", "acknowledgment preserves the engine")
  assert_eq(ack.body.completion_delivery, "async",
    "grace acknowledgment marks completion for later owner delivery")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "completed",
    result = { value = "late result" } })
  assert_true(has_relayed_lead_turn(),
    "completion after timeout follows the normal owner-scoped notification path")
  assert_eq(tool_result("grace-timeout"), nil,
    "late completion does not deliver the tool result twice")
end

do
  fresh()
  local timers = controlled_grace()
  local run_id = start_file_run("grace-timeout-file", "grace-timeout-file.mag")
  _test.calls_clear()
  timers[1].callback()
  local file_ack = tool_result("grace-timeout-file")
  assert_eq(file_ack.body.output.status, "executing",
    "mag-apply identifies the async grace outcome")
  assert_eq(file_ack.body.completion_delivery, "async",
    "mag-apply grace acknowledgment marks async delivery")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "completed",
    duration_ms = 432000, result = { text = "late file result" } })
  local block = find_call(decode_calls(), function(c)
    return c.body.kind == "chat.graph_result.append" and c.body.run_id == run_id
  end)
  assert_true(block ~= nil, "async mag-apply emits the standard terminal result block")
  assert_eq(block.body.invocation_kind, "apply",
    "terminal result keeps the canonical apply invocation kind")
  assert_eq(block.body.invocation_label, "grace-timeout-file.mag",
    "terminal result keeps the canonical source-file label")
  assert_eq(block.body.duration_ms, 432000,
    "terminal result forwards the runtime-owned duration unchanged")
end

do
  fresh()
  local timers = controlled_grace()
  local first = start_file_run("grace-concurrent-a", "grace-concurrent-a.mag")
  _test.calls_clear()
  local second = start_file_run("grace-concurrent-b", "grace-concurrent-b.mag")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = second, status = "completed",
    result = { value = "second" } })
  assert_eq(tool_result("grace-concurrent-a"), nil,
    "a terminal result cannot settle another run's firing")
  assert_eq(tool_result("grace-concurrent-b").body.output.result.value, "second",
    "terminal correlation is strictly by run_id")
  assert_true(timers[2].canceled and not timers[1].canceled,
    "only the matching run's deadline is canceled")
  _test.calls_clear()
  timers[1].callback()
  assert_eq(tool_result("grace-concurrent-a").body.output.run_id, first,
    "the remaining run keeps its own asynchronous acknowledgment")
end

do
  fresh()
  local timers = controlled_grace()
  local run_id = start_file_run("grace-pre-start-failure", "grace-pre-start-failure.mag")
  _test.calls_clear()
  feed("mag", {
    kind = "mag.error",
    in_reply_to = run_id,
    message = "structured-output actor \"worker.llm\": JsonValue at $.last_output has no faithful OpenAI strict structured-output representation; correction: pass only the success output type",
  })
  local result = tool_result("grace-pre-start-failure")
  assert_true(result ~= nil and type(result.body.error) == "string",
    "pre-start mag.error synchronously fails the invoking tool")
  assert_true(result.body.error:find("worker.llm", 1, true) ~= nil
      and result.body.error:find("$.last_output", 1, true) ~= nil
      and result.body.error:find("pass only the success output type", 1, true) ~= nil,
    "pre-start failure preserves the concrete actionable schema error")
  assert_eq(result.body.output, nil,
    "pre-start failure cannot degrade into an executing acknowledgment")
  assert_true(timers[1].canceled,
    "pre-start failure cancels the asynchronous acknowledgment deadline")
  assert_eq(lw._internals.state.active_runs[run_id], nil,
    "pre-start failure leaves no queued active registry entry")
  assert_eq(lw._internals.run_registry:get(run_id).phase, "terminal",
    "pre-start failure is retained only as a canonical failed terminal outcome")
  assert_eq(find_call(decode_calls(), function(c)
    return c.body.kind == "mag.run_started" and c.body.run_id == run_id
  end), nil, "lead-workflow does not fabricate mag.run_started for a rejected run")
  _test.calls_clear()
  timers[1].callback()
  assert_eq(tool_result("grace-pre-start-failure"), nil,
    "a stale grace callback cannot emit status=executing after failure")
  invoke_tool("status-after-pre-start-failure", "mag-status", { run_id = run_id })
  local status = tool_result("status-after-pre-start-failure")
  assert_eq(status.body.output.run.status, "failed",
    "mag-status reports the canonical failure, never a queued ghost")
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
      { id = "lead.source", factory = "source", params = { value = { prompt = "<initial task text>" } } },
      { id = "lead.entry", factory = "adapter", params = { seed = "provider-in" } },
      { id = "lead.llm", factory = "llm", params = {} },
    },
    routes = {
      { id = "source/entry", from = actor_port("lead.source", "nefor.graph.Value", "nefor.graph.Value"),
        to = actor_port("lead.entry", "generic-provider.ProviderOut", "generic-provider.ProviderOut"),
        transforms = {} },
      { id = "entry/llm", from = actor_port("lead.entry", "generic-provider.ProviderOut", "generic-provider.ProviderOut"),
        to = actor_port("lead.llm", "generic-provider.ProviderOut", "generic-provider.ProviderOut"),
        transforms = {} },
    },
    messages = { { to = "lead.source", content = { kind = "mag.Unit" } } }, kills = {},
    result = { from = actor_port("lead.llm", "generic-provider.TextAnswer", "generic-provider.TextAnswer") },
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
      artifact = envelope_from_modification(lead_turn_modification()),
    }))
    calls = decode_calls()
  end
  local exec = find_call(calls, function(c)
    return c.body.kind == "mag.execute" and c.target == "mag"
       and c.body.run_name == "lead"
  end)
  if exec == nil then return nil end
  local source = type(exec.body.params_overlay) == "table"
      and exec.body.params_overlay["actor:11:lead.source"] or nil
  return type(source) == "table" and type(source.value) == "table"
      and source.value.prompt or nil
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
  local oversized_output = string.rep("S", 40000)
  ofh:write(oversized_output)
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
  assert_true(prompt:find(string.rep("S", 1000), 1, true) ~= nil,
    "the relayed turn carries the oversized sink output content")
  assert_eq(#prompt:match("S+"), #oversized_output,
    "the async relay constructs the full canonical Task before model-context projection")
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
  assert_true(block.body.output:find(string.rep("S", 1000), 1, true) == nil,
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
    result    = { from = "worker.llm", kind = "generic-provider.TextAnswer",
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

-- Structural result metadata is required even though result collection is not
-- represented as an actor.
do
  fresh()
  write_mag_file("firing-sink-missing-write", "no-sink.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-sink-missing", "no-sink.mag")
  local m = read_only_modification()
  m.result = nil
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
              and err.body.error:find("requires result", 1, true) ~= nil,
    "missing result boundary is rejected; got " .. json.encode(_test.calls()))
end

do
  fresh()
  write_mag_file("firing-sink-orphan-write", "orphan-sink.mag", READ_ONLY_MAG)
  _test.calls_clear()
  execute_mag("firing-sink-orphan", "orphan-sink.mag")
  local m = read_only_modification()
  m.result = nil -- remove the structural terminal selection
  feed_loaded(m)
  local err = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result"
       and c.body.id == "firing-sink-orphan"
       and type(c.body.error) == "string"
  end)
  assert_true(err ~= nil
              and err.body.error:find("requires result", 1, true) ~= nil,
    "an orphaned result fixture is rejected; got " .. json.encode(_test.calls()))
end

-- mag.loaded snapshots qualified factory identities for validation.
do
  fresh()
  feed("mag", {
    kind      = "mag.loaded",
    factory_contracts = factory_contracts({ "sink", "llm", "stub", "run-tool" }),
  })
  local set = lw._internals.state.kernel_factories
  assert_true(type(set) == "table"
              and set["nefor.factory.sink"] == true
              and set["nefor.factory.llm"] == true,
    "mag.loaded populates the factory registry snapshot")
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
    "write-capable fresh MAG apply without plan must return a tool.result error")
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

-- After /approve, the same writer program is accepted; the system overlay
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
    "after plan approval, write-capable fresh MAG apply must reach the kernel; got "
    .. json.encode(_test.calls()))
  assert_eq(exec.body.params_overlay, nil,
    "writer needs no runtime overlay without ambient system context")
  assert_eq(artifact_from_modification(writer_modification()).actors[1].params.value.reasoning_effort, "low",
    "writer keeps the effort authored in the compiled artifact")
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
-- flushes the approval so the next writer MAG apply is gated again.
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
    "after approval expires, the writer MAG apply is gated again")
end

-- ------------------------------------------------------------------
-- Replay must not synthesize fresh chat.plan.append envelopes from
-- lead-workflow.plan.submitted. The session log already contains the
-- original chat.plan.append in chronological order; regenerating it
-- during replay appends historical plans at the tail on reattach.
-- ------------------------------------------------------------------

do
  fresh()
  local replay_window = require("core.replay_window")
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
  local replay_window = require("core.replay_window")
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
  assert_true(exec ~= nil, "auto bypasses the human plan gate for writer MAG apply")
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
  assert_true(exec ~= nil, "yolo bypasses writer MAG apply approval gate")
end

-- A killed lead run invalidates its parked write-review correlation. A late
-- verdict cannot approve dead work, and the next run can still request approval.
do
  fresh()
  local session_id = sessions.current_id()
  invoke_tool_with_metadata("dead-plan", "write-review",
    { plan = "dead run plan", view = "inline" },
    { invocation = invocation(session_id, "lead", "dead-run/cap-1") })
  local plan = lw._internals.state.active_plan
  assert_eq(plan.pending_firing_id, "dead-plan", "write-review parks the firing")
  assert_eq(plan.pending_correlation, "dead-run/cap-1", "write-review records correlation")
  assert_eq(plan.pending_run_id, "run-provenance", "write-review records owning run")

  feed("mag", { kind = "mag.run_result", run_id = "run-provenance", status = "killed" })
  assert_eq(lw._internals.state.active_plan, nil, "killed owner clears pending approval state")
  _test.calls_clear()
  feed("nefor-tui", { kind = "chat.command", name = "approve", args = "too late" })
  assert_eq(find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "dead-plan"
  end), nil, "late approval for a dead run emits no verdict")

  invoke_tool_with_metadata("cancelled-plan", "write-review",
    { plan = "cancelled plan", view = "inline" },
    { invocation = invocation(session_id, "lead", "cancelled-run/cap-1") })
  feed("tool-gate", { kind = "lead-workflow.tool.cancel", id = "cancelled-plan" })
  assert_eq(lw._internals.state.active_plan, nil,
    "hard cancellation clears the parked write-review state")

  invoke_tool_with_metadata("fresh-plan", "write-review",
    { plan = "fresh run plan", view = "inline" },
    { invocation = invocation(session_id, "lead", "fresh-run/cap-1") })
  assert_eq(lw._internals.state.active_plan.pending_firing_id, "fresh-plan",
    "future write-review remains usable")
  feed("nefor-tui", { kind = "chat.command", name = "approve", args = "ship" })
  assert_true(find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "fresh-plan"
      and c.body.output.status == "approved"
  end) ~= nil, "future approval resolves normally")
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
  assert_true(type(run_id) == "string", "active_runs has an entry after fresh MAG apply")

  -- Also submit a plan that's awaiting approval at session-end.
  feed("tool-gate", {
    kind = "lead-workflow.tool.invoke",
    id   = "firing-plan-at-end",
    name = "write-review",
    args = { plan = "in-flight plan", view = "inline" },
  })
  assert_eq(lw._internals.state.active_plan.status, "pending",
    "plan slot is pending before session_end")
  lw._internals.set_graph_status_now(function() return 100 end)
  invoke_tool("status-at-session-end", "mag-status", { run_id = run_id })
  assert_true(next(lw._internals.state.graph_status_cooldowns) ~= nil,
    "mag-status cooldown exists before session_end")
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
  assert_eq(next(lw._internals.state.graph_status_cooldowns), nil,
    "mag-status cooldown state is cleared at session_end")
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
      { { id = "worker-" .. i, factory = "llm" } }, "worker-" .. i,
      "dispatch-" .. i, "terminated-" .. i, sessions.current_id())
    invoke_tool("wait-terminated-" .. i, "mag-await", { run_id = run_id })
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

-- File-based apply cancellation invalidates by dispatch firing, is idempotent,
-- and makes either kind of late compiler response a silent no-op.
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
  local cancel_calls = decode_calls()
  assert_eq(find_call(cancel_calls, function(c) return c.body.kind == "tool.result" end), nil,
    "canceling an unsubmitted file load emits no source settlement")

  feed("tool-gate", { kind = "lead-workflow.tool.cancel", id = "file-pending-execute" })
  feed("mag", { kind = "mag.loaded", in_reply_to = load.body.id, hash = "sha256:file-late",
    factory_contracts = factory_contracts(),
    artifact = envelope_from_modification(read_only_modification()) })
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
  feed("mag", { kind = "mag.loaded", in_reply_to = load.body.id, hash = "sha256:session-late",
    factory_contracts = factory_contracts(),
    artifact = envelope_from_modification(read_only_modification()) })
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

-- Non-root run control is direct-dispatch authority, not graph ancestry. The
-- same actor may await/status/terminate its detached run; every other actor,
-- including the owning run and descendants, receives the same denial.
do
  fresh()
  local registry = lw._internals.run_registry
  local direct_actor = "parent.run-tool"
  local sibling_actor = "sibling.run-tool"
  local child_actor = "child.run-tool"
  local direct_id = registry:mint_run_id()
  lw._internals.register_active_run(direct_id,
    { { id = "worker", factory = "llm" } }, "worker", "dispatch-direct",
    "direct", sessions.current_id(), direct_actor)
  local sibling_id = registry:mint_run_id()
  lw._internals.register_active_run(sibling_id,
    { { id = "sibling", factory = "llm" } }, "sibling", "dispatch-sibling",
    "sibling", sessions.current_id(), sibling_actor)
  local grandchild_id = registry:mint_run_id()
  lw._internals.register_active_run(grandchild_id,
    { { id = "grandchild", factory = "llm" } }, "grandchild", "dispatch-grandchild",
    "grandchild", sessions.current_id(), child_actor)

  local function metadata_for(actor, owning_run)
    return { invocation = invocation(sessions.current_id(), "subagent",
      "scope/cap-" .. actor:gsub("[^%w]", "-"), actor, owning_run) }
  end
  local direct = metadata_for(direct_actor, "parent-run")
  local sibling = metadata_for(sibling_actor, "sibling-run")
  local child = metadata_for(child_actor, direct_id)

  _test.calls_clear()
  invoke_tool_with_metadata("direct-status", "mag-status", { run_id = direct_id }, direct)
  local reply = find_call(decode_calls(), function(c) return c.body.id == "direct-status" end)
  assert_true(reply ~= nil and reply.body.output.run.run_id == direct_id,
    "subagent may status a detached run it directly dispatched")

  invoke_tool_with_metadata("direct-wait", "mag-await", { run_id = direct_id }, direct)
  assert_eq(registry.waiter_runs["direct-wait"], direct_id,
    "subagent may await a detached run it directly dispatched")
  invoke_tool_with_metadata("direct-kill", "mag-terminate", { run_id = direct_id }, direct)
  assert_true(find_call(decode_calls(), function(c)
    return c.body.kind == "mag.kill_run" and c.body.run_id == direct_id
  end) ~= nil, "subagent may terminate a detached run it directly dispatched")

  local denied = {
    { label = "self", actor = direct_actor, target = direct_id },
    { label = "ancestor", actor = direct_actor, target = "mag-run-ancestor" },
    { label = "sibling", actor = direct_actor, target = sibling_id },
    { label = "grandchild", actor = direct_actor, target = grandchild_id },
    { label = "tool", actor = direct_actor, target = "mag-run-attached-tool" },
    { label = "descendant", actor = child_actor, target = direct_id },
  }
  for _, case in ipairs(denied) do
    local md = metadata_for(case.actor, case.target)
    for _, tool in ipairs({ "mag-await", "mag-status", "mag-terminate" }) do
      _test.calls_clear()
      invoke_tool_with_metadata(case.label .. "-" .. tool, tool,
        { run_id = case.target }, md)
      local result = find_call(decode_calls(), function(c)
        return c.body.kind == "tool.result" and c.body.id == case.label .. "-" .. tool
      end)
      assert_true(result ~= nil, case.label .. " " .. tool .. " fails immediately")
      local code = result.body.error_code
        or (result.body.output and (result.body.output.error_code or result.body.output.status))
      assert_true(code == "run_control_unauthorized" or code == "run_control_self"
          or code == "await_run_unknown",
        case.label .. " " .. tool .. " has a stable structured denial")
    end
  end

  _test.calls_clear()
  invoke_tool_with_metadata("direct-list", "mag-status", {}, direct)
  reply = find_call(decode_calls(), function(c) return c.body.id == "direct-list" end)
  assert_eq(#reply.body.output.active, 1, "unscoped subagent status lists only direct runs")
  assert_eq(reply.body.output.active[1].run_id, direct_id,
    "unscoped filtering excludes sibling and grandchild runs")

  -- Model-supplied caller_id cannot forge the kernel-stamped actor identity.
  _test.calls_clear()
  feed("tool-gate", { kind = "lead-workflow.tool.invoke", id = "forged", name = "mag-status",
    caller_id = direct_actor, invocation = sibling.invocation, args = { run_id = direct_id } })
  reply = find_call(decode_calls(), function(c) return c.body.id == "forged" end)
  assert_eq(reply.body.output.error_code, "run_control_unauthorized",
    "forged caller metadata is ignored in favor of stamped invocation provenance")

  registry:reset()
  assert_eq(next(registry.dispatcher_runs), nil, "session reset clears dispatcher ownership")
  assert_eq(next(registry.run_dispatchers), nil, "session reset clears reverse ownership")
end

-- Workers have the same fresh-dispatch-only tool contract as the lead,
-- including when they supply a handle for their own directly dispatched child.
do
  fresh()
  local registry = lw._internals.run_registry
  local actor = "parent.run-tool"
  local child_id = registry:mint_run_id()
  lw._internals.register_active_run(child_id, {}, "terminal", "child-dispatch",
    "child", sessions.current_id(), actor)
  local metadata = { invocation = invocation(sessions.current_id(), "subagent",
    "scope/cap-parent-apply", actor, "mag-run-parent") }
  _test.calls_clear()
  invoke_tool_with_metadata("apply-child", "mag-apply", {
    file = "authority-delta.mag", content = "(artifact nil)", run_id = child_id,
  }, metadata)
  local result = tool_result("apply-child")
  assert_true(result and result.body.error:find("run_id is not supported", 1, true) ~= nil,
    "workers cannot modify even their own dispatched live run")
  assert_eq(find_call(decode_calls(), function(c)
    return c.body.kind == "mag.load" or c.body.kind == "mag.build"
      or c.body.kind == "mag.execute" or c.body.kind == "mag.apply"
  end), nil, "worker live modification is rejected before compilation or dispatch")
end

-- ------------------------------------------------------------------
-- double-Esc interrupts DETACHED dispatched runs (the tag-blocking incident)
-- ------------------------------------------------------------------
--
-- A fresh `mag apply` is FIRE-AND-FORGET: it acks "executing" at dispatch
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

-- (fresh mag apply dispatch cancel propagation) A `tool.cancel` addressed to a
-- fresh `mag apply` DISPATCH firing propagates into that detached run —
-- completeness for the general cancel route.
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

-- Delayed gate approvals consume their preserved invocation provenance. Lead
-- provenance still receives the stable detached acknowledgement; if approval
-- lands after a session switch, MAG fails before touching a workspace or
-- starting a compiler load.
do
  fresh()
  local owning_session = sessions.current_id()
  write_mag_file("provenance-write", "provenance.mag", READ_ONLY_MAG)
  _test.calls_clear()
  invoke_tool_with_metadata("provenance-mag-lead", "mag-apply", {
    file = "provenance.mag",
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
  invoke_tool_with_metadata("provenance-mag-agent", "mag-apply", {
    file = "provenance-attached.mag",
  }, { caller_id = "r-agent/cap-1", invocation = invocation(owning_session, "subagent", "r-agent/cap-1") })
  feed_loaded(read_only_modification())
  local detached_exec = find_call(decode_calls(), function(c) return c.body.kind == "mag.execute" end)
  assert_true(detached_exec ~= nil and lw._internals.state.active_runs[detached_exec.body.run_id] ~= nil,
    "subagent file mag is detached and registered under its direct dispatcher")
  assert_eq(lw._internals.run_registry.run_dispatchers[detached_exec.body.run_id],
    "worker.run-tool", "kernel-stamped actor owns the detached file run")
  assert_true(find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "provenance-mag-agent"
  end) ~= nil, "detached file mag acknowledges its stable run handle immediately")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = detached_exec.body.run_id,
    status = "completed", result = { text = "detached result" } })

  fresh()
  owning_session = sessions.current_id()
  local stale_mag = invocation(owning_session, "lead", "r-stale/cap-1")
  sessions.new()
  _test.calls_clear()
  invoke_tool_with_metadata("stale-mag", "mag-apply", {
    file = "never-loaded.mag",
  }, { caller_id = "r-current/cap-1", invocation = stale_mag })
  local calls = decode_calls()
  local stale_error = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "stale-mag"
  end)
  assert_true(stale_error ~= nil and stale_error.body.error:find("no longer active", 1, true),
    "delayed mag approval fails against its ended invocation session")
  assert_eq(find_call(calls, function(c) return c.body.kind == "mag.load" end), nil,
    "stale mag provenance performs no workspace load or execute")

end

-- mag-await blocks on the canonical terminal event without polling. Multiple
-- waiters receive one canonical result each and suppress automatic delivery.
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
  invoke_tool("waiter-b", "mag-await", { run_id = run_id })
  invoke_tool("waiter-a", "mag-await", { run_id = run_id })
  assert_eq(#decode_calls(), 0, "active mag-await retains firing with no immediate result")
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
  assert_eq(relays, 0, "explicit waiters suppress automatic asynchronous delivery")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "completed" })
  agentic_loop.relay_run_completion = original
  assert_eq(#decode_calls(), 0, "duplicate terminal event is a total no-op")
  assert_eq(relays, 0, "duplicate terminal event cannot trigger automatic delivery")

  invoke_tool("already-done", "mag-await", { run_id = run_id })
  local immediate = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "already-done"
  end)
  assert_true(immediate ~= nil and immediate.body.output.result.text == "slow output",
    "retained completed run returns immediately")
end

-- Canonical await retention is independent of the legacy mag-status
-- projection. An output_path-only success keeps the old structural terminal
-- result summary, while await returns the complete canonical terminal body.
do
  local run_id = dispatch_awaitable("status-compat")
  local terminal = lw._internals.state.active_runs[run_id].terminal
  _test.calls_clear()
  invoke_tool("status-compat-wait", "mag-await", { run_id = run_id })
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
    "mag-status keeps the prior structural terminal result projection")
  assert_eq(summary.output_path, nil,
    "mag-status does not leak canonical-only top-level output_path")

  run_id = dispatch_awaitable("status-failure-fallback")
  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "failed" })
  summary = lw._internals.summarize_run(lw._internals.state.completed_runs[#lw._internals.state.completed_runs])
  assert_eq(summary.error, "mag run failed",
    "failed mag-status summary exposes actionable fallback error")
end

-- Failed and killed terminals preserve typed status/error semantics.
do
  for _, case in ipairs({
    { name = "failed", code = "await_run_failed", error = "worker failed", duration_ms = 3210 },
    { name = "killed", code = "await_run_killed", error = "stopped", duration_ms = 6543 },
  }) do
    local run_id = dispatch_awaitable("await-" .. case.name)
    invoke_tool("wait-" .. case.name, "mag-await", { run_id = run_id })
    _test.calls_clear()
    feed("mag", { kind = "mag.run_result", run_id = run_id, status = case.name,
      error = case.error, duration_ms = case.duration_ms, metadata = { passthrough = true } })
    local reply = find_call(decode_calls(), function(c)
      return c.body.kind == "tool.result" and c.body.id == "wait-" .. case.name
    end)
    assert_true(reply ~= nil and reply.body.error_code == case.code,
      case.name .. " waiter receives stable typed error")
    assert_eq(reply.body.status, case.name, case.name .. " status is preserved")
    assert_eq(reply.body.terminal.duration_ms, case.duration_ms,
      case.name .. " runtime-owned duration is preserved")
    assert_eq(reply.body.terminal.metadata.passthrough, true,
      case.name .. " canonical metadata is preserved")
  end
end

-- Canceling one waiter detaches only it; no kernel control or source result is
-- emitted, the run and other waiters continue.
do
  local run_id = dispatch_awaitable("await-cancel")
  _test.calls_clear()
  invoke_tool("cancel-me", "mag-await", { run_id = run_id })
  invoke_tool("keep-me", "mag-await", { run_id = run_id })
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

-- mag-terminate retains a terminating run and its waiter until canonical
-- killed confirmation. Session end instead settles waiters and clears state.
do
  local run_id = dispatch_awaitable("await-terminate")
  _test.calls_clear()
  invoke_tool("termination-waiter", "mag-await", { run_id = run_id })
  invoke_tool("termination-request", "mag-terminate", { run_id = run_id })
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
  invoke_tool("session-waiter", "mag-await", { run_id = run_id })
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
    invoke_tool(case.id, "mag-await", { run_id = case.run_id })
    local reply = find_call(decode_calls(), function(c)
      return c.body.kind == "tool.result" and c.body.id == case.id
    end)
    assert_true(reply ~= nil and reply.body.error_code == case.code,
      case.code .. " is returned directly")
    _test.calls_clear()
  end
  local registry = lw._internals.run_registry
  local other_session = registry:register({ run_id = registry:mint_run_id(), run_name = "other",
    session_id = "other-session", terminal = "worker" })
  invoke_tool("wrong", "mag-await", { run_id = other_session.run_id })
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
  invoke_tool("expired", "mag-await", { run_id = first.run_id })
  local expired = find_call(decode_calls(), function(c)
    return c.body.kind == "tool.result" and c.body.id == "expired"
  end)
  assert_true(expired ~= nil and expired.body.error_code == "await_run_expired",
    "displaced terminal outcome leaves an expired tombstone")
  registry.terminal_limit = 64
end

-- Build routing is shared by preview and apply; diagnostic status cannot
-- resurrect canceled work. The cold-path suite above remains opt-out coverage.
for _, status in ipairs({ "miss", "hit" }) do
  for _, action in ipairs({ "compile", "apply" }) do
    fresh()
    lw.configure { project_build = { cache_dir = "/persistent/cache" },
      dependency_module_roots = { "/immutable/modules" } }
    write_mag_file("build-write", "build.mag", READ_ONLY_MAG)
    _test.calls_clear()
    invoke_tool("build-request", action == "compile" and "mag-preview" or "mag-apply",
      { file = "build.mag" })
    local load = find_call(decode_calls(), function(c) return c.body.kind == "mag.build" end)
    assert_true(load ~= nil, action .. " opts into project build")
    local ws = require("libs.mag-workspace").workspace_dir(sessions.current_id())
    assert_eq(load.body.project_root, ws, "session project root")
    assert_eq(load.body.cache_dir, "/persistent/cache", "composition-selected cache")
    assert_eq(load.body.module_roots[1], "/immutable/modules", "ordered dependency roots")
    assert_eq(load.body.no_cache, false, "cache enabled by default")
    assert_eq(load.body.entry, "build.mag", "relative file entry")
    feed("tool-gate", { kind = "lead-workflow.tool.cancel", id = "build-request" })
    _test.calls_clear()
    feed("mag", { kind = "mag.loaded", in_reply_to = load.body.id,
      build = { status = status }, hash = "sha256:test",
      artifact = envelope_from_modification(read_only_modification()) })
    assert_eq(find_call(decode_calls(), function(c)
      return c.body.kind == "mag.execute" or c.body.kind == "mag.apply"
    end), nil, "late " .. status .. " cannot execute canceled " .. action)
  end
end

do
  local workspace = require("libs.mag-workspace")
  local ws = assert(workspace.init_workspace("manifest-test"))
  local path = ws .. "/mag.toml"
  local file = assert(io.open(path, "r"))
  assert_eq(file:read("*a"), "version = 1\n", "minimal project created")
  file:close()
  file = assert(io.open(path, "w")); file:write("invalid authored manifest"); file:close()
  assert(workspace.init_workspace("manifest-test"))
  file = assert(io.open(path, "r"))
  assert_eq(file:read("*a"), "invalid authored manifest", "authored manifest is never repaired")
  file:close()
  assert_eq(workspace.compile_request("cold", ws, "x.mag", {}, nil).kind,
    "mag.load", "helper preserves explicit cold opt-out")
  for _, options in ipairs({ true, { cache_dir = "relative" },
      { cache_dir = "/cache", no_cache = "yes" } }) do
    assert_true(not pcall(workspace.project_build_options, options), "invalid policy rejected")
  end
end

for _, action in ipairs({ "compile", "apply" }) do
  fresh()
  lw.configure { project_build = { cache_dir = "/persistent/cache" } }
  write_mag_file("build-success-write", "build.mag", READ_ONLY_MAG)
  _test.calls_clear()
  invoke_tool("build-success", action == "compile" and "mag-preview" or "mag-apply",
    { file = "build.mag" })
  local load = find_call(decode_calls(), function(c) return c.body.kind == "mag.build" end)
  feed("mag", { kind = "mag.loaded", in_reply_to = load.body.id,
    build = { status = "hit" }, hash = "sha256:test", factories = KERNEL_FACTORIES,
    factory_contracts = factory_contracts(),
    artifact = envelope_from_modification(read_only_modification()) })
  local execute = find_call(decode_calls(), function(c) return c.body.kind == "mag.execute" end)
  if action == "compile" then
    assert_eq(execute, nil, "build preview never executes")
    assert_true(tool_result("build-success") ~= nil, "build preview replies")
  else
    assert_true(execute ~= nil, "build " .. action .. " submits retained artifact")
    assert_eq(execute.body.model_snapshot.model, "snapshot-model", "snapshot remains execution-only")
    assert_eq(load.body.model_snapshot, nil, "live snapshot is not a build input")
  end
end

-- A negative kernel delivery receipt is not merely a warning. The child
-- obligation settles as undeliverable and its still-live owner is canceled.
do
  fresh()
  local registry = lw._internals.run_registry
  local parent_id, child_id = registry:mint_run_id(), registry:mint_run_id()
  local request = "request-delivery-rejected"
  lw._internals.register_active_run(parent_id, {}, "terminal", "parent-dispatch",
    "parent", sessions.current_id(), nil, nil, { request })
  lw._internals.register_active_run(child_id, {}, "terminal", "child-dispatch",
    "child", sessions.current_id(), "owner.run-tool",
    { run_id = parent_id, actor_id = "owner.llm" }, { request })
  feed("mag", { kind = "mag.run_result", run_id = child_id, status = "completed", result = { text = "child result" } })
  assert_true(find_call(decode_calls(), function(c) return c.body.kind == "mag.resume_actor" end) ~= nil,
    "completion is routed to the live nested owner")
  _test.calls_clear()
  feed("mag", { kind = "mag.actor_resumed", run_id = parent_id, actor_id = "owner.llm", accepted = false })
  assert_true(find_call(decode_calls(), function(c) return c.body.kind == "mag.interrupt_run" and c.body.run_id == parent_id end) ~= nil,
    "rejected delivery cancels the owning request work")
  assert_eq(find_call(decode_calls(), function(c) return c.body.kind == "agentic_loop.request_completed" end), nil,
    "request failure still waits for accepted owner cancellation")
  feed("mag", { kind = "mag.run_result", run_id = parent_id, status = "failed", error = "canceled after undeliverable completion" })
  local completed = find_call(decode_calls(), function(c) return c.body.kind == "agentic_loop.request_completed" end)
  assert_true(completed ~= nil, "owner terminal releases the final obligation")
  assert_eq(completed.body.status, "error")
  assert_eq(completed.body.error.code, "completion_undeliverable")
end

-- MAG authority loss finalizes registry handles and whole requests without
-- inventing a kernel terminal.
do
  fresh()
  local registry = lw._internals.run_registry
  local run_id = registry:mint_run_id()
  local request_id = "request-authority-lost"
  lw._internals.register_active_run(run_id, {}, "terminal", "authority-dispatch",
    "authority-run", sessions.current_id(), nil, nil, { request_id })
  agentic_loop._internals.request_lifecycle:set_terminal(
    { request_id }, "success", "stale success")
  invoke_tool("authority-waiter", "mag-await", { run_id = run_id })
  _test.calls_clear()

  assert_eq(lw.reconcile_mag_authority_loss("MAG process disappeared"), 1)
  assert_eq(lw.reconcile_mag_authority_loss("duplicate death"), 0,
    "authority reconciliation is idempotent")
  local calls = decode_calls()
  local waiter = find_call(calls, function(c)
    return c.body.kind == "tool.result" and c.body.id == "authority-waiter"
  end)
  assert_true(waiter ~= nil and waiter.body.error_code == "await_run_authority_lost",
    "awaiters receive an explicit unknown authority-loss outcome")
  local completed = find_call(calls, function(c)
    return c.body.kind == "agentic_loop.request_completed" and c.body.request_id == request_id
  end)
  assert_true(completed ~= nil, "accepted request receives durable completion")
  assert_eq(completed.body.status, "error")
  assert_eq(completed.body.error.code, "mag_authority_lost")
  assert_eq(completed.body.error.outcome, "unknown")
  assert_eq(find_call(calls, function(c) return c.body.kind == "mag.run_result" end), nil,
    "surviving lifecycle owners never fabricate mag.run_result")
  local retained = registry:get(run_id)
  assert_eq(retained.status, "unknown")
  assert_eq(retained.canonical_body, nil)
  assert_eq(next(registry.active_runs), nil)

  _test.calls_clear()
  feed("mag", { kind = "mag.run_result", run_id = run_id, status = "completed" })
  assert_eq(#decode_calls(), 0, "late terminals cannot reclassify authority loss")
end
