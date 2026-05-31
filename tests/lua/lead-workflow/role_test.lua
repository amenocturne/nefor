-- tests/lua/lead-workflow/role_test.lua — smoke tests for the
-- team-port lead-workflow role loader.

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

local function contains(list, target)
  for _, v in ipairs(list) do
    if v == target then return true end
  end
  return false
end

local ADVERTISED_TOOLS = {
  "read_file", "write_file", "bash",
  "list_dir", "search_text",
  "jira", "wiki",
  "finalize",
}

local function is_advertised(name)
  return contains(ADVERTISED_TOOLS, name)
end

local lead_role = require("lead-workflow.role")

assert_true(type(lead_role.LEAD_SYSTEM_PROMPT) == "string",
  "LEAD_SYSTEM_PROMPT is a string")
assert_true(#lead_role.LEAD_SYSTEM_PROMPT > 0,
  "LEAD_SYSTEM_PROMPT is non-empty")
assert_true(
  not lead_role.LEAD_SYSTEM_PROMPT:find("^%[lead%-workflow%.role:"),
  "LEAD_SYSTEM_PROMPT is the real prompt, not a missing-file placeholder"
)

assert_true(type(lead_role.AGENT_CONFIGS) == "table", "AGENT_CONFIGS is a table")
local team_roles = { "explorer", "worker", "reviewer", "docs", "critic" }
for _, role in ipairs(team_roles) do
  local cfg = lead_role.AGENT_CONFIGS[role]
  assert_true(type(cfg) == "table", "AGENT_CONFIGS." .. role .. " exists")
  assert_true(type(cfg.system_prompt) == "string", role .. ".system_prompt is a string")
  assert_true(#cfg.system_prompt > 0, role .. ".system_prompt is non-empty")
  assert_true(
    not cfg.system_prompt:find("^%[lead%-workflow%.role:"),
    role .. ".system_prompt is the real prompt, not a placeholder"
  )
  if cfg.model ~= nil then
    assert_true(type(cfg.model) == "string", role .. ".model is a string")
    assert_true(#cfg.model > 0, role .. ".model is non-empty when set")
  end
  assert_true(type(cfg.tool_allowlist) == "table", role .. ".tool_allowlist is a table")
  assert_true(#cfg.tool_allowlist > 0, role .. ".tool_allowlist is non-empty")
end

for _, removed in ipairs({ "builder", "tester", "reflector", "prompt-engineer" }) do
  assert_true(lead_role.AGENT_CONFIGS[removed] == nil,
    "removed role " .. removed .. " is not registered")
end

for _, role in ipairs(team_roles) do
  local cfg = lead_role.AGENT_CONFIGS[role]
  for _, name in ipairs(cfg.tool_allowlist) do
    assert_true(is_advertised(name),
      role .. " allowlist references unadvertised tool '" .. name .. "'")
  end
end

local READ_ONLY_SET = { "read_file", "list_dir", "search_text" }
for _, role in ipairs({ "explorer", "reviewer", "critic" }) do
  assert_true(lead_role.AGENT_CONFIGS[role].read_only == true,
    role .. " is marked read-only")
  for _, tool in ipairs(READ_ONLY_SET) do
    assert_true(contains(lead_role.AGENT_CONFIGS[role].tool_allowlist, tool),
      role .. " allowlist contains " .. tool)
  end
  for _, tool in ipairs({ "write_file", "bash", "jira", "wiki" }) do
    assert_true(not contains(lead_role.AGENT_CONFIGS[role].tool_allowlist, tool),
      role .. " allowlist does NOT contain " .. tool)
  end
end

assert_true(lead_role.AGENT_CONFIGS.worker.read_only == false,
  "worker is write-capable")
for _, tool in ipairs({ "read_file", "list_dir", "search_text", "write_file", "bash" }) do
  assert_true(contains(lead_role.AGENT_CONFIGS.worker.tool_allowlist, tool),
    "worker allowlist contains " .. tool)
end

assert_true(lead_role.AGENT_CONFIGS.docs.read_only == false,
  "docs is write-capable")
for _, tool in ipairs({ "jira", "wiki", "read_file", "list_dir", "search_text", "write_file" }) do
  assert_true(contains(lead_role.AGENT_CONFIGS.docs.tool_allowlist, tool),
    "docs allowlist contains " .. tool)
end

assert_true(type(lead_role.ORCHESTRATION_TOOLS) == "table", "ORCHESTRATION_TOOLS is a table")
for _, tool in ipairs({
  "read_file", "dispatch-graph", "write-review",
}) do
  assert_true(contains(lead_role.ORCHESTRATION_TOOLS, tool),
    "ORCHESTRATION_TOOLS contains " .. tool)
end
for _, tool in ipairs({ "write_file", "bash", "progress", "critique", "terminate" }) do
  assert_true(not contains(lead_role.ORCHESTRATION_TOOLS, tool),
    "ORCHESTRATION_TOOLS does NOT contain " .. tool)
end

assert_true(type(lead_role.TOOL_ALLOWLIST) == "table", "TOOL_ALLOWLIST is a table")
for _, tool in ipairs({
  "read_file", "write_file", "bash", "jira", "wiki",
  "dispatch-graph", "write-review",
}) do
  assert_true(contains(lead_role.TOOL_ALLOWLIST, tool),
    "TOOL_ALLOWLIST union contains " .. tool)
end

local prompts_root = (rawget(_G, "NEFOR_CONFIG_DIR") or ".") .. "/prompts"
local prompt_files = { "lead", "explorer", "worker", "reviewer", "docs", "critic" }
local removed_prompt_files = { "builder", "tester", "reflector", "prompt-engineer" }

for _, name in ipairs(prompt_files) do
  local fh = io.open(prompts_root .. "/" .. name .. ".md", "r")
  assert_true(fh ~= nil, "prompt " .. name .. ".md exists")
  if fh then fh:close() end
end
for _, name in ipairs(removed_prompt_files) do
  local fh = io.open(prompts_root .. "/" .. name .. ".md", "r")
  assert_true(fh == nil, "removed prompt " .. name .. ".md does not exist")
  if fh then fh:close() end
end
