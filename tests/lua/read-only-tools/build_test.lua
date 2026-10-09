-- The shipped tool boundary retains repository discovery and ordinary skills.
-- Driven by engine/tests/read_only_tools_test.rs with isolated on-disk fixtures.

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

local captured
local replies = {}
nefor.engine = {
  now = function() return 0 end,
  send = function(payload)
    local decoded = nefor.json.decode(payload)
    local body = decoded.body or {}
    if body.kind == "tool-gate.tools.advertise" then captured = body.tools end
    if body.kind == "tool.result" then replies[body.id] = body end
  end,
}

local rot = require("libs.read-only-tools")
local removed = { "list_dir", "search_text", "python-read", "instructions" }
local invocation = 0

local function advertise(spec)
  captured = nil
  spec.receive_msg({ origin = "plugin", payload = nefor.json.encode({
    body = { kind = "tool-gate.hello" },
  }) })
  local names = {}
  for _, tool in ipairs(captured or {}) do names[tool.name] = true end
  return names
end

local function invoke(spec, name, args)
  invocation = invocation + 1
  local id = "tool-call-" .. invocation
  spec.receive_msg({ origin = "plugin", payload = nefor.json.encode({ body = {
    kind = "read-only-tools.tool.invoke", id = id, name = name, args = args,
  } }) })
  return assert(replies[id], "every invocation settles its tool result")
end

local remaining = rot.build { include = { "discover_instruction_files", "skill" } }
local names = advertise(remaining)
assert_true(names.discover_instruction_files and names.skill, "remaining tools are advertised")
for _, name in ipairs(removed) do
  assert_true(not names[name], "removed tool is not advertised: " .. name)
  assert_true(not pcall(rot.build, { include = { name } }), "removed base tool cannot be registered: " .. name)
  local reply = invoke(remaining, name, {})
  assert_true(type(reply.error) == "string" and reply.output == nil, "removed invocation fails: " .. name)
end

local skill = invoke(remaining, "skill", { name = "example" })
assert_true(skill.output == "Ordinary workflow skill.\n" and skill.error == nil,
  "ordinary skill loading retains its complete configured content")
local discovery = invoke(remaining, "discover_instruction_files", {
  path = READ_ONLY_TEST_WORKSPACE, scope = "subfolders",
})
assert_true(discovery.error == nil and discovery.output:find("AGENTS.md", 1, true)
  and discovery.output:find("nested/CLAUDE.md", 1, true), "repository instruction discovery remains callable")
assert_true(not discovery.output:find("Root repository guidance", 1, true),
  "discovery lists instruction paths without loading their contents")

-- The actual starter source advertises a surviving capability and can satisfy
-- a readiness barrier without any deleted base tool.
local starter = require("read-only-tools")
local starter_names = advertise(starter)
assert_true(starter_names.discover_instruction_files, "starter advertises repository discovery")
for _, name in ipairs(removed) do
  assert_true(not starter_names[name], "starter does not advertise removed tool: " .. name)
end
local ready = false
local barrier = require("libs.startup-readiness")._new {
  required_plugins = {}, required_tools = { "discover_instruction_files" },
  on_ready = function() ready = true end,
}
barrier.observe({ kind = "tool.register", tools = captured }, "tool-gate")
assert_true(ready, "surviving starter advertisement satisfies readiness")
assert_true(invoke(starter, "discover_instruction_files", {
  path = READ_ONLY_TEST_WORKSPACE, scope = "subfolders",
}).error == nil, "starter dispatches the surviving capability")

assert_true(next(advertise(rot.build {})) == nil, "base tools remain opt-in")
assert_true(not pcall(rot.build, { include = { "does_not_exist" } }), "unknown base name fails")
assert_true(not pcall(rot.build, { include = { "skill", "skill" } }), "duplicate base name fails")

local tool = require("libs.tool")
local function custom_display(label)
  return {
    compact = { label = label }, expanded = { label = label, fields = {} },
    result = { kind = "content", fields = {} },
  }
end
local custom = tool.define {
  name = "custom", description = "Custom lookup",
  parameters = { type = "object", properties = { value = { type = "string" } }, required = { "value" } },
  display = custom_display("Custom"),
  handler = function(args, call) call.ok(args.value) end,
}
local extras = rot.build { include = { "skill" }, extra_tools = { custom } }
local extra_names = advertise(extras)
assert_true(extra_names.skill and extra_names.custom, "config extras compose with remaining base tools")
assert_true(invoke(extras, "custom", { value = "custom result" }).output == "custom result",
  "custom handler still receives its arguments and settles the invocation")
local undeclared = invoke(extras, "custom", { value = "x", path = "alias" })
assert_true(undeclared.error == "custom: unknown argument `path`; accepted arguments: value",
  "extra tool schemas are closed: " .. tostring(undeclared.error))
assert_true(invoke(extras, "skill", { name = 3 }).error
  == "skill: argument `name` must be exactly one of: string or array of string, got integer",
  "base tool arguments are validated before the handler")
assert_true(invoke(extras, "skill", { name = {} }).error == "skill: name must list at least one skill",
  "an empty skill list is a semantic error")
assert_true(not pcall(rot.build, {
  include = { "skill" }, extra_tools = { tool.define {
    name = "skill", description = "x", parameters = { type = "object" },
    display = custom_display("skill"), handler = function(_, call) call.ok("unexpected") end,
  } },
}), "extra tool cannot collide with an included base name")

-- A checked output rejection is settled by the invocation owner using a small
-- correlated error. A failing extra must not send a second result after success.
do
  local send = nefor.engine.send
  local rejected, terminals = 0, 0
  nefor.engine.send = function(payload)
    local body = nefor.json.decode(payload).body
    if body.kind == "tool.result" and type(body.output) == "string" and #body.output > 1024 then
      rejected = rejected + 1
      error("frame_too_large")
    end
    if body.kind == "tool.result" then terminals = terminals + 1 end
    return send(payload)
  end
  local result = invoke(extras, "custom", { value = string.rep("Ж", 2048) })
  assert_true(result.output == nil and result.error_code == "output_rejected",
    "rejected response becomes one correlated terminal")
  assert_true(rejected == 1 and terminals == 1, "no recursive fallback")
  local later = invoke(extras, "custom", { value = "later response" })
  assert_true(later.output == "later response", "producer remains usable after rejection")
  nefor.engine.send = send
end
for _, already_settled in ipairs({ false, true }) do
  local terminals = 0
  local send = nefor.engine.send
  nefor.engine.send = function(payload)
    if nefor.json.decode(payload).body.kind == "tool.result" then terminals = terminals + 1 end
    return send(payload)
  end
  local throwing = rot.build { extra_tools = { tool.define {
    name = "custom", description = "Throwing custom tool",
    parameters = { type = "object" }, display = custom_display("custom"),
    handler = function(_, emit)
      if already_settled then emit.ok("completed") end
      error("expected external invocation failure")
    end,
  } } }
  local result = invoke(throwing, "custom", {})
  assert_true(terminals == 1, "throwing invocation settles exactly once")
  assert_true(already_settled and result.output == "completed" or not already_settled and result.error ~= nil)
  nefor.engine.send = send
end

-- Count physical publications, not dictionary entries: retained callbacks
-- cannot settle twice or publish a terminal after cancellation.
do
  local terminals, retained = 0, nil
  local send = nefor.engine.send
  nefor.engine.send = function(payload)
    if nefor.json.decode(payload).body.kind == "tool.result" then terminals = terminals + 1 end
    return send(payload)
  end
  local guarded = rot.build { extra_tools = { tool.define {
    name = "custom", description = "Callback settlement guard",
    parameters = { type = "object" }, display = custom_display("custom"),
    handler = function(_, call)
      retained = call
      call.ok("first")
      call.err("second")
      call.ok("third")
    end,
  } } }
  assert_true(invoke(guarded, "custom", {}).output == "first", "first callback owns the result")
  assert_true(terminals == 1 and retained.settled(), "callbacks publish once")
  local delayed = rot.build { extra_tools = { tool.define {
    name = "custom", description = "Deferred callback",
    parameters = { type = "object" }, display = custom_display("custom"),
    handler = function(_, call) retained = call end,
  } } }
  delayed.receive_msg({ payload = nefor.json.encode({ body = {
    kind = "read-only-tools.tool.invoke", id = "late-callback", name = "custom", args = {},
  } }) })
  assert_true(not retained.settled(), "deferred invocation is pending")
  delayed.receive_msg({ payload = nefor.json.encode({ body = {
    kind = "read-only-tools.tool.cancel", id = "late-callback",
  } }) })
  retained.ok("late success")
  retained.err("late failure")
  assert_true(retained.settled() and terminals == 1, "cancelled callbacks publish nothing")
  nefor.engine.send = send
end

local display = require("libs.chat.tool_display")
local contract = {
  compact = { label = "Read file", primary = { label = "path", select = { source = "args", path = "path" }, kind = "path" } },
  expanded = { label = "Read file", fields = {} },
  result = { kind = "receipt", text = "content loaded", fields = {} },
}
local projection = assert(display.project(contract, { path = "README.md" }, "payload bytes", false))
assert_true(projection.label == "Read file" and projection.primary == "README.md", "display projects semantic label and primary")

return true
