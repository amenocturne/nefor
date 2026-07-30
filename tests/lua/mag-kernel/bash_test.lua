-- tests/lua/mag-kernel/bash_test.lua — factory-level unit tests for the shell
-- capability node (factories/bash.lua).
--
-- Driven from `plugins/mag/tests/bash_factory_lua.rs` (same bare-VM harness as
-- engine/tests/starter_mag_kernel_test.rs): stub `nefor.log`, package.path at
-- `plugins/mag/lua/mag-kernel/`. There is no real tool gate: capability.invoke
-- envelopes are captured and their refs replayed as reply activations, exactly
-- as routing.lua's bus_response would — a scripted capability responder.

local Registry = require("registry")
local bash = require("factories.bash")
local sink = require("factories.sink")

-- ------------------------------------------------------------------
-- helpers
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

local function capture()
  local out = {}
  return out, function(message) out[#out + 1] = message end
end

local function bash_params(command)
  return {
    command = command,
    timeout_ms = { present = false, milliseconds = 0 },
  }
end

local function find_kind(msgs, kind)
  for _, m in ipairs(msgs) do
    if m.kind == kind then return m end
  end
  return nil
end

-- Single-input graph activation (routing.lua, the kernel⇄factory contract).
local function single(from, tag, message)
  return { shape = "single", messages = { { from = from, tag = tag, message = message } } }
end

-- A correlated reply activation, the way routing.lua's bus_response builds it
-- from a captured capability.invoke's ref.
local function reply(ref, result, err)
  return { kind = "reply", ref = ref, result = result, error = err }
end

-- ==================================================================
-- declaration: registers cleanly; the pipe contract as declared data
-- ==================================================================

do
  local reg = Registry.new({ require_preview = false })
  local decl, err = reg:register({ declaration = bash.declaration, construct = bash.construct })
  assert_true(decl ~= nil and err == nil, "bash factory registers cleanly: " .. tostring(err))

  local d = reg:declaration("bash")
  assert_eq(d.inputs.input[1], "mag.Unit", "bash fires dependency-style on Unit")
  assert_eq(d.inputs.input[2], "mag.Text", "bash accepts upstream text as stdin")
  assert_eq(d.outputs[1], "mag.Text", "bash emits its stdout as mag.Text")
  assert_eq(d.outputs[2], "mag.CommandFailed", "the failure tag is declared (routable)")
  assert_eq(d.signals[1], "kill", "bash declares the kill signal")
end

-- ==================================================================
-- a compiled shell chain validates against the registry contracts
-- (bash -> bash -> sink; the sink's widened input accepts mag.Text)
-- ==================================================================

do
  local reg = Registry.new({ require_preview = false })
  reg:register({ declaration = bash.declaration, construct = bash.construct })
  reg:register({ declaration = sink.declaration, construct = sink.construct })

  local result = reg:validate_modification({
    actors = {
      { id = "bash-1", factory = "bash", params = { command = "rg foo" },
        evidence={version=2,identity="nefor.factory.bash",arguments={},input={kind="union",items={{kind="primitive",name="Unit"},{kind="named",name="nefor.contracts.Text",arguments={}}}},output={kind="named",name="nefor.contracts.Text",arguments={}}},
        input={type={kind="primitive",name="Unit"},wire="mag.Unit"},outputs={{type={kind="named",name="nefor.contracts.Text",arguments={}},wire="mag.Text"}},
        routes = { ["mag.Text"] = { { actor = "bash-2", wire = "mag.Text" } } } },
      { id = "bash-2", factory = "bash", params = { command = "sort" },
        evidence={version=2,identity="nefor.factory.bash",arguments={},input={kind="union",items={{kind="primitive",name="Unit"},{kind="named",name="nefor.contracts.Text",arguments={}}}},output={kind="named",name="nefor.contracts.Text",arguments={}}},
        input={type={kind="named",name="nefor.contracts.Text",arguments={}},wire="mag.Text"},outputs={{type={kind="named",name="nefor.contracts.Text",arguments={}},wire="mag.Text"}},
        routes = { ["mag.Text"] = { { actor = "sink", wire = "mag.Text" } } } },
      { id = "sink", factory = "sink", params = {}, routes = {},
        evidence = { version = 2, identity = "nefor.factory.sink",
          arguments = {{kind="named",name="nefor.contracts.Text",arguments={}}}, input = {kind="named",name="nefor.contracts.Text",arguments={}}, output = {kind="primitive",name="Unit"} },
        input = { type = {kind="named",name="nefor.contracts.Text",arguments={}}, wire = "mag.Text" },
        outputs = { { type = {kind="primitive",name="Unit"}, wire = "mag.Unit" } } },
    },
  })
  assert_true(result.ok,
    "a bash pipe into the sink validates: " .. table.concat(result.errors or {}, "; "))
end

-- ==================================================================
-- construction: requires a command; ready is id-signed
-- ==================================================================

do
  local _, emit = capture()
  local inst, err = bash.construct("bash-1", {}, emit)
  assert_true(inst == nil and err ~= nil, "construct without a command fails")
  assert_true(err:find("command", 1, true) ~= nil, "the failure names the missing param")

  local msgs, emit2 = capture()
  local ok_inst = bash.construct("bash-1", {
    command = "ls", timeout_ms = { present = false, milliseconds = 0 },
  }, emit2)
  assert_true(ok_inst ~= nil, "bash constructs with a command")
  local ready = find_kind(msgs, "mag.ready")
  assert_true(ready ~= nil, "bash emits ready at construct")
  assert_eq(ready.from, "bash-1", "ready is id-signed")

  -- timeout_ms is a precise record: an ill-typed bound fails
  -- construction; a valid one rides the capability args.
  local _, bad_err = bash.construct("bash-2", {
    command = "ls", timeout_ms = { present = true, milliseconds = "soon" },
  }, emit2)
  assert_true(bad_err ~= nil and bad_err:find("timeout_ms", 1, true) ~= nil,
    "an ill-typed timeout_ms fails construction with the detail")

  local m3, emit3 = capture()
  local bounded = bash.construct("bash-3", {
    command = "sleep 90", timeout_ms = { present = true, milliseconds = 120000 },
  }, emit3)
  assert_true(bounded ~= nil, "bash constructs with a bounded Timeout")
  bounded.deliver(single("mag.control", "mag.Unit", { kind = "mag.Unit" }))
  local inv = find_kind(m3, "capability.invoke")
  assert_eq(inv.request.args.timeout_ms, 120000, "the authored timeout rides the args")
end

-- ==================================================================
-- Unit-fired command: no stdin; stdout becomes the mag.Text output
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = bash.construct("bash-1", bash_params("printf 'b\\na\\n'"), emit)

  local pending = inst.deliver(single("mag.control", "mag.Unit", { kind = "mag.Unit" }))
  assert_eq(pending.status, "pending", "the command defers until the capability answers")

  local inv = find_kind(msgs, "capability.invoke")
  assert_true(inv ~= nil, "one capability.invoke per activation")
  assert_eq(inv.from, "bash-1", "invoke is id-signed")
  assert_eq(inv.capability, "bash", "the capability is the bash tool")
  assert_eq(inv.request.name, "bash", "the wrapped request names the tool")
  assert_eq(inv.request.args.command, "printf 'b\\na\\n'", "the authored command rides the args")
  assert_true(inv.request.args.stdin == nil, "a Unit firing carries no stdin")

  -- Scripted responder: the basic-tools combined-output shape.
  local completion = inst.deliver(reply(inv.ref, "b\na\n[exit 0]"))
  assert_eq(completion.status, "ok", "a clean exit completes ok")

  local out = find_kind(msgs, "mag.Text")
  assert_true(out ~= nil, "stdout becomes the mag.Text output")
  assert_eq(out.from, "bash-1", "output is id-signed")
  assert_eq(out.text, "b\na\n", "the exit footer is stripped; stdout passes verbatim")
end

-- ==================================================================
-- stdin-fed command: upstream text is delivered as stdin (the pipe)
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = bash.construct("bash-2", bash_params("sort"), emit)

  inst.deliver(single("bash-1", "mag.Text", { kind = "mag.Text", text = "b\na\n" }))
  local inv = find_kind(msgs, "capability.invoke")
  assert_eq(inv.request.args.stdin, "b\na\n", "the upstream stdout arrives as stdin")
  assert_eq(inv.request.args.command, "sort", "the command is the node's own")

  local completion = inst.deliver(reply(inv.ref, "a\nb\n[exit 0]"))
  assert_eq(completion.status, "ok", "the piped command completes ok")
  local out = find_kind(msgs, "mag.Text")
  assert_eq(out.text, "a\nb\n", "the piped command's stdout flows on")
end

-- ==================================================================
-- failure: non-zero exit fails the completion with the stderr detail
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = bash.construct("bash-1", bash_params("false"), emit)
  inst.deliver(single("mag.control", "mag.Unit", { kind = "mag.Unit" }))
  local inv = find_kind(msgs, "capability.invoke")

  local completion = inst.deliver(reply(inv.ref, "[stderr]\nboom: no such file\n[exit 2]"))
  assert_eq(completion.status, "failed", "a non-zero exit fails the completion")
  assert_eq(completion.failure, "mag.CommandFailed", "the declared failure tag is returned")
  assert_true(completion.value.error:find("exited 2", 1, true) ~= nil,
    "the failure names the exit code: " .. tostring(completion.value.error))
  assert_true(completion.value.error:find("boom: no such file", 1, true) ~= nil,
    "the failure carries the stderr detail")
  assert_true(find_kind(msgs, "mag.Text") == nil, "a failed command emits no text output")
end

-- ==================================================================
-- failure: a transport/gate error fails the completion with its detail
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = bash.construct("bash-1", bash_params("ls"), emit)
  inst.deliver(single("mag.control", "mag.Unit", { kind = "mag.Unit" }))
  local inv = find_kind(msgs, "capability.invoke")

  local completion = inst.deliver(reply(inv.ref, nil, "tool denied by gate policy"))
  assert_eq(completion.status, "failed", "a capability error fails the completion")
  assert_true(completion.value.error:find("tool denied by gate policy", 1, true) ~= nil,
    "the failure carries the capability error")
end

-- ==================================================================
-- stderr alongside a clean exit stays out of the piped stdout
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = bash.construct("bash-1", bash_params("rg foo"), emit)
  inst.deliver(single("mag.control", "mag.Unit", { kind = "mag.Unit" }))
  local inv = find_kind(msgs, "capability.invoke")

  local completion = inst.deliver(reply(inv.ref, "match\n[stderr]\nwarn: skipped dir\n[exit 0]"))
  assert_eq(completion.status, "ok", "warnings on stderr do not fail a clean exit")
  local out = find_kind(msgs, "mag.Text")
  assert_eq(out.text, "match\n", "only stdout pipes downstream")
end

-- ==================================================================
-- kill drops in-flight state; a late answer is voided
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = bash.construct("bash-1", bash_params("sleep 5"), emit)
  inst.deliver(single("mag.control", "mag.Unit", { kind = "mag.Unit" }))
  local inv = find_kind(msgs, "capability.invoke")

  inst.handle_kill()

  local completion = inst.deliver(reply(inv.ref, "too late\n[exit 0]"))
  assert_true(completion == nil, "a reply after kill resolves nothing")
  assert_true(find_kind(msgs, "mag.Text") == nil, "a voided answer emits no output")
end

print("mag-kernel bash_test: all assertions passed")
