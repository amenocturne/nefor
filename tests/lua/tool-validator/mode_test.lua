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

-- yolo: defensive approve if a prompt-mode request reaches the validator.
do
  fresh("yolo")
  feed({ kind = "chat.tool.permission_request", id = "perm-yolo", tool = "bash", args = { command = "maybe" } })
  local calls = decode_calls()
  assert_eq(#calls, 1, "yolo emits one envelope")
  assert_eq(calls[1].kind, "tool.permission_response", "yolo approves")
  assert_eq(calls[1].decision, "approve", "yolo decision is approve")
end

print("tool_validator_mode_test: all assertions passed")
