-- The validator's actor: it forwards the gate's question to the chat surface
-- and decides nothing. Also the classifier probe: one cached probe, and only
-- the configured executable is ever run. Driven by
-- engine/tests/starter_tool_validator_test.rs with a stubbed process runner.

local CLASSIFIER = "/managed/plugins/da/bin/da"
local tv_lib = require("libs.tool-validator")
local tv = require("tool-validator").build { classifier = CLASSIFIER }
local json = nefor.json

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format("assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ", tostring(expected), tostring(actual)), 2)
  end
end

local function decode_calls()
  local out = {}
  for _, c in ipairs(_test.calls()) do
    local ok, decoded = pcall(json.decode, c.payload)
    if ok and type(decoded) == "table" and type(decoded.body) == "table" then
      out[#out + 1] = decoded.body
    end
  end
  return out
end

local function feed(validator, body)
  validator.receive_msg({
    ts = "2026-05-08T00:00:00.000Z", origin = "tool-gate",
    payload = json.encode({ type = "event", from = "tool-gate", body = body }),
  })
end

-- A question from the gate becomes one popup request, whatever the tool.
for _, tool in ipairs({ "shell.script", "write_file", "mag-apply", "read_file" }) do
  _test.calls_clear()
  feed(tv, {
    kind = "chat.tool.permission_request", id = "ask-" .. tool, tool = tool,
    args = { script = "forbidden rm" }, approval_policy = "Default",
    reason = "not shown to be safe", allowlist = { tool },
  })
  local calls = decode_calls()
  assert_eq(#calls, 1, tool .. ": exactly one envelope")
  assert_eq(calls[1].kind, "chat.tool.popup_request", tool .. ": the question is shown, not answered")
  assert_eq(calls[1].id, "ask-" .. tool, "the popup keeps the call id")
  assert_eq(calls[1].tool, tool, "the popup names the tool")
  assert_eq(calls[1].args.script, "forbidden rm", "the popup carries the arguments")
  assert_eq(calls[1].reason, "not shown to be safe", "the popup says why the user is asked")
  assert_eq(calls[1].approval_policy, "Default", "the popup names the caller's policy")
end

-- The actor never answers for the user and ignores everything else: the
-- permission mode is the gate's concern.
do
  _test.calls_clear()
  feed(tv, { kind = "tool-gate.mode_changed", mode = "yolo" })
  feed(tv, { kind = "sessions.session_end", session_id = "old" })
  feed(tv, { kind = "chat.tool.permission_request", tool = "shell.script" })
  feed(tv, { kind = "chat.tool.permission_request", id = "no-tool" })
  assert_eq(#decode_calls(), 0, "nothing is emitted for other events or malformed questions")
  feed(tv, { kind = "chat.tool.permission_request", id = "after-yolo", tool = "shell.script", args = {} })
  local calls = decode_calls()
  assert_eq(calls[1].kind, "chat.tool.popup_request", "a question is forwarded in every mode")
  for _, call in ipairs(calls) do
    assert_eq(call.kind ~= "tool.permission_response", true, "the validator never answers a question itself")
  end
end

-- A classifier that cannot be probed denies and names the configured path.
do
  local original_run = nefor.process.run
  nefor.process.run = function(_) return { code = -1, stderr = "spawn failed: no such file" } end
  local validator = tv_lib.build { classifier = "/missing/plugins/da/bin/da" }
  local result, reason = validator.classify_command("Default", "git status")
  nefor.process.run = original_run
  assert_eq(result, "denied", "an unusable classifier denies")
  assert_eq(reason:find("tool_classifier_unavailable", 1, true) ~= nil, true, "the failure is actionable")
  assert_eq(reason:find("/missing/plugins/da/bin/da", 1, true) ~= nil, true, "it names the configured path")
end

-- A classifier can pass its cached version probe and still fail on the actual
-- command; only the injected path is ever run, and the probe runs once.
do
  local original_run = nefor.process.run
  local commands, probes = {}, 0
  nefor.process.run = function(opts)
    commands[#commands + 1] = opts.cmd
    if type(opts.args) == "table" and opts.args[1] == "--version" then
      probes = probes + 1
      return { code = 0, stdout = "da fixture" }
    end
    return { code = -1, stderr = "classifier process crashed" }
  end
  local validator = tv_lib.build { classifier = CLASSIFIER }
  local result, reason = validator.classify_command("ReadOnly", "git status")
  validator.classify_command("Default", "git log")
  nefor.process.run = original_run
  assert_eq(result, "denied", "a failure after the probe denies")
  assert_eq(reason:find("classifier process crashed", 1, true) ~= nil, true, "the diagnostic is preserved")
  assert_eq(probes, 1, "the probe is cached")
  assert_eq(#commands, 3, "one probe and one run per classification")
  for _, command in ipairs(commands) do assert_eq(command, CLASSIFIER, "only the injected path is run") end
end

print("tool_validator ask_test: all assertions passed")
