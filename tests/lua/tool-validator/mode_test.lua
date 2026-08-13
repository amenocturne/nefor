local tv = require("tool-validator")
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

-- yolo still bypasses approval policy after capability membership succeeds.
do
  fresh("yolo")
  feed({
    kind = "chat.tool.permission_request",
    id = "perm-yolo-edit-capable",
    tool = "edit_file",
    allowlist = { "read_file", "edit_file" },
    args = { path = "some/file.lua", old_string = "a", new_string = "b" },
  })
  local calls = decode_calls()
  assert_eq(#calls, 1, "yolo capable edit_file emits one envelope")
  assert_eq(calls[1].kind, "tool.permission_response", "yolo capable edit_file approves")
  assert_eq(calls[1].decision, "approve", "yolo capable edit_file decision is approve")
  assert_eq(calls[1].reason, nil, "yolo edit_file approval has no denial reason")
end

-- yolo: write_file also bypasses the approved-plan denial path.
do
  fresh("yolo")
  feed({
    kind = "chat.tool.permission_request",
    id = "perm-yolo-write-no-plan",
    tool = "write_file",
    args = { path = "some/file.lua", content = "return true\n" },
  })
  local calls = decode_calls()
  assert_eq(#calls, 1, "yolo write_file no-plan emits one envelope")
  assert_eq(calls[1].kind, "tool.permission_response", "yolo write_file no-plan approves")
  assert_eq(calls[1].decision, "approve", "yolo write_file no-plan decision is approve")
end

-- auto: direct edit/write is autonomous.
do
  fresh("auto")
  feed({
    kind = "chat.tool.permission_request",
    id = "perm-auto-edit",
    tool = "edit_file",
    args = { path = "some/file.lua", old_string = "a", new_string = "b" },
  })
  local calls = decode_calls()
  assert_eq(#calls, 1, "auto edit_file emits one envelope")
  assert_eq(calls[1].kind, "tool.permission_response", "auto edit_file approves")
  assert_eq(calls[1].decision, "approve", "auto edit_file decision is approve")
  assert_eq(calls[1].args, nil, "auto edit_file approval has no policy args")
end

-- Read-only status is derived from the complete allowlist, not a wire flag.
do
  local validator = tv_lib.build { read_only_tools = { "read_file", "read_image" } }
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
  local validator = tv_lib.build { read_only_tools = { "read_file", "read_image" } }
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
  local validator = tv_lib.build { read_only_tools = { "read_file" } }
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
  local validator = tv_lib.build { read_only_tools = { "read_file" } }
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

print("tool_validator_mode_test: all assertions passed")
