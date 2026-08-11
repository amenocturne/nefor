local Registry = require("registry")
local process = require("factories.process")

local function assert_eq(actual, expected, message)
  if actual ~= expected then
    error(string.format("%s: expected %s, got %s", message, tostring(expected), tostring(actual)), 2)
  end
end
local function assert_true(value, message)
  if not value then error(message, 2) end
end
local function capture()
  local messages = {}
  return messages, function(message) messages[#messages + 1] = message end
end
local function find(messages, kind)
  for _, message in ipairs(messages) do if message.kind == kind then return message end end
end
local function input(tag, value)
  local named = tag == "mag.Text"
  return { messages = {{ tag = "nefor.process.Input", message = value or {}, arrival = {
    constructor_id = named and "nefor.contracts.Text" or "Unit",
    type = named and {kind="named", name="nefor.contracts.Text", arguments={}}
      or {kind="primitive", name="Unit"},
  } }} }
end
local function reply(ref, result, err)
  return { kind = "reply", ref = ref, result = result, error = err }
end
local function unbounded()
  return { present = false, milliseconds = 0 }
end

for _, case in ipairs({
  { module = process.exec, name = "process-exec", capability = "process.exec",
    params = { argv = {"printf", "%s", "hello"}, cwd = "/repo", timeout = unbounded() } },
  { module = process.script, name = "shell-script", capability = "shell.script",
    params = { script = "printf hello", cwd = "/repo", timeout = unbounded() } },
}) do
  local registry = Registry.new()
  local declaration, error = registry:register({
    declaration = case.module.declaration, construct = case.module.construct,
  })
  assert_true(declaration ~= nil and error == nil, case.name .. " registers")
  assert_eq(declaration.name, case.name, "factory name")
  assert_eq(declaration.inputs.input[1], "nefor.process.Input", "union input wire")
  assert_eq(declaration.outputs[1], "nefor.process.Result", "structured result wire")
  assert_eq(declaration.outputs[2], "nefor.process.CapabilityFailed", "typed failure wire")

  local messages, emit = capture()
  local instance, construct_error = case.module.construct("operation", case.params, emit)
  assert_true(instance ~= nil and construct_error == nil, case.name .. " constructs")
  assert_true(find(messages, "mag.ready") ~= nil, "ready emitted")
  assert_eq(instance.deliver(input("mag.Unit", {})).status, "pending", "invoke pending")
  local invocation = find(messages, "capability.invoke")
  assert_eq(invocation.capability, case.capability, "capability name")
  assert_eq(invocation.request.name, case.capability, "request name")
  assert_eq(invocation.request.args.cwd, "/repo", "cwd retained")
  assert_eq(invocation.request.args.timeout.present, false, "explicit unbounded timeout retained")
  assert_eq(invocation.request.args.timeout.milliseconds, 0, "unbounded timeout sentinel retained")
  local completion = instance.deliver(reply(invocation.ref, {
    stdout = "hello", stderr = "warning",
    termination = { kind = "code", code = 7 },
  }))
  assert_eq(completion.status, "ok", "nonzero is a normal process result")
  local result = find(messages, "nefor.process.Result")
  assert_eq(result.value.stdout, "hello", "stdout preserved")
  assert_eq(result.value.stderr, "warning", "stderr preserved")
  assert_eq(result.value.termination.kind, "code", "status kind preserved")
  assert_eq(result.value.termination.value, 7, "exit code preserved")
end

do
  local messages, emit = capture()
  local instance = process.exec.construct("piped", {
    argv = {"cat"}, cwd = "/repo", timeout = {present=true, milliseconds=25},
  }, emit)
  instance.deliver(input("mag.Text", {value={content="stdin"}}))
  local invocation = find(messages, "capability.invoke")
  assert_eq(invocation.request.args.stdin, "stdin", "Text becomes stdin")
  assert_eq(invocation.request.args.timeout.present, true, "bounded timeout remains explicit")
  assert_eq(invocation.request.args.timeout.milliseconds, 25, "positive timeout passed")
  local failed = instance.deliver(reply(invocation.ref, nil, "provider unavailable"))
  assert_eq(failed.status, "failed", "capability error fails")
  assert_eq(failed.failure, "nefor.process.CapabilityFailed", "typed capability failure")
  assert_eq(failed.value.operation, "process.exec", "failure identifies operation")
end

for _, bad in ipairs({
  {argv={"true"}, cwd="/repo", timeout={present=true,milliseconds=0}},
  {argv={"true"}, cwd="/repo", timeout={present=true,milliseconds=-1}},
  {argv={"true"}, cwd="/repo", timeout={present=true,milliseconds=1.5}},
  {argv={}, cwd="/repo", timeout=unbounded()},
  {argv={"true", 2}, cwd="/repo", timeout=unbounded()},
  {argv={"true"}, cwd="", timeout=unbounded()},
}) do
  local messages, emit = capture()
  local instance, error = process.exec.construct("invalid", bad, emit)
  assert_true(instance == nil and error ~= nil, "malformed exec params rejected")
  assert_true(find(messages, "capability.invoke") == nil, "invalid params never invoke")
end

do
  local messages, emit = capture()
  local instance, error = process.script.construct("invalid", {
    script="", cwd="/repo", timeout=unbounded(),
  }, emit)
  assert_true(instance == nil and error ~= nil, "empty script rejected")
  assert_true(find(messages, "capability.invoke") == nil, "invalid script never invokes")
end

do
  local messages, emit = capture()
  local instance = process.exec.construct("killed", {
    argv={"sleep", "5"}, cwd="/repo", timeout=unbounded(),
  }, emit)
  instance.deliver(input("mag.Unit", {}))
  local invocation = find(messages, "capability.invoke")
  instance.handle_kill()
  assert_true(instance.deliver(reply(invocation.ref, {stdout="late",stderr="",status=0})) == nil,
    "late reply after MAG cancellation is void")
end

print("mag-kernel process_test: all assertions passed")
