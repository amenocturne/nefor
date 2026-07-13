local startup = require("startup")

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error("assertion failed: " .. (msg or "values differ")
      .. " expected=" .. tostring(expected) .. " actual=" .. tostring(actual), 2)
  end
end

local function assert_error(argv, expected)
  local ok, err = pcall(startup.parse, argv)
  assert_eq(ok, false, "invalid arguments fail")
  if not tostring(err):find(expected, 1, true) then
    error("expected error containing " .. expected .. ", got: " .. tostring(err), 2)
  end
end

do
  local opts = startup.parse({ "--prompt", "hello", "--mode", "auto", "--session", "s1" })
  assert_eq(opts.prompt, "hello", "prompt parses in arbitrary order")
  assert_eq(opts.session_id, "s1", "session parses in arbitrary order")
  assert_eq(opts.mode, "auto", "mode parses")
end

do
  local opts = startup.parse({ "--mode", "safe", "--yolo", "--mode", "auto" })
  assert_eq(opts.mode, "auto", "last mode control wins")
  opts = startup.parse({ "--mode", "auto", "--yolo" })
  assert_eq(opts.mode, "yolo", "alias wins when it appears last")
end

do
  local modes = {}
  startup.apply_mode(startup.parse({ "--yolo", "--session", "s1" }), {
    set_mode = function(mode) modes[#modes + 1] = mode end,
  })
  assert_eq(#modes, 1, "explicit mode is applied once")
  assert_eq(modes[1], "yolo", "alias applies yolo through set_mode")
end

do
  local calls = 0
  startup.apply_mode(startup.parse({ "--session", "s1" }), {
    set_mode = function(_) calls = calls + 1 end,
  })
  assert_eq(calls, 0, "session resume does not restore a mode")
end

assert_error({ "--mode" }, "--mode requires one of")
assert_error({ "--mode", "--prompt", "hello" }, "--mode requires one of")
assert_error({ "--mode", "reckless" }, "invalid startup mode: reckless")
assert_error({ "--wat" }, "unknown startup arg: --wat")
assert_error({ "--session" }, "--session requires a session id")
assert_error({ "--prompt" }, "--prompt requires a prompt")

print("starter_startup_test: all assertions passed")
