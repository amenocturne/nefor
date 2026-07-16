-- starter/lead_role_test.lua — smoke tests for the lead-workflow role
-- loader. Driven from
-- `crates/nefor/tests/starter_lead_role_test.rs`.
--
-- The loader has no bus dependency — these tests just exercise that
-- prompts get read off disk and the exported tables are shaped right.

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ",
      tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

-- Module loads without error.
local lead_role = require("libs.lead-workflow.role")

-- BASE_SYSTEM_PROMPT is real shared content and does not claim the root role.
assert_true(type(lead_role.BASE_SYSTEM_PROMPT) == "string", "BASE_SYSTEM_PROMPT is a string")
assert_true(#lead_role.BASE_SYSTEM_PROMPT > 0, "BASE_SYSTEM_PROMPT is non-empty")
assert_true(
  not lead_role.BASE_SYSTEM_PROMPT:find("lead orchestrator", 1, true),
  "BASE_SYSTEM_PROMPT does not claim the root role"
)

-- LEAD_SYSTEM_PROMPT composes the base with the root-only role cue.
assert_true(type(lead_role.LEAD_SYSTEM_PROMPT) == "string", "LEAD_SYSTEM_PROMPT is a string")
assert_true(#lead_role.LEAD_SYSTEM_PROMPT > 0, "LEAD_SYSTEM_PROMPT is non-empty")
assert_true(
  not lead_role.LEAD_SYSTEM_PROMPT:find("^%[lead%-workflow%.role: prompt"),
  "LEAD_SYSTEM_PROMPT is the real prompt, not a missing-file placeholder"
)
assert_true(
  lead_role.LEAD_SYSTEM_PROMPT:find(lead_role.BASE_SYSTEM_PROMPT, 1, true) == 1,
  "LEAD_SYSTEM_PROMPT begins with the shared base"
)
assert_true(
  lead_role.LEAD_SYSTEM_PROMPT:find("lead orchestrator", 1, true) ~= nil,
  "LEAD_SYSTEM_PROMPT includes the root lead overlay"
)
