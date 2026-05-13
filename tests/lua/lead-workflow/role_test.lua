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

-- AGENT_CONFIGS has the three v0.1 roles.
assert_true(type(lead_role.AGENT_CONFIGS) == "table", "AGENT_CONFIGS is a table")
for _, role in ipairs({ "explorer", "builder", "reviewer" }) do
  local cfg = lead_role.AGENT_CONFIGS[role]
  assert_true(type(cfg) == "table", "AGENT_CONFIGS." .. role .. " exists")
  assert_true(type(cfg.system_prompt) == "string", role .. ".system_prompt is a string")
  assert_true(#cfg.system_prompt > 0, role .. ".system_prompt is non-empty")
  assert_true(
    not cfg.system_prompt:find("^%[lead%-workflow%.role: prompt"),
    role .. ".system_prompt is the real prompt, not a placeholder"
  )
  assert_eq(cfg.model, nil, role .. ".model defaults to nil")
  assert_true(type(cfg.tool_allowlist) == "table", role .. ".tool_allowlist is a table")
  assert_true(#cfg.tool_allowlist > 0, role .. ".tool_allowlist is non-empty")
end

-- Builder gets write_file. Explorer and reviewer don't.
assert_true(
  contains(lead_role.AGENT_CONFIGS.builder.tool_allowlist, "write_file"),
  "builder allowlist contains write_file"
)
assert_true(
  contains(lead_role.AGENT_CONFIGS.builder.tool_allowlist, "edit"),
  "builder allowlist contains edit"
)
assert_true(
  not contains(lead_role.AGENT_CONFIGS.explorer.tool_allowlist, "write_file"),
  "explorer allowlist does NOT contain write_file"
)
assert_true(
  not contains(lead_role.AGENT_CONFIGS.reviewer.tool_allowlist, "write_file"),
  "reviewer allowlist does NOT contain write_file"
)
assert_true(
  not contains(lead_role.AGENT_CONFIGS.reviewer.tool_allowlist, "edit"),
  "reviewer allowlist does NOT contain edit"
)

-- Explorer and reviewer share the read-only tool set.
for _, tool in ipairs({ "read_file", "grep", "find", "ls", "glob", "bash" }) do
  assert_true(
    contains(lead_role.AGENT_CONFIGS.explorer.tool_allowlist, tool),
    "explorer allowlist contains " .. tool
  )
  assert_true(
    contains(lead_role.AGENT_CONFIGS.reviewer.tool_allowlist, tool),
    "reviewer allowlist contains " .. tool
  )
end

-- Lead's orchestration tools are minimal and don't include the
-- investigation/edit tools sub-agents have.
assert_true(type(lead_role.ORCHESTRATION_TOOLS) == "table", "ORCHESTRATION_TOOLS is a table")
for _, tool in ipairs({ "read_file", "dispatch-graph", "write-review", "await-approval" }) do
  assert_true(
    contains(lead_role.ORCHESTRATION_TOOLS, tool),
    "ORCHESTRATION_TOOLS contains " .. tool
  )
end
for _, tool in ipairs({ "grep", "find", "ls", "glob", "write_file", "edit", "bash" }) do
  assert_true(
    not contains(lead_role.ORCHESTRATION_TOOLS, tool),
    "ORCHESTRATION_TOOLS does NOT contain " .. tool
  )
end
