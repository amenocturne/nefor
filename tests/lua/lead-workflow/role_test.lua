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

local function contains(list, target)
  for _, v in ipairs(list) do
    if v == target then return true end
  end
  return false
end

-- Module loads without error.
local lead_role = require("lead-workflow.role")

-- LEAD_SYSTEM_PROMPT is a non-empty string with the expected role-cue.
assert_true(type(lead_role.LEAD_SYSTEM_PROMPT) == "string", "LEAD_SYSTEM_PROMPT is a string")
assert_true(#lead_role.LEAD_SYSTEM_PROMPT > 0, "LEAD_SYSTEM_PROMPT is non-empty")
assert_true(
  not lead_role.LEAD_SYSTEM_PROMPT:find("^%[lead%-workflow%.role: prompt"),
  "LEAD_SYSTEM_PROMPT is the real prompt, not a missing-file placeholder"
)

-- Lead's orchestration tools are minimal and don't include the
-- shell/new-file tools that MAG agents use.
assert_true(type(lead_role.ORCHESTRATION_TOOLS) == "table", "ORCHESTRATION_TOOLS is a table")
for _, tool in ipairs({
  "read_file",
  "read_image",
  "list_dir",
  "search_text",
  "instructions",
  "edit_file",
  "mag-env",
  "mag",
  "write-review",
  "graph-status",
  "terminate-graph",
}) do
  assert_true(
    contains(lead_role.ORCHESTRATION_TOOLS, tool),
    "ORCHESTRATION_TOOLS contains " .. tool
  )
end
-- await-approval was removed; the verdict now arrives as a tagged user
-- message instead of via a blocking tool. The lead must NOT advertise
-- a no-op stub of the old name.
assert_true(
  not contains(lead_role.ORCHESTRATION_TOOLS, "await-approval"),
  "ORCHESTRATION_TOOLS must NOT contain await-approval (replaced by tagged user-message verdict feedback)"
)
assert_true(
  not contains(lead_role.ORCHESTRATION_TOOLS, "dispatch-graph"),
  "ORCHESTRATION_TOOLS must NOT contain dispatch-graph (replaced by MAG)"
)
for _, tool in ipairs({ "write_file", "bash" }) do
  assert_true(
    not contains(lead_role.ORCHESTRATION_TOOLS, tool),
    "ORCHESTRATION_TOOLS does NOT contain " .. tool
  )
end
