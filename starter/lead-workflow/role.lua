-- starter/lead-workflow/role.lua — team role/prompt metadata.
--
-- 0.4 execution is MAG-based: the lead writes agent graphs with the `mag`
-- tool instead of calling the old `dispatch-graph` tool. This module remains
-- the team-owned roster/prompt loader used by init.lua, tests, docs, and the
-- MAG role library.
--
-- Exposes:
--   * LEAD_SYSTEM_PROMPT  — string, the lead orchestrator's prompt
--                           (starter/prompts/lead.md).
--   * AGENT_CONFIGS       — table keyed by role name; each entry has
--                           { tool_allowlist, read_only }. Role prompt bodies
--                           live in mag/lib/prompts/<role>.md (the single
--                           source, read by the lead when it authors a MAG
--                           graph) — they are not duplicated here.
--   * ORCHESTRATION_TOOLS — list of direct lead tools authored in lead-turn.mag.
--   * TOOL_ALLOWLIST      — union used by config/tool-gate policy checks.
--
-- Sub-agents intentionally do not set per-role models; MAG profiles resolve
-- to the provider-level default configured at boot.

local M = {}

-- NEFOR_CONFIG_DIR is the global the engine sets to the directory
-- containing init.lua. For tests that load the module without booting
-- the engine, the test rig sets NEFOR_CONFIG_DIR explicitly.
local STARTER_ROOT = (rawget(_G, "NEFOR_CONFIG_DIR") or ".")
local PROMPTS_DIR  = STARTER_ROOT .. "/prompts"

local function read_prompt(name)
  local path = PROMPTS_DIR .. "/" .. name .. ".md"
  local f, err = io.open(path, "r")
  if not f then
    return nil, "lead-workflow.role: cannot open " .. path .. ": " .. tostring(err)
  end
  local content = f:read("*a")
  f:close()
  if not content or content == "" then
    return nil, "lead-workflow.role: empty prompt at " .. path
  end
  return content
end

-- A missing prompt file is a developer error, not a runtime condition.
-- Surface a placeholder so the module still loads, but make it
-- obviously broken if it ever reaches a model.
local function load_or_placeholder(name)
  local content, err = read_prompt(name)
  if content then return content end
  return "[lead-workflow.role: prompt '" .. name .. "' missing — " .. tostring(err) .. "]"
end

M.LEAD_SYSTEM_PROMPT = load_or_placeholder("lead")

-- Per-role boundaries:
--   * explorer        — read-only set (read_file + list_dir + search_text).
--   * reviewer/critic — read-only set. No shell, no write.
--   * worker          — general write-capable executor for approved work.
--   * docs            — specialized write-capable documentation agent with
--                       Jira/Confluence access via the dp/confluence skills
--                       (loaded with the `skill` tool, run through mag-eval).
M.AGENT_CONFIGS = {
  explorer = {
    -- MAG agents use read_file for known paths and mag-eval shell expressions
    -- for listing/searching. list_dir/search_text remain advertised for direct
    -- lead convenience and legacy tests, but role prompts steer agents to MAG.
    tool_allowlist = { "read_file", "mag-eval" },
    read_only      = true,
  },
  worker = {
    tool_allowlist = { "read_file", "edit_file", "write_file", "mag-eval" },
    read_only      = false,
  },
  reviewer = {
    tool_allowlist = { "read_file", "mag-eval" },
    read_only      = true,
  },
  docs = {
    tool_allowlist = { "skill", "read_file", "edit_file", "write_file", "mag-eval" },
    read_only      = false,
  },
  critic = {
    tool_allowlist = { "read_file", "mag-eval" },
    read_only      = true,
  },
}

-- The lead does NOT get broad shell directly. Quick world reads use mag-eval;
-- durable delegation uses mag; write-capable MAG programs are gated by
-- write-review before execute.
M.ORCHESTRATION_TOOLS = {
  "read_file",
  "skill",
  "mag-eval",
  "mag",
  "write-review",
  "graph-status",
  "terminate-graph",
}

-- TOOL_ALLOWLIST — union of every role's tool surface plus the lead's
-- orchestration tools. Fed into tool-gate's `--prompt <name>` argv.
-- Including orchestration names here is harmless: tool-gate is happy
-- to allowlist names that no Rust plugin advertises.
do
  local seen = {}
  local union = {}
  local function add(t)
    if not seen[t] then seen[t] = true; union[#union + 1] = t end
  end
  for _, role in pairs(M.AGENT_CONFIGS) do
    for _, t in ipairs(role.tool_allowlist) do add(t) end
  end
  for _, t in ipairs(M.ORCHESTRATION_TOOLS) do add(t) end
  M.TOOL_ALLOWLIST = union
end

return M
