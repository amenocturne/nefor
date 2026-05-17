-- tests/lua/lead-workflow/role_test.lua — smoke tests for the
-- team-port lead-workflow role loader. Forked from upstream's matching
-- test file because the team port carries a different role roster
-- (lead + 7 sub-agents vs upstream's lead + 3) and stricter per-role
-- tool boundaries (e.g. read-only review roles get only read_file).
--
-- The loader has no bus dependency — these tests just exercise that
-- prompts get read off disk and the exported tables are shaped right.

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

local function contains(list, target)
  for _, v in ipairs(list) do
    if v == target then return true end
  end
  return false
end

-- The set of tool names actually advertised on the wire. basic-tools
-- ships read_file, write_file, bash. read-only-tools (Lua-resident)
-- adds list_dir + search_text — read-only investigation primitives
-- that replaced the previous explorer-with-bash shape. `finalize` is
-- the synthetic terminator the agent reasoner appends. Anything
-- outside this list cannot be called at runtime; an allowlist that
-- names it is dead config.
local ADVERTISED_TOOLS = {
  "read_file", "write_file", "bash",
  "list_dir", "search_text",
  "finalize",
}

local function is_advertised(name)
  return contains(ADVERTISED_TOOLS, name)
end

local lead_role = require("lead-workflow.role")

-- LEAD_SYSTEM_PROMPT is a non-empty real prompt, not a missing-file
-- placeholder.
assert_true(type(lead_role.LEAD_SYSTEM_PROMPT) == "string",
  "LEAD_SYSTEM_PROMPT is a string")
assert_true(#lead_role.LEAD_SYSTEM_PROMPT > 0,
  "LEAD_SYSTEM_PROMPT is non-empty")
assert_true(
  not lead_role.LEAD_SYSTEM_PROMPT:find("^%[lead-workflow%.role:"),
  "LEAD_SYSTEM_PROMPT is the real prompt, not a missing-file placeholder"
)

-- AGENT_CONFIGS has the full team-port role roster: lead + 7 sub-agents.
assert_true(type(lead_role.AGENT_CONFIGS) == "table", "AGENT_CONFIGS is a table")
local team_roles = {
  "explorer", "builder", "reviewer",
  "tester", "critic", "reflector", "prompt-engineer",
}
for _, role in ipairs(team_roles) do
  local cfg = lead_role.AGENT_CONFIGS[role]
  assert_true(type(cfg) == "table", "AGENT_CONFIGS." .. role .. " exists")
  assert_true(type(cfg.system_prompt) == "string", role .. ".system_prompt is a string")
  assert_true(#cfg.system_prompt > 0, role .. ".system_prompt is non-empty")
  assert_true(
    not cfg.system_prompt:find("^%[lead-workflow%.role:"),
    role .. ".system_prompt is the real prompt, not a placeholder"
  )
  -- Per-role model: with config.lua pinning per-variant role_models
  -- (prod → Nestor, test → ollama, mock → empty), the field is non-nil
  -- in prod/test and nil only in mock.
  if cfg.model ~= nil then
    assert_true(type(cfg.model) == "string", role .. ".model is a string")
    assert_true(#cfg.model > 0, role .. ".model is non-empty when set")
  end
  assert_true(type(cfg.tool_allowlist) == "table", role .. ".tool_allowlist is a table")
  assert_true(#cfg.tool_allowlist > 0, role .. ".tool_allowlist is non-empty")
end

-- Every name in every role's allowlist must be a tool actually
-- advertised on the wire. Without this, an allowlist that mentions e.g.
-- `grep` or `edit` looks correct but breaks at runtime when the agent
-- tries to call a tool the gate doesn't know.
for _, role in ipairs(team_roles) do
  local cfg = lead_role.AGENT_CONFIGS[role]
  for _, name in ipairs(cfg.tool_allowlist) do
    assert_true(is_advertised(name),
      role .. " allowlist references tool '" .. name
      .. "' which is NOT advertised by basic-tools (advertised: "
      .. table.concat(ADVERTISED_TOOLS, ", ") .. ")")
  end
end

-- The read-only investigation set: read_file + list_dir + search_text.
-- Every role gets this baseline; the differentiators below are which
-- mutation tools each role layers on top.
local READ_ONLY_SET = { "read_file", "list_dir", "search_text" }

-- Read-only roles (explorer, reviewer, critic, reflector) get only the
-- read-only set. No shell, no write. bash is a sandbox-escape hatch
-- via shell composition, so "read-only role with bash" is a
-- contradiction.
local read_only_roles = { "explorer", "reviewer", "critic", "reflector" }
for _, role in ipairs(read_only_roles) do
  for _, tool in ipairs(READ_ONLY_SET) do
    assert_true(
      contains(lead_role.AGENT_CONFIGS[role].tool_allowlist, tool),
      role .. " allowlist contains " .. tool
    )
  end
  for _, tool in ipairs({ "write_file", "bash" }) do
    assert_true(
      not contains(lead_role.AGENT_CONFIGS[role].tool_allowlist, tool),
      role .. " allowlist does NOT contain " .. tool
    )
  end
end

-- Builder is the only role with the read-only set + write_file + bash.
for _, tool in ipairs({ "read_file", "list_dir", "search_text", "write_file", "bash" }) do
  assert_true(
    contains(lead_role.AGENT_CONFIGS.builder.tool_allowlist, tool),
    "builder allowlist contains " .. tool
  )
end

-- Tester gets the read-only set + bash for running the test command;
-- no write_file.
for _, tool in ipairs({ "read_file", "list_dir", "search_text", "bash" }) do
  assert_true(
    contains(lead_role.AGENT_CONFIGS.tester.tool_allowlist, tool),
    "tester allowlist contains " .. tool
  )
end
assert_true(
  not contains(lead_role.AGENT_CONFIGS.tester.tool_allowlist, "write_file"),
  "tester allowlist does NOT contain write_file"
)

-- prompt-engineer writes prompt files; gets the read-only set +
-- write_file. No bash.
local pe = lead_role.AGENT_CONFIGS["prompt-engineer"]
for _, tool in ipairs({ "read_file", "list_dir", "search_text", "write_file" }) do
  assert_true(contains(pe.tool_allowlist, tool),
    "prompt-engineer allowlist contains " .. tool)
end
assert_true(
  not contains(pe.tool_allowlist, "bash"),
  "prompt-engineer allowlist does NOT contain bash"
)

-- Lead's orchestration tools are minimal and don't include any of the
-- investigation/edit tools sub-agents have.
assert_true(type(lead_role.ORCHESTRATION_TOOLS) == "table", "ORCHESTRATION_TOOLS is a table")
for _, tool in ipairs({
  "read_file", "dispatch-graph", "write-review", "await-approval",
  "progress", "critique", "terminate",
}) do
  assert_true(
    contains(lead_role.ORCHESTRATION_TOOLS, tool),
    "ORCHESTRATION_TOOLS contains " .. tool
  )
end
for _, tool in ipairs({ "write_file", "bash" }) do
  assert_true(
    not contains(lead_role.ORCHESTRATION_TOOLS, tool),
    "ORCHESTRATION_TOOLS does NOT contain " .. tool
  )
end

-- TOOL_ALLOWLIST: union of every role's tool surface plus the lead's
-- orchestration tools. Consumed by init.lua's tool-gate argv builder.
assert_true(type(lead_role.TOOL_ALLOWLIST) == "table",
  "TOOL_ALLOWLIST is a table")
for _, tool in ipairs({
  "read_file", "write_file", "bash",
  "dispatch-graph", "write-review", "terminate",
}) do
  assert_true(
    contains(lead_role.TOOL_ALLOWLIST, tool),
    "TOOL_ALLOWLIST union contains " .. tool
  )
end

-- Soft check: any backticked lowercase identifier in a prompt that
-- matches a known wire tool name MUST be advertised. Tokens that look
-- like shell utilities are allowed (reached via bash). Unknown tokens
-- are skipped to avoid false positives on prose / code paths.
local KNOWN_WIRE_TOOLS = {
  -- Once-spec'd names that aren't wired by basic-tools. A prompt
  -- backtick-referencing one of these is almost certainly stale.
  read = true, write = true, edit = true,
  grep_tool = true, find_tool = true, ls_tool = true, glob = true,
}

local prompts_root = (rawget(_G, "NEFOR_CONFIG_DIR") or ".") .. "/prompts"
local prompt_files = {
  "lead", "explorer", "builder", "reviewer",
  "tester", "critic", "reflector", "prompt-engineer",
}

local function scan_backticks(text)
  local found = {}
  for token in text:gmatch("`([a-z_][a-z_]*)`") do
    found[#found + 1] = token
  end
  return found
end

for _, name in ipairs(prompt_files) do
  local path = prompts_root .. "/" .. name .. ".md"
  local fh = io.open(path, "r")
  if fh then
    local body = fh:read("*a"); fh:close()
    for _, token in ipairs(scan_backticks(body)) do
      if KNOWN_WIRE_TOOLS[token] and not is_advertised(token) then
        -- lead.md explicitly states what tools the lead does NOT have
        -- ("You have NO `write`, `grep`, ..."), which is correct
        -- documentation, not a stale instruction. Skip negation
        -- clauses.
        local clause_start = body:find("`" .. token .. "`", 1, true)
        local window = body:sub(math.max(1, clause_start - 30),
                                clause_start + #token + 4)
        local is_negation = window:lower():find("no ", 1, true)
                         or window:lower():find("not ", 1, true)
                         or window:lower():find("does n", 1, true)
        if not is_negation then
          error(string.format(
            "prompt %s.md references backticked tool name `%s` which is "
            .. "not advertised by basic-tools (advertised: %s). If this "
            .. "is meant as a shell utility reached via `bash`, extend "
            .. "the scan in this test.",
            name, token, table.concat(ADVERTISED_TOOLS, ", ")))
        end
      end
    end
  end
end
