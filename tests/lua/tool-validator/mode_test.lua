local CLASSIFIER = "/managed/plugins/da/bin/da"
local tv = require("tool-validator").build { shell_classifier = CLASSIFIER }
local tv_lib = require("libs.tool-validator")
local json = nefor.json

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format("assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ", tostring(expected), tostring(actual)), 2)
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
      out[#out + 1] = decoded.body
    end
  end
  return out
end

local function make_entry(body)
  return {
    ts      = "2026-05-08T00:00:00.000Z",
    origin  = "tool-gate",
    payload = json.encode({ type = "event", from = "tool-gate", body = body }),
  }
end

local function feed(body)
  tv.receive_msg(make_entry(body))
end

local function fresh(mode)
  tv._internals.reset()
  tv._internals.set_mode(mode or "safe")
  _test.calls_clear()
end

-- safe: a deferred shell script classification opens a popup and does not deny.
do
  fresh("safe")
  feed({ kind = "chat.tool.permission_request", id = "perm-safe", tool = "shell.script", args = { script = "maybe" } })
  local calls = decode_calls()
  assert_eq(#calls, 1, "safe defer emits one envelope")
  assert_eq(calls[1].kind, "chat.tool.popup_request", "safe defer opens popup")
  assert_eq(calls[1].id, "perm-safe", "popup keeps id")
end

-- safe: even a forbidden shell script classification opens a popup. Safe mode
-- means interactive governance, not hard runtime denial.
do
  fresh("safe")
  feed({ kind = "chat.tool.permission_request", id = "perm-safe-forbidden", tool = "shell.script", args = { script = "forbidden rm" } })
  local calls = decode_calls()
  assert_eq(#calls, 1, "safe forbidden emits one envelope")
  assert_eq(calls[1].kind, "chat.tool.popup_request", "safe forbidden opens popup")
  assert_eq(calls[1].id, "perm-safe-forbidden", "popup keeps forbidden id")
end

-- auto: the same deferred request is denied with recovery text and no popup.
do
  fresh("auto")
  feed({ kind = "chat.tool.permission_request", id = "perm-auto", tool = "shell.script", args = { script = "maybe" } })
  local calls = decode_calls()
  assert_eq(#calls, 1, "auto defer emits one envelope")
  assert_eq(calls[1].kind, "tool.permission_response", "auto defer denies")
  assert_eq(calls[1].decision, "deny", "auto decision is deny")
  assert_true(type(calls[1].reason) == "string" and calls[1].reason:find("permission_denied[auto]", 1, true) ~= nil,
    "auto denial includes recovery marker")
end

-- auto: forbidden shell script stays denied because auto has no human in the loop.
do
  fresh("auto")
  feed({ kind = "chat.tool.permission_request", id = "perm-auto-forbidden", tool = "shell.script", args = { script = "forbidden rm" } })
  local calls = decode_calls()
  assert_eq(#calls, 1, "auto forbidden emits one envelope")
  assert_eq(calls[1].kind, "tool.permission_response", "auto forbidden denies")
  assert_eq(calls[1].decision, "deny", "auto forbidden decision is deny")
end

-- Session switches revoke process-local mode authority.
do
  fresh("yolo")
  feed({ kind = "sessions.session_end", session_id = "old-session" })
  assert_eq(tv._internals.get_mode(), "safe", "session end resets validator mode")
end

-- yolo: defensive approve if a prompt-mode request reaches the validator.
do
  fresh("yolo")
  feed({ kind = "chat.tool.permission_request", id = "perm-yolo", tool = "shell.script", args = { script = "maybe" } })
  local calls = decode_calls()
  assert_eq(#calls, 1, "yolo emits one envelope")
  assert_eq(calls[1].kind, "tool.permission_response", "yolo approves")
  assert_eq(calls[1].decision, "approve", "yolo decision is approve")
end

-- yolo: write_file also bypasses the approved-plan denial path.
do
  fresh("yolo")
  feed({
    kind = "chat.tool.permission_request",
    id = "perm-yolo-write-no-plan",
    tool = "write_file",
    args = { path = "some/file.lua", new_string = "return true\n" },
  })
  local calls = decode_calls()
  assert_eq(#calls, 1, "yolo write_file no-plan emits one envelope")
  assert_eq(calls[1].kind, "tool.permission_response", "yolo write_file no-plan approves")
  assert_eq(calls[1].decision, "approve", "yolo write_file no-plan decision is approve")
end

-- Read-only status is derived from the complete allowlist, not a wire flag.
do
  local validator = tv_lib.build { shell_classifier = CLASSIFIER, read_only_tools = { "read_file", "read_image" } }
  validator._internals.set_mode("safe")
  _test.calls_clear()
  validator.receive_msg(make_entry({
    kind = "chat.tool.permission_request",
    id = "perm-read-only",
    tool = "read_file",
    allowlist = { "read_file", "read_image" },
    args = { path = "README.md" },
  }))
  local calls = decode_calls()
  assert_eq(#calls, 1, "read-only allowlist emits one envelope")
  assert_eq(calls[1].kind, "tool.permission_response", "read-only allowlist auto-approves")
  assert_eq(calls[1].decision, "approve", "read-only allowlist decision is approve")
end

-- A mixed allowlist is write-capable even when the requested tool is itself
-- read-only; it follows ordinary safe-mode policy and opens a popup.
do
  local validator = tv_lib.build { shell_classifier = CLASSIFIER, read_only_tools = { "read_file", "read_image" } }
  validator._internals.set_mode("safe")
  _test.calls_clear()
  validator.receive_msg(make_entry({
    kind = "chat.tool.permission_request",
    id = "perm-mixed",
    tool = "read_file",
    allowlist = { "read_file", "write_file" },
    args = { path = "README.md" },
  }))
  local calls = decode_calls()
  assert_eq(#calls, 1, "mixed allowlist emits one envelope")
  assert_eq(calls[1].kind, "chat.tool.popup_request", "mixed allowlist is not read-only")
end

-- process.exec preserves argv as structured data. Only an explicit structural
-- predicate may approve it; malformed and unknown shapes fail closed.
do
  local seen
  local validator = tv_lib.build {
    shell_classifier = CLASSIFIER,
    process_fastpaths = {
      function(argv, args, read_only)
        seen = { argv = argv, args = args, read_only = read_only }
        return read_only and argv[1] == "rg" and argv[2] == "--files" and #argv == 2
      end,
    },
    read_only_tools = { "process.exec" },
  }
  validator._internals.set_mode("safe")
  _test.calls_clear()
  validator.receive_msg(make_entry({
    kind = "chat.tool.permission_request",
    id = "process-fastpath",
    tool = "process.exec",
    allowlist = { "process.exec" },
    args = { argv = { "rg", "--files" }, cwd = "/repo with spaces",
      timeout = { present = true, milliseconds = 5000 } },
  }))
  local calls = decode_calls()
  assert_eq(calls[1].decision, "approve", "proven structural process fast path approves")
  assert_eq(seen.argv[1], "rg", "predicate sees executable boundary")
  assert_eq(seen.argv[2], "--files", "predicate sees argument boundary")
  assert_eq(seen.args.cwd, "/repo with spaces", "predicate sees cwd separately")
  assert_eq(seen.args.timeout.milliseconds, 5000, "predicate sees timeout separately")
  assert_eq(seen.read_only, true, "predicate sees capability classification")

  local cases = {
    { id = "unknown", args = { argv = { "rg", "TODO" }, cwd = ".", timeout = { present = false, milliseconds = 0 } } },
    { id = "joined", args = { argv = "rg --files", cwd = ".", timeout = { present = false, milliseconds = 0 } } },
    { id = "empty", args = { argv = {}, cwd = ".", timeout = { present = false, milliseconds = 0 } } },
    { id = "hole", args = { argv = { [1] = "rg", [3] = "--files" }, cwd = ".", timeout = { present = false, milliseconds = 0 } } },
    { id = "cwd", args = { argv = { "rg", "--files" }, cwd = "", timeout = { present = false, milliseconds = 0 } } },
    { id = "timeout", args = { argv = { "rg", "--files" }, cwd = ".", timeout = { present = true, milliseconds = 0 } } },
  }
  for _, case in ipairs(cases) do
    validator._internals.reset()
    validator._internals.set_mode("auto")
    _test.calls_clear()
    validator.receive_msg(make_entry({
      kind = "chat.tool.permission_request",
      id = "process-" .. case.id,
      tool = "process.exec",
      allowlist = { "process.exec" },
      args = case.args,
    }))
    calls = decode_calls()
    assert_eq(calls[1].decision, "deny", case.id .. " process shape fails closed")
  end
end

-- Capability validation is fail-closed before every mode-specific policy,
-- including yolo. Missing allowlist is the legacy unrestricted shape.
do
  local validator = tv_lib.build { shell_classifier = CLASSIFIER, read_only_tools = { "read_file" } }
  local cases = {
    { id = "empty", mode = "safe", allowlist = {} },
    { id = "malformed", mode = "safe", allowlist = { "read_file", 7 } },
    { id = "excluded-yolo", mode = "yolo", allowlist = { "write_file" } },
    { id = "malformed-yolo", mode = "yolo", allowlist = { "read_file", false } },
  }
  for _, case in ipairs(cases) do
    validator._internals.reset()
    validator._internals.set_mode(case.mode)
    _test.calls_clear()
    validator.receive_msg(make_entry({
      kind = "chat.tool.permission_request",
      id = case.id,
      tool = "read_file",
      allowlist = case.allowlist,
      read_only = true,
      args = { path = "README.md" },
    }))
    local calls = decode_calls()
    assert_eq(#calls, 1, case.id .. " emits one envelope")
    assert_eq(calls[1].kind, "tool.permission_response", case.id .. " responds directly")
    assert_eq(calls[1].decision, "deny", case.id .. " denies")
  end
end

-- Legacy read_only is ignored; absence of an authoritative allowlist remains
-- unrestricted and follows normal policy rather than becoming read-only.
do
  local validator = tv_lib.build { shell_classifier = CLASSIFIER, read_only_tools = { "read_file" } }
  validator._internals.set_mode("safe")
  _test.calls_clear()
  validator.receive_msg(make_entry({
    kind = "chat.tool.permission_request",
    id = "perm-obsolete-flag",
    tool = "read_file",
    read_only = true,
    args = { path = "README.md" },
  }))
  local calls = decode_calls()
  assert_eq(calls[1].kind, "chat.tool.popup_request", "missing allowlist preserves unrestricted compatibility")
end

-- A missing or unusable managed classifier settles the pending gate request
-- with one actionable denial. It never silently approves or leaves the request
-- waiting for a popup that cannot be produced.
do
  local original_run = nefor.process.run
  nefor.process.run = function(_)
    return { code = -1, stderr = "spawn failed: no such file" }
  end
  local validator = tv_lib.build { shell_classifier = "/missing/plugins/da/bin/da" }
  validator._internals.set_mode("safe")
  _test.calls_clear()
  validator.receive_msg(make_entry({
    kind = "chat.tool.permission_request",
    id = "missing-classifier",
    tool = "shell.script",
    args = { script = "git status" },
  }))
  nefor.process.run = original_run
  local calls = decode_calls()
  assert_eq(#calls, 1, "classifier failure settles exactly once")
  assert_eq(calls[1].kind, "tool.permission_response", "classifier failure responds to gate")
  assert_eq(calls[1].decision, "deny", "classifier failure denies")
  assert_true(calls[1].reason:find("tool_classifier_unavailable[da]", 1, true) ~= nil,
    "classifier failure is actionable")
  assert_true(calls[1].reason:find("/missing/plugins/da/bin/da", 1, true) ~= nil,
    "classifier failure identifies configured path")
end

-- A classifier can pass its cached version probe and still fail on the actual
-- command. That later failure has the same single-settlement guarantee and
-- uses only the injected private path.
do
  local original_run = nefor.process.run
  local commands = {}
  nefor.process.run = function(opts)
    commands[#commands + 1] = opts.cmd
    if type(opts.args) == "table" and opts.args[1] == "--version" then
      return { code = 0, stdout = "da fixture" }
    end
    return { code = -1, stderr = "classifier process crashed" }
  end
  local validator = tv_lib.build { shell_classifier = CLASSIFIER }
  validator._internals.set_mode("safe")
  _test.calls_clear()
  validator.receive_msg(make_entry({
    kind = "chat.tool.permission_request",
    id = "classifier-crash",
    tool = "shell.script",
    args = { script = "git status" },
  }))
  nefor.process.run = original_run
  local calls = decode_calls()
  assert_eq(#calls, 1, "post-probe classifier failure settles exactly once")
  assert_eq(calls[1].kind, "tool.permission_response", "post-probe failure responds to gate")
  assert_eq(calls[1].decision, "deny", "post-probe failure denies")
  assert_true(calls[1].reason:find("classifier process crashed", 1, true) ~= nil,
    "post-probe failure preserves diagnostic")
  assert_eq(#commands, 2, "probe and classification are the only processes")
  assert_eq(commands[1], CLASSIFIER, "probe uses injected private path")
  assert_eq(commands[2], CLASSIFIER, "classification uses injected private path")
end

do
  local ok, err = pcall(tv_lib.build, {})
  assert_eq(ok, false, "classifier dependency is required")
  assert_true(tostring(err):find("shell_classifier", 1, true) ~= nil,
    "missing dependency names the composition seam")
end

print("tool_validator_mode_test: all assertions passed")
