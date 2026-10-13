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
    constructor_id = named and "example.MessageContent" or "Unit",
    type = named and {kind="named", name="example.MessageContent", arguments={}}
      or {kind="primitive", name="Unit"},
  } }} }
end
local function reply(ref, result, err)
  return { kind = "reply", ref = ref, result = result, error = err }
end
local function unbounded()
  return { present = false, milliseconds = 0 }
end
local function typed_params(params) return params end

for _, case in ipairs({
  { module = process.exec, name = "process-exec", capability = "process.exec",
    params = typed_params({ argv = {"printf", "%s", "hello"}, cwd = "/repo", timeout = unbounded() }) },
  { module = process.script, name = "shell-script", capability = "shell.script",
    params = typed_params({ script = "printf hello", cwd = "/repo", timeout = unbounded() }) },
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
  local diagnostics = {}
  local instance, construct_error = case.module.construct("operation", case.params, emit, {
    diagnostic = function(value) diagnostics[#diagnostics + 1] = value end,
  })
  assert_true(instance ~= nil and construct_error == nil, case.name .. " constructs")
  assert_true(find(messages, "mag.ready") ~= nil, "ready emitted")
  assert_eq(instance.deliver(input("mag.Unit", {})).status, "pending", "invoke pending")
  local invocation = find(messages, "capability.invoke")
  assert_eq(invocation.capability, case.capability, "capability name")
  assert_eq(invocation.request.name, case.capability, "request name")
  assert_eq(invocation.request.workflow_node, true, "a node's own request is marked as such")
  assert_eq(invocation.request.approval_policy, nil, "a node has no approval policy of its own")
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
  assert_eq(result.value.termination.constructor, "ProcessExited", "exit constructor preserved")
  assert_eq(result.value.termination.value.code, 7, "exit code preserved")
  assert_eq(diagnostics[1].kind, "process_exit", "process exit diagnostic emitted")
  assert_eq(diagnostics[1].code, 7, "exit diagnostic preserves the raw code")
end

do
  local messages, emit = capture()
  local diagnostics = {}
  local instance = process.exec.construct("signaled", typed_params({
    argv = {"sleep", "5"}, cwd = "/repo", timeout = unbounded(),
  }), emit, {
    diagnostic = function(value) diagnostics[#diagnostics + 1] = value end,
  })
  instance.deliver(input("mag.Unit", {}))
  local invocation = find(messages, "capability.invoke")
  local completion = instance.deliver(reply(invocation.ref, {
    stdout = "partial", stderr = "terminated",
    termination = { kind = "signal", signal = 15 },
  }))
  assert_eq(completion.status, "ok", "signal termination is a normal process result")
  local result = find(messages, "nefor.process.Result")
  assert_eq(result.value.termination.constructor, "ProcessSignaled", "signal constructor preserved")
  assert_eq(result.value.termination.value.signal, 15, "signal number preserved")
  assert_eq(diagnostics[1].kind, "process_exit", "signal diagnostic emitted")
  assert_eq(diagnostics[1].signal, 15, "signal diagnostic preserves the raw signal")
end

do
  local messages, emit = capture()
  local instance = process.exec.construct("piped", typed_params({
    argv = {"cat"}, cwd = "/repo", timeout = {present=true, milliseconds=25},
  }), emit)
  instance.deliver(input("mag.Text", {value={content="must not become stdin"}}))
  local invocation = find(messages, "capability.invoke")
  assert_eq(invocation.request.args.stdin, nil,
    "Unit process nodes never infer stdin from a misleading runtime payload")
  assert_eq(invocation.request.args.timeout.present, true, "bounded timeout remains explicit")
  assert_eq(invocation.request.args.timeout.milliseconds, 25, "positive timeout passed")
  local failed = instance.deliver(reply(invocation.ref, nil, "provider unavailable"))
  assert_eq(failed.status, "failed", "capability error fails")
  assert_eq(failed.failure, "nefor.process.CapabilityFailed", "typed capability failure")
  assert_eq(failed.value.operation, "process.exec", "failure identifies operation")
end

for _, malformed in ipairs({
  "dumped output summary",
  { stdout="", stderr="", termination={kind="code", code="7"} },
  { stdout="", stderr="", termination={kind="signal", signal=1.5} },
}) do
  local messages, emit = capture()
  local diagnostics = {}
  local instance = process.script.construct("malformed", typed_params({
    script="printf hello", cwd="/repo", timeout=unbounded(),
  }), emit, {
    diagnostic = function(value) diagnostics[#diagnostics + 1] = value end,
  })
  instance.deliver(input("mag.Unit", {}))
  local invocation = find(messages, "capability.invoke")
  local failed = instance.deliver(reply(invocation.ref, malformed))
  assert_eq(failed.status, "failed", "malformed process result fails without crashing")
  assert_eq(failed.failure, "nefor.process.CapabilityFailed", "malformed result uses typed failure")
  assert_eq(failed.value.error, "capability returned malformed ProcessResult",
    "malformed result explains the contract failure")
  assert_eq(#diagnostics, 0, "malformed result emits no process-exit diagnostic")
end

for _, bad in ipairs({
  typed_params({argv={"true"}, cwd="/repo", timeout={present=true,milliseconds=0}}),
  typed_params({argv={"true"}, cwd="/repo", timeout={present=true,milliseconds=-1}}),
  typed_params({argv={"true"}, cwd="/repo", timeout={present=true,milliseconds=1.5}}),
  typed_params({argv={"true"}, cwd="/repo", timeout={present=true,milliseconds="25"}}),
  typed_params({argv={"true"}, cwd="/repo", timeout={present=false,milliseconds=1}}),
  typed_params({argv={"true"}, cwd="/repo", timeout={present=true,milliseconds=1e20}}),
  typed_params({argv={}, cwd="/repo", timeout=unbounded()}),
  typed_params({argv={"true", 2}, cwd="/repo", timeout=unbounded()}),
  typed_params({argv={"true"}, cwd="", timeout=unbounded()}),
}) do
  local messages, emit = capture()
  local instance, error = process.exec.construct("invalid", bad, emit)
  assert_true(instance == nil and error ~= nil, "malformed exec params rejected")
  assert_true(find(messages, "capability.invoke") == nil, "invalid params never invoke")
end

do
  local messages, emit = capture()
  local instance, error = process.exec.construct("invalid-timeout", typed_params({
    argv={"true"}, cwd="/repo", timeout={present=true,milliseconds=0},
  }), emit)
  assert_eq(instance, nil, "invalid normalized timeout is rejected")
  assert_true(error:find("normalized runtime timeout", 1, true) ~= nil,
    "timeout diagnostics identify the normalized runtime boundary")
end

do
  local messages, emit = capture()
  local instance, error = process.script.construct("invalid", typed_params({
    script="", cwd="/repo", timeout=unbounded(),
  }), emit)
  assert_true(instance == nil and error ~= nil, "empty script rejected")
  assert_true(find(messages, "capability.invoke") == nil, "invalid script never invokes")
end

do
  local messages, emit = capture()
  local instance = process.exec.construct("killed", typed_params({
    argv={"sleep", "5"}, cwd="/repo", timeout=unbounded(),
  }), emit)
  instance.deliver(input("mag.Unit", {}))
  local invocation = find(messages, "capability.invoke")
  instance.handle_kill()
  assert_true(instance.deliver(reply(invocation.ref, {stdout="late",stderr="",status=0})) == nil,
    "late reply after MAG cancellation is void")
end


-- Preserve signed-i64 maxima through the actual JSON codec and both capability
-- boundaries, not merely as Lua literals or compiler artifact assertions.
for _, maximum in ipairs({9223372036854775807, 9223372036854775000, 9223372036854720000}) do
  for _, factory in ipairs({process.exec, process.script}) do
    local params = nefor.json.decode(nefor.json.encode({
      argv={"true"}, script="true", cwd="/repo",
      timeout={present=true, milliseconds=maximum},
    }))
    local messages, emit = capture()
    local instance, err = factory.construct("maximum-timeout", params, emit)
    assert_true(instance ~= nil and err == nil, "maximum timeout constructs")
    instance.deliver(input("mag.Unit", {}))
    local invocation = nefor.json.decode(nefor.json.encode(find(messages, "capability.invoke")))
    assert_eq(math.type(invocation.request.args.timeout.milliseconds), "integer", "transport retains integer")
    assert_eq(invocation.request.args.timeout.milliseconds, maximum, "transport retains exact maximum")
  end
end

print("mag-kernel process_test: all assertions passed")
