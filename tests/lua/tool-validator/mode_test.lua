local tv = require("tool-validator")
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

-- safe: a deferred bash classification opens a popup and does not deny.
do
  fresh("safe")
  feed({ kind = "chat.tool.permission_request", id = "perm-safe", tool = "bash", args = { command = "maybe" } })
  local calls = decode_calls()
  assert_eq(#calls, 1, "safe defer emits one envelope")
  assert_eq(calls[1].kind, "chat.tool.popup_request", "safe defer opens popup")
  assert_eq(calls[1].id, "perm-safe", "popup keeps id")
end

-- safe: even a forbidden bash classification opens a popup. Safe mode
-- means interactive governance, not hard runtime denial.
do
  fresh("safe")
  feed({ kind = "chat.tool.permission_request", id = "perm-safe-forbidden", tool = "bash", args = { command = "forbidden rm" } })
  local calls = decode_calls()
  assert_eq(#calls, 1, "safe forbidden emits one envelope")
  assert_eq(calls[1].kind, "chat.tool.popup_request", "safe forbidden opens popup")
  assert_eq(calls[1].id, "perm-safe-forbidden", "popup keeps forbidden id")
end

-- auto: the same deferred request is denied with recovery text and no popup.
do
  fresh("auto")
  feed({ kind = "chat.tool.permission_request", id = "perm-auto", tool = "bash", args = { command = "maybe" } })
  local calls = decode_calls()
  assert_eq(#calls, 1, "auto defer emits one envelope")
  assert_eq(calls[1].kind, "tool.permission_response", "auto defer denies")
  assert_eq(calls[1].decision, "deny", "auto decision is deny")
  assert_true(type(calls[1].reason) == "string" and calls[1].reason:find("permission_denied[auto]", 1, true) ~= nil,
    "auto denial includes recovery marker")
end

-- auto: forbidden bash stays denied because auto has no human in the loop.
do
  fresh("auto")
  feed({ kind = "chat.tool.permission_request", id = "perm-auto-forbidden", tool = "bash", args = { command = "forbidden rm" } })
  local calls = decode_calls()
  assert_eq(#calls, 1, "auto forbidden emits one envelope")
  assert_eq(calls[1].kind, "tool.permission_response", "auto forbidden denies")
  assert_eq(calls[1].decision, "deny", "auto forbidden decision is deny")
end

-- yolo: defensive approve if a prompt-mode request reaches the validator.
do
  fresh("yolo")
  feed({ kind = "chat.tool.permission_request", id = "perm-yolo", tool = "bash", args = { command = "maybe" } })
  local calls = decode_calls()
  assert_eq(#calls, 1, "yolo emits one envelope")
  assert_eq(calls[1].kind, "tool.permission_response", "yolo approves")
  assert_eq(calls[1].decision, "approve", "yolo decision is approve")
end

-- yolo: full-approve happens before read-only and approved-plan gates.
do
  fresh("yolo")
  feed({
    kind = "chat.tool.permission_request",
    id = "perm-yolo-edit-readonly",
    tool = "edit_file",
    read_only = true,
    args = { path = "some/file.lua", old_string = "a", new_string = "b" },
  })
  local calls = decode_calls()
  assert_eq(#calls, 1, "yolo edit_file read-only emits one envelope")
  assert_eq(calls[1].kind, "tool.permission_response", "yolo edit_file read-only approves")
  assert_eq(calls[1].decision, "approve", "yolo edit_file read-only decision is approve")
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

print("tool_validator_mode_test: all assertions passed")
